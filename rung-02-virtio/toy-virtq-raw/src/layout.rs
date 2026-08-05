//! The split virtqueue memory layout, from the VIRTIO 1.2 specification, chapter 2.7.
//!
//! A virtqueue is not an object. It is an *agreement about the meaning of bytes* at three addresses
//! in shared memory, plus two doorbells. Nothing in this file allocates anything or owns anything -
//! it only computes where each field lives and what it means.
//!
//! # The three rings
//!
//! ```text
//!   descriptor table            avail ring (driver -> device)      used ring (device -> driver)
//!   16 bytes x queue_size       written by the driver              written by the device
//!
//!   +---------------------+     +---------------------+            +---------------------+
//! 0 | addr len flags next |     | flags               | +0         | flags               | +0
//!   +---------------------+     | idx                 | +2         | idx                 | +2
//! 1 | addr len flags next |     +---------------------+            +---------------------+
//!   +---------------------+     | ring[0]  (le16)     | +4         | ring[0].id   (le32) | +4
//! 2 | addr len flags next |     | ring[1]             | +6         | ring[0].len  (le32) | +8
//!   +---------------------+     | ...                 |            | ring[1].id          | +12
//!   | ...                 |     | ring[qsize-1]       |            | ...                 |
//!   +---------------------+     +---------------------+            +---------------------+
//!   | addr len flags next |     | used_event   (le16) |            | avail_event  (le16) |
//!   +---------------------+     +---------------------+            +---------------------+
//! ```
//!
//! The descriptor table is a *pool*, not a queue. Entries in it are not consumed in order and are
//! not ordered at all; the avail ring is what imposes order, by carrying the *index* of the head
//! descriptor of each chain the driver wants processed.
//!
//! That indirection is the reason a chain can be scattered: one request may be a header in one
//! buffer, a payload in another, and a status byte in a third, each at an unrelated address, linked
//! by `next`. The device sees one logical request; the guest never had to make it contiguous.
//!
//! # The two counters
//!
//! `avail.idx` and `used.idx` are **free-running 16-bit counters that are never reset**. They count
//! total entries ever published, and they wrap at 65,536. The ring slot for entry `i` is
//! `ring[i % queue_size]`.
//!
//! This is worth dwelling on because almost every subtle virtqueue bug is here. There is no
//! "empty/full" flag and no separate count. The consumer knows how much work is outstanding by
//! subtracting its own position from the published index, **in wrapping arithmetic**. Comparing
//! them with `<` instead of subtracting is a bug that works perfectly for the first 65,536
//! operations.
//!
//! # What this file does not implement
//!
//! - **Indirect descriptors** (`VIRTQ_DESC_F_INDIRECT`), where one descriptor points at a whole
//!   table of descriptors living in a buffer. See `EXERCISES.md`.
//! - **Packed virtqueues**, the VIRTIO 1.1 alternative layout that folds all three rings into one
//!   array to improve cache behaviour. Split rings are what Cloud Hypervisor and Firecracker use
//!   for the devices this study targets.

use crate::mem::GuestAddr;

/// Size of one descriptor: `le64 addr, le32 len, le16 flags, le16 next`.
pub const DESC_SIZE: u64 = 16;

/// This descriptor is not the last in its chain; `next` names the following one.
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// The buffer is **device-writable**. Absent, it is device-readable.
///
/// The direction is stated from the device's point of view, and it is a promise the driver makes,
/// not one the device may assume. A device that writes to a descriptor without this flag is
/// corrupting a buffer the driver may still be reading. This is the single most important flag in
/// virtio and it is checked in `device.rs` rather than trusted.
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
/// The buffer contains a table of descriptors rather than data. Not implemented here.
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// In `used.flags`, set by the *device*: "driver, do not notify me". The coarse predecessor of
/// `EVENT_IDX` - all or nothing, where `EVENT_IDX` says "not until index N".
pub const VIRTQ_USED_F_NO_NOTIFY: u16 = 1;
/// In `avail.flags`, set by the *driver*: "device, do not interrupt me".
pub const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;

/// One entry in the descriptor table, as the driver wrote it.
///
/// Every field is guest-controlled. `addr` and `len` describe a buffer that the device is about to
/// dereference, `next` is an index the device is about to follow, and `flags` decides whether the
/// device may write. Treating any of them as trustworthy is how a device model becomes a guest
/// escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

impl Descriptor {
    pub fn has_next(&self) -> bool {
        self.flags & VIRTQ_DESC_F_NEXT != 0
    }
    pub fn is_write_only(&self) -> bool {
        self.flags & VIRTQ_DESC_F_WRITE != 0
    }
    pub fn is_indirect(&self) -> bool {
        self.flags & VIRTQ_DESC_F_INDIRECT != 0
    }
}

