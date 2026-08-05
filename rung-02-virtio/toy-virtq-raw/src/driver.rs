//! The driver side of the virtqueue: the half that lives inside the guest.
//!
//! Included because a virtqueue cannot be understood from one side. The notification-suppression
//! logic in particular is a *negotiation* - each side publishes a threshold the other reads - and
//! reading only the device half makes `used_event` look like a magic number that appears from
//! nowhere.
//!
//! In a real system this code is a Linux kernel driver (`drivers/virtio/virtio_ring.c`) and the
//! VMM never sees it. Writing it here is the cheapest way to see that the driver's `need_event`
//! call is the *same function* as the device's, with the two rings swapped.

use std::num::Wrapping;

use crate::layout::{
    VirtqLayout, need_event, DESC_SIZE, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
};
use crate::mem::{GuestAddr, MemError, SharedMem};

/// One buffer the driver wants in a chain.
pub struct Buffer {
    pub addr: GuestAddr,
    pub len: u32,
    /// True if the *device* may write here. The driver is making a promise about its own memory.
    pub device_writable: bool,
}

/// The driver half of one virtqueue, plus a bump allocator for buffers.
pub struct Driver {
    pub layout: VirtqLayout,
    /// Free-running count of chains published. This is `avail.idx`.
    avail_idx: Wrapping<u16>,
    /// The value of `avail.idx` at the previous notification decision.
    last_kick: Wrapping<u16>,
    /// Free-running count of completions collected. Compared against the device's `used.idx`.
    last_used: Wrapping<u16>,
    /// Descriptor table slots not currently in use, as a free list. A real driver keeps exactly
    /// this, threaded through the unused descriptors' `next` fields to avoid a separate allocation.
    free_descs: Vec<u16>,
    /// Chains published but not yet completed, keyed by head index, holding the descriptors to
    /// return to the free list. A real driver needs this too: the used ring gives back only the
    /// head, so the driver must remember the rest of the chain itself or walk it again. Losing
    /// track here is a descriptor leak, and a queue that slowly stops accepting work.
    outstanding: std::collections::HashMap<u16, Vec<u16>>,
    /// Bump pointer for buffer space, which begins after the rings.
    arena: u64,
    arena_end: u64,
    event_idx: bool,
}

impl Driver {
    pub fn new(layout: VirtqLayout, arena_end: u64, event_idx: bool) -> Self {
        Driver {
            layout,
            avail_idx: Wrapping(0),
            last_kick: Wrapping(0),
            last_used: Wrapping(0),
            // Descending so that `pop` hands out 0, 1, 2 ... which makes traces readable.
            free_descs: (0..layout.queue_size).rev().collect(),
            outstanding: std::collections::HashMap::new(),
            arena: layout.end().0.next_multiple_of(16),
            arena_end,
            event_idx,
        }
    }

    pub fn avail_idx(&self) -> u16 {
        self.avail_idx.0
    }

    pub fn free_descriptors(&self) -> usize {
        self.free_descs.len()
    }

    /// Allocate `len` bytes of buffer space and fill it with `data`.
    pub fn alloc(&mut self, mem: &mut SharedMem, data: &[u8]) -> Option<GuestAddr> {
        let addr = self.alloc_uninit(data.len() as u32)?;
        mem.write_slice(addr, data).ok()?;
        Some(addr)
    }

    /// Allocate `len` bytes without initialising them - for buffers the device will fill.
    pub fn alloc_uninit(&mut self, len: u32) -> Option<GuestAddr> {
        let addr = self.arena;
        let end = addr.checked_add(len as u64)?;
        if end > self.arena_end {
            return None;
        }
        // 8-byte align the next allocation. Not required by virtio - descriptors may point
        // anywhere - but it keeps the trace legible and matches what a real allocator would do.
        self.arena = end.next_multiple_of(8);
        Some(GuestAddr(addr))
    }

