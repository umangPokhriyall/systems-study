//! The device side of the virtqueue: the half that lives in the VMM, outside the guest.
//!
//! Its job in one sentence: read chains the driver made available, do what they ask, write results
//! into the used ring, and notify only when it has to.
//!
//! Everything it reads is written by the thing it is isolating. `device.rs` in a real VMM is the
//! attack surface, and the shape of the code below - a walk with three independent bounds on it, a
//! direction check on every buffer, and errors that terminate a chain rather than being ignored -
//! is what that fact looks like in practice.

use std::fmt;
use std::num::Wrapping;

use crate::layout::{Descriptor, VirtqLayout, need_event, DESC_SIZE, VIRTQ_USED_F_NO_NOTIFY};
use crate::mem::{GuestAddr, MemError, SharedMem};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainError {
    /// A `next` pointer named a descriptor outside the table.
    IndexOutOfRange { index: u16, queue_size: u16 },
    /// The chain visited more descriptors than the table holds, so it contains a cycle.
    TooLong { limit: u16 },
    /// The chain's buffers total more than 2^32 bytes (VIRTIO 1.2, 2.7.5.2).
    TooManyBytes,
    /// A descriptor's buffer is not inside the shared region.
    BadBuffer(MemError),
    /// Indirect descriptors are not implemented here.
    Indirect,
    /// A device-readable descriptor appeared after a device-writable one.
    ReadableAfterWritable,
    /// Reading the descriptor itself failed.
    Mem(MemError),
}

impl fmt::Display for ChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChainError::IndexOutOfRange { index, queue_size } => {
                write!(f, "descriptor index {index} is outside a table of {queue_size}")
            }
            ChainError::TooLong { limit } => {
                write!(f, "chain longer than {limit} descriptors: it contains a cycle")
            }
            ChainError::TooManyBytes => write!(f, "chain describes more than 2^32 bytes"),
            ChainError::BadBuffer(e) => write!(f, "descriptor buffer is not in the region: {e}"),
            ChainError::Indirect => write!(f, "indirect descriptors are not implemented"),
            ChainError::ReadableAfterWritable => {
                write!(f, "device-readable descriptor after a device-writable one")
            }
            ChainError::Mem(e) => write!(f, "reading the descriptor table failed: {e}"),
        }
    }
}

impl std::error::Error for ChainError {}

/// Walks one descriptor chain, starting from its head index.
///
/// # Why this is the security-critical code in any VMM
///
/// The chain is a **linked list whose nodes and pointers are entirely written by the guest**. The
/// host must traverse it without trusting a single field. Three independent bounds exist because
/// they fail in different ways and no one of them subsumes the others:
///
/// 1. **`ttl`**, initialised to `queue_size` and decremented per descriptor. Bounds the *length* of
///    the walk. Without it, `desc[0].next = 0` is an infinite loop in the host with the guest
///    supplying nothing but two writes - a denial of service that costs the attacker nothing.
///
/// 2. **`next_index < queue_size`**. Bounds *where* the walk may go. Without it, a `next` of 60,000
///    reads 16 bytes from far past the descriptor table, which in a real VMM is other guest memory,
///    or another queue's rings.
///
/// 3. **`yielded_bytes` staying under 2^32** (VIRTIO 1.2, 2.7.5.2). Bounds the *work*. A chain of
///    128 descriptors each claiming 4 GiB is short, in-range, and asks the device to process half a
///    terabyte.
///
/// A fourth check is not in the spec but belongs in any real implementation: every descriptor's
/// `addr..addr+len` must lie inside the shared region, checked with the overflow-safe arithmetic in
/// `mem.rs`. This iterator does that on `read_buffer`/`write_buffer` rather than during the walk,
/// because a chain may legitimately be inspected before its buffers are touched.
///
/// # How this differs from `virtio-queue`
///
/// Upstream's `DescriptorChain` is an `Iterator<Item = Descriptor>` that ends the iteration on any
/// error, via `.ok()?`. That is safe and it is what the callers want. It also makes a malformed
/// chain indistinguishable from a short one, so a device model cannot report that a guest sent
/// something illegal. This iterator yields `Result` instead, so the demo can name what went wrong -
/// which is the point of a study implementation, and arguably a real gap upstream.
pub struct ChainIter<'a> {
    mem: &'a SharedMem,
    layout: VirtqLayout,
    next_index: u16,
    ttl: u16,
    yielded_bytes: u32,
    /// Once a device-writable descriptor is seen, every later one must also be writable. The spec
    /// requires the driver to order them that way (VIRTIO 1.2, 2.7.5.3), and a device that does not
    /// check will happily treat a buffer the driver is still reading as a place to write output.
    seen_writable: bool,
    done: bool,
}