/// Where each part of a split virtqueue lives in shared memory.
///
/// In VIRTIO 1.0 and later the three rings are located independently - the driver writes three
/// separate addresses into the device's configuration registers, and they need not be adjacent.
/// This layout places them contiguously with the required alignment because it is easier to read,
/// and records the alignment explicitly so the requirement is visible rather than accidental.
#[derive(Debug, Clone, Copy)]
pub struct VirtqLayout {
    pub queue_size: u16,
    pub desc_table: GuestAddr,
    pub avail_ring: GuestAddr,
    pub used_ring: GuestAddr,
}

/// Alignment required of each ring by VIRTIO 1.2 section 2.7.
///
/// These are not arbitrary. Each is the natural alignment of the largest field in the structure, so
/// that no field straddles a cache line boundary in a way that would make a `le32` read non-atomic
/// on some architecture. A misaligned ring is a spec violation the device is entitled to reject,
/// and several device models do.
const DESC_ALIGN: u64 = 16;
const AVAIL_ALIGN: u64 = 2;
const USED_ALIGN: u64 = 4;

const fn align_up(v: u64, align: u64) -> u64 {
    v.div_ceil(align) * align
}

impl VirtqLayout {
    /// Lay out a queue of `queue_size` entries starting at `base`.
    ///
    /// # Panics
    /// If `queue_size` is not a power of two, or is zero, or exceeds 32,768.
    ///
    /// The power-of-two requirement (VIRTIO 1.2, 2.7) exists so that `index % queue_size` is a
    /// mask rather than a division. It is also what makes the wrapping arithmetic on the free-running
    /// counters work out: with a power-of-two size, a 16-bit counter wrapping at 65,536 lands
    /// exactly on a ring boundary, so the slot mapping stays consistent across the wrap.
    pub fn new(base: GuestAddr, queue_size: u16) -> Self {
        assert!(
            queue_size > 0 && queue_size.is_power_of_two() && queue_size <= 32768,
            "queue size must be a power of two in 1..=32768, got {queue_size}"
        );
        let qs = queue_size as u64;

        let desc_table = align_up(base.0, DESC_ALIGN);
        let avail_ring = align_up(desc_table + DESC_SIZE * qs, AVAIL_ALIGN);
        // avail: flags(2) + idx(2) + ring(2*qs) + used_event(2)
        let used_ring = align_up(avail_ring + 4 + 2 * qs + 2, USED_ALIGN);

        VirtqLayout {
            queue_size,
            desc_table: GuestAddr(desc_table),
            avail_ring: GuestAddr(avail_ring),
            used_ring: GuestAddr(used_ring),
        }
    }

    /// Total bytes occupied, so a caller can size the region or place something after it.
    pub fn total_size(&self) -> u64 {
        let qs = self.queue_size as u64;
        // used: flags(2) + idx(2) + ring(8*qs) + avail_event(2)
        (self.used_ring.0 + 4 + 8 * qs + 2) - self.desc_table.0
    }

    pub fn end(&self) -> GuestAddr {
        GuestAddr(self.desc_table.0 + self.total_size())
    }

    // --- descriptor table ---

    /// Address of descriptor `index`. Returns `None` if the index is out of range, which is the
    /// caller's cue that the driver supplied a bad `next` - not a reason to panic.
    pub fn desc(&self, index: u16) -> Option<GuestAddr> {
        if index >= self.queue_size {
            return None;
        }
        Some(GuestAddr(self.desc_table.0 + index as u64 * DESC_SIZE))
    }

    // --- avail ring: written by the driver, read by the device ---

    pub fn avail_flags(&self) -> GuestAddr {
        self.avail_ring
    }
    pub fn avail_idx(&self) -> GuestAddr {
        GuestAddr(self.avail_ring.0 + 2)
    }
    /// Slot for the `i`-th entry ever published. Note the modulo: `i` is a free-running counter,
    /// the slot is not.
    pub fn avail_slot(&self, i: u16) -> GuestAddr {
        GuestAddr(self.avail_ring.0 + 4 + (i % self.queue_size) as u64 * 2)
    }
    /// `used_event`: written by the **driver**, read by the device. "Do not interrupt me until
    /// `used.idx` reaches this value plus one." Lives at the end of the avail ring because it is
    /// driver-written, which is the rule for telling the two event fields apart.
    pub fn used_event(&self) -> GuestAddr {
        GuestAddr(self.avail_ring.0 + 4 + self.queue_size as u64 * 2)
    }

    // --- used ring: written by the device, read by the driver ---

