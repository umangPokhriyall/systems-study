//! A layout bug in `virtio-queue`'s `mock.rs`, found while building rung 2.
//!
//! `SplitQueueRing::end()` computes the end of a ring as `ring.addr + ring.len`, where `ring.len`
//! is the number of *elements*, not the number of bytes, and where `ring.addr` already skips the
//! 2-byte `flags` and 2-byte `idx` header. The trailing `event` field is not counted either.
//!
//! For an avail ring of N entries the true size is `4 + 2*N + 2` bytes, but `end()` reports
//! `4 + N`. `MockSplitQueue::create` derives the used ring's address from that, so for every queue
//! size the used ring is placed *inside* the avail ring.
//!
//! The proposed fix is to count bytes rather than elements, and to include the header and the
//! trailing event field:
//!
//! ```ignore
//! pub fn end(&self) -> GuestAddress {
//!     self.ring
//!         .addr
//!         .checked_add((self.ring.len * size_of::<T>()) as GuestUsize)
//!         .and_then(|a| a.checked_add(size_of::<u16>() as GuestUsize)) // the event field
//!         .unwrap()
//! }
//! ```
//!
//! Scope: `mock.rs` is behind the `test-utils` feature, so this affects tests and test harnesses
//! rather than any shipping VMM. It is still worth fixing, because a test harness that silently
//! corrupts the structure under test is worse than no harness - it was found here because
//! `needs_notification` returned an answer that disagreed with a hand-written implementation, and
//! the cause was that `used_event` had been overwritten by a used-ring element.
//!
//! Verified against virtio-queue 0.18.0 from crates.io on 2026-08-05.
use virtio_queue::mock::MockSplitQueue;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

const REGION: usize = 0x20_0000;

/// Ignored by default so `cargo test --workspace` stays green: these tests fail on purpose, to
/// demonstrate current upstream behaviour. Run them with:
///
/// ```text
/// cargo test -p toy-virtq-crates --test mock_layout -- --ignored --nocapture
/// ```
///
/// If either of them starts passing, upstream has fixed it and this file should be deleted.
#[test]
#[ignore = "demonstrates an upstream bug: run with --ignored"]
fn used_ring_overlaps_avail_ring() {
    for len in [2u16, 8, 16, 128, 256] {
        let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), REGION)]).unwrap();
        let vq = MockSplitQueue::new(&mem, len);

        let avail = vq.avail_addr().0;
        let used = vq.used_addr().0;
        // VIRTIO 1.2, 2.7.6: flags(2) + idx(2) + ring(2*len) + used_event(2).
        let avail_true_end = avail + 4 + 2 * len as u64 + 2;

        println!(
            "len={len:<4} avail={avail:<6} used={used:<6} avail_true_end={avail_true_end:<6} \
             overlap={} bytes",
            avail_true_end.saturating_sub(used)
        );
        assert!(
            used >= avail_true_end,
            "used ring at {used} starts inside the avail ring, which ends at {avail_true_end}"
        );
    }
}

#[test]
#[ignore = "demonstrates an upstream bug: run with --ignored"]
fn used_ring_writes_corrupt_available_entries() {
    // The consequence, demonstrated rather than argued: writing one used element clobbers avail
    // ring entries the device has not consumed yet.
    let len = 8u16;
    let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), REGION)]).unwrap();
    let vq = MockSplitQueue::new(&mem, len);

    // Fill the avail ring with a recognisable pattern, as a driver publishing 8 chains would.
    let avail_ring = vq.avail_addr().0 + 4;
    for i in 0..len {
        mem.write_obj::<u16>(0xAA00 | i, GuestAddress(avail_ring + 2 * i as u64)).unwrap();
    }
    let before: Vec<u16> = (0..len)
        .map(|i| mem.read_obj::<u16>(GuestAddress(avail_ring + 2 * i as u64)).unwrap())
        .collect();

    // The device completes one chain: used element 0 = (id, len).
    let used_ring = vq.used_addr().0 + 4;
    mem.write_obj::<u32>(0, GuestAddress(used_ring)).unwrap();
    mem.write_obj::<u32>(0x1234_5678, GuestAddress(used_ring + 4)).unwrap();

    let after: Vec<u16> = (0..len)
        .map(|i| mem.read_obj::<u16>(GuestAddress(avail_ring + 2 * i as u64)).unwrap())
        .collect();

    println!("avail before: {before:04x?}");
    println!("avail after : {after:04x?}");
    assert_eq!(before, after, "writing one used element changed the available ring");
}