impl<'a> ChainIter<'a> {
    pub fn new(mem: &'a SharedMem, layout: VirtqLayout, head: u16) -> Self {
        ChainIter {
            mem,
            layout,
            next_index: head,
            // The bound is the queue size: a chain cannot legitimately be longer than the table it
            // is built from, because that would require visiting some descriptor twice.
            ttl: layout.queue_size,
            yielded_bytes: 0,
            seen_writable: false,
            done: false,
        }
    }
}

impl Iterator for ChainIter<'_> {
    type Item = Result<Descriptor, ChainError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        // Bound 1: length. A cycle in the guest-written `next` pointers ends here.
        if self.ttl == 0 {
            self.done = true;
            return Some(Err(ChainError::TooLong { limit: self.layout.queue_size }));
        }
        // Bound 2: destination.
        let Some(addr) = self.layout.desc(self.next_index) else {
            self.done = true;
            return Some(Err(ChainError::IndexOutOfRange {
                index: self.next_index,
                queue_size: self.layout.queue_size,
            }));
        };

        let desc = match read_descriptor(self.mem, addr) {
            Ok(d) => d,
            Err(e) => {
                self.done = true;
                return Some(Err(ChainError::Mem(e)));
            }
        };

        if desc.is_indirect() {
            self.done = true;
            return Some(Err(ChainError::Indirect));
        }

        // Bound 3: total work. `checked_add` rather than a comparison, because the sum is what
        // overflows and a wrapped sum compares fine against any limit.
        match self.yielded_bytes.checked_add(desc.len) {
            Some(n) => self.yielded_bytes = n,
            None => {
                self.done = true;
                return Some(Err(ChainError::TooManyBytes));
            }
        }

        // Direction ordering.
        if desc.is_write_only() {
            self.seen_writable = true;
        } else if self.seen_writable {
            self.done = true;
            return Some(Err(ChainError::ReadableAfterWritable));
        }

        if desc.has_next() {
            self.next_index = desc.next;
            self.ttl -= 1;
        } else {
            self.done = true;
        }
        Some(Ok(desc))
    }
}

fn read_descriptor(mem: &SharedMem, addr: GuestAddr) -> Result<Descriptor, MemError> {
    debug_assert_eq!(DESC_SIZE, 16);
    Ok(Descriptor {
        addr: mem.read_u64(addr)?,
        len: mem.read_u32(GuestAddr(addr.0 + 8))?,
        flags: mem.read_u16(GuestAddr(addr.0 + 12))?,
        next: mem.read_u16(GuestAddr(addr.0 + 14))?,
    })
}

/// The device half of one virtqueue.
///
/// It holds only *indices*, never a borrow of memory: every method takes `&SharedMem` or
/// `&mut SharedMem`. That is not a convenience, it is the same shape `virtio-queue`'s `Queue` has,
/// and for the same reason - the memory is shared with something else, so a device may not hold a
/// long-lived reference into it.
pub struct Device {
    pub layout: VirtqLayout,
    /// The next entry of the avail ring this device has not yet consumed. Free-running.
    next_avail: Wrapping<u16>,
    /// The next slot of the used ring this device will write. Free-running.
    next_used: Wrapping<u16>,
    /// Completions added since the last `needs_notification` call. Needed because a device may add
    /// several before deciding whether to interrupt, and the decision has to consider all of them.
    num_added: Wrapping<u16>,
    event_idx: bool,
}

/// Why the device refused part of a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The chain itself was malformed.
    Chain(ChainError),
    /// The chain was well-formed but one of its buffers was not inside the shared region.
    Buffer(MemError),
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RejectReason::Chain(e) => write!(f, "{e}"),
            RejectReason::Buffer(e) => write!(f, "buffer rejected: {e}"),
        }
    }
}