    pub fn used_flags(&self) -> GuestAddr {
        self.used_ring
    }
    pub fn used_idx(&self) -> GuestAddr {
        GuestAddr(self.used_ring.0 + 2)
    }
    /// `(id, len)` for the `i`-th completion ever published. `id` is the *head* descriptor index of
    /// the chain, which is how the driver knows which of its outstanding requests finished. `len`
    /// is the number of bytes the device **wrote**, which is not the same as the size of the
    /// writable buffers it was given.
    pub fn used_slot(&self, i: u16) -> (GuestAddr, GuestAddr) {
        let base = self.used_ring.0 + 4 + (i % self.queue_size) as u64 * 8;
        (GuestAddr(base), GuestAddr(base + 4))
    }
    /// `avail_event`: written by the **device**, read by the driver. The mirror image of
    /// `used_event`: "do not notify me until `avail.idx` reaches this value plus one."
    pub fn avail_event(&self) -> GuestAddr {
        GuestAddr(self.used_ring.0 + 4 + self.queue_size as u64 * 8)
    }
}

/// The notification-suppression predicate, shared by both directions.
///
/// This is the whole of `EVENT_IDX`, and it is the same three-term expression on both sides -
/// the device uses it with `used.idx` and `used_event`, the driver with `avail.idx` and
/// `avail_event`. Recognising that they are one function rather than two symmetric special cases is
/// most of understanding the feature.
///
/// ```text
///   wrapping(new - event - 1)  <  wrapping(new - old)
/// ```
///
/// In words: **has the counter crossed `event + 1` since we last checked?** `old` is where the
/// counter stood at the previous check, `new` is where it stands now, and `event` is the value the
/// other side asked to be told about.
///
/// Everything is `u16` wrapping arithmetic, and it must be, because these counters are free-running
/// and wrap at 65,536. The subtractions convert absolute positions into *distances*, which stay
/// meaningful across a wrap where the absolute values do not - `new < event` is nonsense once the
/// counter has wrapped past 65,535, but `new - event` is still the right distance.
///
/// Taken from the Linux kernel's `vring_need_event()` (`drivers/virtio/virtio_ring.c`), which is
/// also what `virtio-queue`'s `needs_notification` implements. The spec (VIRTIO 1.2, 2.7.7)
/// describes the simpler rule "notify when `idx == event + 1`"; every real implementation uses the
/// inequality instead, so that a batch of completions added between two checks cannot step over the
/// exact equality and lose the notification entirely.
pub fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_alignment_is_satisfied() {
        for qs in [1u16, 2, 8, 256, 32768] {
            let l = VirtqLayout::new(GuestAddr(3), qs); // deliberately misaligned base
            assert_eq!(l.desc_table.0 % DESC_ALIGN, 0);
            assert_eq!(l.avail_ring.0 % AVAIL_ALIGN, 0);
            assert_eq!(l.used_ring.0 % USED_ALIGN, 0);
        }
    }

    #[test]
    fn rings_do_not_overlap() {
        let l = VirtqLayout::new(GuestAddr(0), 8);
        assert!(l.desc_table.0 + DESC_SIZE * 8 <= l.avail_ring.0);
        assert!(l.used_event().0 + 2 <= l.used_ring.0);
        assert!(l.avail_event().0 + 2 <= l.end().0);
    }

    #[test]
    fn ring_slots_wrap_at_queue_size_not_at_the_counter() {
        let l = VirtqLayout::new(GuestAddr(0), 8);
        assert_eq!(l.avail_slot(0), l.avail_slot(8));
        assert_eq!(l.avail_slot(3), l.avail_slot(11));
    }

    #[test]
    fn need_event_fires_exactly_once_on_crossing() {
        // The driver asked to be told when the counter reaches 5 (i.e. event = 4).
        assert!(!need_event(4, 4, 3), "not there yet");
        assert!(need_event(4, 5, 4), "crossed");
        assert!(!need_event(4, 6, 5), "already told");
    }

    #[test]
    fn need_event_is_correct_across_the_16_bit_wrap() {
        // This is the case a naive `new > event` comparison gets wrong, and it happens once every
        // 65,536 operations - often enough to be a real bug, rare enough to survive testing.
        assert!(need_event(0xffff, 0x0000, 0xffff), "wrapped past the event value");
        assert!(!need_event(0x0002, 0x0001, 0x0000), "event is still ahead of us");
    }

    #[test]
    fn need_event_handles_a_batch_stepping_over_the_target() {
        // Six completions added between two checks, with the target in the middle. The spec's
        // simple `idx == event + 1` rule would miss this entirely; the inequality catches it.
        assert!(need_event(100, 106, 100));
    }
}