    /// Build a descriptor chain from `buffers` and make it available to the device.
    ///
    /// Returns the head descriptor index, which is the identifier the device will hand back in the
    /// used ring.
    ///
    /// # Ordering, which is the whole point
    ///
    /// 1. Write every descriptor.
    /// 2. Write the avail ring slot.
    /// 3. **Release fence.**
    /// 4. Bump `avail.idx`.
    ///
    /// Only step 4 makes the chain visible. If steps 1-2 could be reordered after step 4, the
    /// device would read an index advertising a descriptor that had not been written, and would
    /// process whatever was in that slot from the last time it was used. That is not a
    /// hypothetical: it is the single most common source of "virtio works on x86 and hangs on
    /// arm64" bugs, because x86's memory model hides the mistake and arm64's does not.
    pub fn add_chain(
        &mut self,
        mem: &mut SharedMem,
        buffers: &[Buffer],
    ) -> Result<u16, DriverError> {
        assert!(!buffers.is_empty(), "a chain needs at least one buffer");
        if buffers.len() > self.free_descs.len() {
            return Err(DriverError::OutOfDescriptors);
        }
        // The spec requires all device-readable descriptors before all device-writable ones
        // (VIRTIO 1.2, 2.7.5.3). The device checks this too, because it cannot trust the driver -
        // but a driver that violates it is simply broken, so assert rather than return.
        assert!(
            buffers.windows(2).all(|w| !w[0].device_writable || w[1].device_writable),
            "device-readable buffers must precede device-writable ones"
        );

        let indices: Vec<u16> = (0..buffers.len()).map(|_| self.free_descs.pop().unwrap()).collect();
        let head = indices[0];
        self.outstanding.insert(head, indices.clone());

        for (i, buf) in buffers.iter().enumerate() {
            let idx = indices[i];
            let last = i + 1 == buffers.len();
            let mut flags = 0u16;
            if buf.device_writable {
                flags |= VIRTQ_DESC_F_WRITE;
            }
            if !last {
                flags |= VIRTQ_DESC_F_NEXT;
            }
            let at = self.layout.desc(idx).expect("free list holds valid indices");
            mem.write_slice(at, &buf.addr.0.to_le_bytes())?;
            mem.write_u32(GuestAddr(at.0 + 8), buf.len)?;
            mem.write_u16(GuestAddr(at.0 + 12), flags)?;
            mem.write_u16(
                GuestAddr(at.0 + 14),
                if last { 0 } else { indices[i + 1] },
            )?;
            debug_assert_eq!(DESC_SIZE, 16);
        }

        // Step 2: publish the head index into the ring slot for this position.
        mem.write_u16(self.layout.avail_slot(self.avail_idx.0), head)?;

        // Steps 3 and 4, together.
        self.avail_idx += Wrapping(1);
        mem.store_idx_release(self.layout.avail_idx(), self.avail_idx.0)?;

        Ok(head)
    }

    /// Should the driver kick the device?
    ///
    /// The mirror image of `Device::needs_notification`, using `avail_event` instead of
    /// `used_event` and the avail counter instead of the used one. Same predicate, same reasoning.
    ///
    /// A kick is a write to a doorbell register, which in a real guest is an MMIO or port I/O store
    /// that exits to the VMM - the exact operation rung 1 measured at ~1.6 µs. Suppressing one is
    /// worth that much.
    pub fn needs_kick(&mut self, mem: &SharedMem) -> Result<bool, MemError> {
        let new = self.avail_idx;
        let old = self.last_kick;
        self.last_kick = new;
        if !self.event_idx {
            return Ok(true);
        }
        let avail_event = mem.load_relaxed(self.layout.avail_event())?;
        Ok(need_event(avail_event, new.0, old.0))
    }

    /// Tell the device which completion to interrupt us on.
    ///
    /// `used_event = last_used` means "interrupt me when one more completion arrives". A driver
    /// that is polling sets it far ahead instead, and receives no interrupts at all - which is how
    /// a busy-polling driver and an interrupt-driven one use the same mechanism.
    pub fn arm_used_event(&self, mem: &mut SharedMem) -> Result<(), MemError> {
        mem.write_u16(self.layout.used_event(), self.last_used.0)
    }

    /// Collect every completion the device has published since the last call.
    ///
    /// Returns `(head_index, bytes_written)` per completion, and frees the chain's descriptors.
    pub fn collect_used(&mut self, mem: &SharedMem) -> Result<Vec<(u16, u32)>, MemError> {
        let used_idx = Wrapping(mem.load_idx_acquire(self.layout.used_idx())?);
        let mut out = Vec::new();
        while self.last_used != used_idx {
            let (id_addr, len_addr) = self.layout.used_slot(self.last_used.0);
            let id = mem.read_u32(id_addr)? as u16;
            let len = mem.read_u32(len_addr)?;
            // Return the whole chain's descriptors, not just the head. The used ring reports only
            // the head, so a driver that freed only that would leak every other descriptor in
            // every multi-descriptor chain and eventually deadlock against its own free list.
            if let Some(descs) = self.outstanding.remove(&id) {
                self.free_descs.extend(descs);
            }
            out.push((id, len));
            self.last_used += Wrapping(1);
        }
        Ok(out)
    }
}

#[derive(Debug)]
pub enum DriverError {
    OutOfDescriptors,
    Mem(MemError),
}

impl From<MemError> for DriverError {
    fn from(e: MemError) -> Self {
        DriverError::Mem(e)
    }
}

impl std::fmt::Display for DriverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DriverError::OutOfDescriptors => write!(
                f,
                "no free descriptors: the queue is full until the device completes something"
            ),
            DriverError::Mem(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for DriverError {}