/// Counters for a processing pass.
///
/// Rejections are *returned*, not logged. A device model is a library: it reports what happened and
/// the VMM decides whether that is a debug line, a metric, or grounds for stopping the guest. A
/// device that printed to stderr would be unusable in the one place this code is meant to resemble.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProcessStats {
    pub chains: u64,
    pub descriptors: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    /// One entry per chain the device refused, with the head index and the reason.
    pub rejected: Vec<(u16, RejectReason)>,
}

impl ProcessStats {
    pub fn errors(&self) -> usize {
        self.rejected.len()
    }
}

impl Device {
    pub fn new(layout: VirtqLayout, event_idx: bool) -> Self {
        Device {
            layout,
            next_avail: Wrapping(0),
            next_used: Wrapping(0),
            num_added: Wrapping(0),
            event_idx,
        }
    }

    pub fn next_used(&self) -> u16 {
        self.next_used.0
    }

    /// How many chains the driver has made available that this device has not consumed.
    ///
    /// Wrapping subtraction, not comparison. `avail_idx < next_avail` is meaningless once the
    /// counters have wrapped past 65,535; the *distance* between them is not.
    pub fn available(&self, mem: &SharedMem) -> Result<u16, MemError> {
        let avail_idx = Wrapping(mem.load_idx_acquire(self.layout.avail_idx())?);
        Ok((avail_idx - self.next_avail).0)
    }

    /// Take the head index of the next available chain, or `None` if there is nothing to do.
    pub fn pop_chain_head(&mut self, mem: &SharedMem) -> Result<Option<u16>, MemError> {
        if self.available(mem)? == 0 {
            return Ok(None);
        }
        let head = mem.read_u16(self.layout.avail_slot(self.next_avail.0))?;
        self.next_avail += Wrapping(1);
        Ok(Some(head))
    }

    /// Iterate the descriptors of a chain.
    pub fn chain<'a>(&self, mem: &'a SharedMem, head: u16) -> ChainIter<'a> {
        ChainIter::new(mem, self.layout, head)
    }

    /// Publish a completion: chain `head` is finished and the device wrote `len` bytes into it.
    ///
    /// `len` is the number of bytes **actually written**, not the size of the writable buffers the
    /// driver provided. Reporting the buffer size instead is a real and common bug: the driver
    /// takes this number as the length of valid data, so an over-report hands the guest whatever
    /// was in the buffer before - which, in a VMM that reuses buffers, is another guest's data.
    pub fn add_used(&mut self, mem: &mut SharedMem, head: u16, len: u32) -> Result<(), MemError> {
        let (id_addr, len_addr) = self.layout.used_slot(self.next_used.0);
        mem.write_u32(id_addr, head as u32)?;
        mem.write_u32(len_addr, len)?;

        self.next_used += Wrapping(1);
        self.num_added += Wrapping(1);

        // Release: the used element above must be visible before the index that advertises it.
        mem.store_idx_release(self.layout.used_idx(), self.next_used.0)
    }

    /// Should the device interrupt the driver?
    ///
    /// Without `EVENT_IDX`, always - one interrupt per completion. With it, only when the used
    /// index has crossed the value the driver asked about. See `layout::need_event`.
    ///
    /// This resets `num_added`, so it must be called exactly once per batch of completions, and its
    /// answer is about that whole batch.
    pub fn needs_notification(&mut self, mem: &SharedMem) -> Result<bool, MemError> {
        if !self.event_idx {
            self.num_added = Wrapping(0);
            return Ok(true);
        }
        let used_event = mem.load_relaxed(self.layout.used_event())?;
        let old = self.next_used - self.num_added;
        self.num_added = Wrapping(0);
        Ok(need_event(used_event, self.next_used.0, old.0))
    }

    /// Ask the driver to notify us again, and report whether work arrived while we were not
    /// looking.
    ///
    /// The return value is the important part and the reason this is not just a setter. Between the
    /// device deciding it has drained the queue and actually re-enabling notifications, the driver
    /// may have added a chain and skipped the kick because notifications were still disabled. The
    /// device would then sleep with work pending, and nothing would ever wake it.
    ///
    /// The fix is to re-check *after* enabling: if the avail index has moved past where we stopped,
    /// there is work, and the caller must loop instead of sleeping. Upstream calls this
    /// `enable_notification` and the double-check is the whole of its return value.
    pub fn enable_notification(&mut self, mem: &mut SharedMem) -> Result<bool, MemError> {
        if self.event_idx {
            // Publish "tell me when avail.idx passes where I stopped".
            mem.write_u16(self.layout.avail_event(), self.next_avail.0)?;
        } else {
            let flags = mem.read_u16(self.layout.used_flags())?;
            mem.write_u16(self.layout.used_flags(), flags & !VIRTQ_USED_F_NO_NOTIFY)?;
        }
        // SeqCst: the store above must not be reordered after the load below. If it were, the load
        // could observe the pre-store world and miss exactly the race this method exists to close.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        Ok(self.available(mem)? != 0)
    }

    pub fn disable_notification(&mut self, mem: &mut SharedMem) -> Result<(), MemError> {
        if self.event_idx {
            // With EVENT_IDX there is nothing to do: the mechanism is already one-shot. Once the
            // driver has been told about index N it will not kick again until N is passed, so
            // notifications are effectively disabled after each one fires. Upstream's
            // `set_notification` has this same empty branch, and it reads like an omission until
            // you see why.
            Ok(())
        } else {
            let flags = mem.read_u16(self.layout.used_flags())?;
            mem.write_u16(self.layout.used_flags(), flags | VIRTQ_USED_F_NO_NOTIFY)
        }
    }

    /// Drain every available chain, applying `work` to each.
    ///
    /// `work` receives the concatenated device-readable bytes and returns the bytes to place in the
    /// device-writable buffers. This is the shape of every virtio device: gather the request,
    /// produce a response, scatter it back.
    pub fn process<F>(&mut self, mem: &mut SharedMem, mut work: F) -> Result<ProcessStats, MemError>
    where
        F: FnMut(&[u8]) -> Vec<u8>,
    {
        let mut stats = ProcessStats::default();

        while let Some(head) = self.pop_chain_head(mem)? {
            // Collect the chain first, then act. A device that acted as it walked would already
            // have written to buffers by the time it discovered the chain was malformed.
            let mut readable: Vec<(u64, u32)> = Vec::new();
            let mut writable: Vec<(u64, u32)> = Vec::new();
            let mut chain_error = None;

            for item in self.chain(mem, head) {
                match item {
                    Ok(d) => {
                        stats.descriptors += 1;
                        if d.is_write_only() {
                            writable.push((d.addr, d.len));
                        } else {
                            readable.push((d.addr, d.len));
                        }
                    }
                    Err(e) => {
                        chain_error = Some(e);
                        break;
                    }
                }
            }

            if let Some(e) = chain_error {
                stats.rejected.push((head, RejectReason::Chain(e)));
                // Still complete it, with zero bytes written. A device that silently drops a
                // malformed chain leaks a descriptor: the driver waits forever for a completion
                // that will not come, and the queue slowly fills with entries nobody will free.
                // Reporting length 0 tells the driver the request produced nothing.
                self.add_used(mem, head, 0)?;
                stats.chains += 1;
                continue;
            }

            // Gather the request.
            let mut request = Vec::new();
            for &(addr, len) in &readable {
                match mem.read_slice(GuestAddr(addr), len as u64) {
                    Ok(s) => request.extend_from_slice(s),
                    Err(e) => stats.rejected.push((head, RejectReason::Buffer(e))),
                }
            }
            stats.bytes_read += request.len() as u64;

            let response = work(&request);

            // Scatter the response across the writable descriptors, in order, stopping when either
            // runs out. Truncating is correct; overrunning a descriptor because the response was
            // longer than the driver's buffer is a host-controlled write into guest memory at an
            // offset the guest did not agree to.
            let mut written = 0u32;
            let mut cursor = 0usize;
            for &(addr, len) in &writable {
                if cursor >= response.len() {
                    break;
                }
                let n = (len as usize).min(response.len() - cursor);
                if let Err(e) = mem.write_slice(GuestAddr(addr), &response[cursor..cursor + n]) {
                    stats.rejected.push((head, RejectReason::Buffer(e)));
                    break;
                }
                cursor += n;
                written += n as u32;
            }
            stats.bytes_written += written as u64;

            self.add_used(mem, head, written)?;
            stats.chains += 1;
        }

        Ok(stats)
    }
}
