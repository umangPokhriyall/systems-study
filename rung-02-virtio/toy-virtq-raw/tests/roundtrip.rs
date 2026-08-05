//! End-to-end tests for the hand-written virtqueue.
//!
//! Two groups. The first checks that a well-formed queue works. The second checks that a
//! *malformed* one is refused - and that group is the more important of the two, because a device
//! model that only works on well-behaved input is not a device model, it is a demo.

use toy_virtq_raw::device::{ChainError, Device, RejectReason};
use toy_virtq_raw::driver::{Buffer, Driver};
use toy_virtq_raw::layout::{
    VirtqLayout, need_event, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE,
};
use toy_virtq_raw::mem::{GuestAddr, SharedMem};

const REGION: usize = 64 * 1024;
const QS: u16 = 8;

fn setup(event_idx: bool) -> (SharedMem, Driver, Device, VirtqLayout) {
    let layout = VirtqLayout::new(GuestAddr(0), QS);
    (
        SharedMem::new(REGION),
        Driver::new(layout, REGION as u64, event_idx),
        Device::new(layout, event_idx),
        layout,
    )
}

/// Write a descriptor directly, bypassing the driver, and publish it. This is what a hostile guest
/// does: it is not running the driver, so no amount of driver-side validation constrains it.
fn publish_raw(mem: &mut SharedMem, layout: VirtqLayout, descs: &[(u64, u32, u16, u16)]) {
    for (i, &(addr, len, flags, next)) in descs.iter().enumerate() {
        let at = layout.desc(i as u16).unwrap();
        mem.write_slice(at, &addr.to_le_bytes()).unwrap();
        mem.write_u32(GuestAddr(at.0 + 8), len).unwrap();
        mem.write_u16(GuestAddr(at.0 + 12), flags).unwrap();
        mem.write_u16(GuestAddr(at.0 + 14), next).unwrap();
    }
    mem.write_u16(layout.avail_slot(0), 0).unwrap();
    mem.store_idx_release(layout.avail_idx(), 1).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Well-formed
// ---------------------------------------------------------------------------------------------

#[test]
fn gather_scatter_round_trip() {
    let (mut mem, mut driver, mut device, _) = setup(true);

    let a = driver.alloc(&mut mem, b"scatter ").unwrap();
    let b = driver.alloc(&mut mem, b"gather").unwrap();
    let reply = driver.alloc_uninit(64).unwrap();
    let head = driver
        .add_chain(
            &mut mem,
            &[
                Buffer { addr: a, len: 8, device_writable: false },
                Buffer { addr: b, len: 6, device_writable: false },
                Buffer { addr: reply, len: 64, device_writable: true },
            ],
        )
        .unwrap();

    let stats = device.process(&mut mem, |r| r.to_ascii_uppercase()).unwrap();
    assert_eq!(stats.chains, 1);
    assert_eq!(stats.descriptors, 3);
    assert_eq!(stats.errors(), 0);

    let completions = driver.collect_used(&mem).unwrap();
    assert_eq!(completions, vec![(head, 14)]);
    // `used.len` is bytes written, not the 64-byte buffer size. Reporting the buffer size would
    // hand the guest 50 bytes of whatever was in that buffer before.
    assert_eq!(mem.read_slice(reply, 14).unwrap(), b"SCATTER GATHER");
}

#[test]
fn every_descriptor_returns_to_the_free_list() {
    // A driver that frees only the head leaks the rest of every multi-descriptor chain, and the
    // queue silently stops accepting work after a while. The failure is far from the cause.
    let (mut mem, mut driver, mut device, _) = setup(true);
    for _ in 0..20 {
        let a = driver.alloc(&mut mem, b"x").unwrap();
        let r = driver.alloc_uninit(4).unwrap();
        driver
            .add_chain(
                &mut mem,
                &[
                    Buffer { addr: a, len: 1, device_writable: false },
                    Buffer { addr: r, len: 4, device_writable: true },
                ],
            )
            .unwrap();
        device.process(&mut mem, |r| r.to_vec()).unwrap();
        driver.collect_used(&mem).unwrap();
        assert_eq!(driver.free_descriptors(), QS as usize);
    }
}

#[test]
fn counters_are_free_running_and_survive_the_16_bit_wrap() {
    // 70,000 requests takes the u16 counters past 65,535 and back through zero. A device that
    // compared indices with `<` instead of subtracting would stop seeing work at that point, and
    // would have passed every test that ran fewer than 65,536 operations.
    let (mut mem, mut driver, mut device, _) = setup(false);
    let a = driver.alloc(&mut mem, b"x").unwrap();
    let r = driver.alloc_uninit(4).unwrap();
    let mut done = 0u32;
    for _ in 0..70_000 {
        driver
            .add_chain(
                &mut mem,
                &[
                    Buffer { addr: a, len: 1, device_writable: false },
                    Buffer { addr: r, len: 4, device_writable: true },
                ],
            )
            .unwrap();
        let stats = device.process(&mut mem, |v| v.to_vec()).unwrap();
        done += stats.chains as u32;
        driver.collect_used(&mem).unwrap();
    }
    assert_eq!(done, 70_000);
}

#[test]
fn event_idx_suppresses_all_but_one_kick_per_batch() {
    // The suppression count is a deterministic property of the protocol, so it can be asserted
    // exactly rather than measured. If this number changes, the notification logic changed.
    let layout = VirtqLayout::new(GuestAddr(0), 128);
    let mut mem = SharedMem::new(REGION);
    let mut driver = Driver::new(layout, REGION as u64, true);
    let mut device = Device::new(layout, true);
    let a = driver.alloc(&mut mem, b"x").unwrap();
    let r = driver.alloc_uninit(4).unwrap();

    let (batch, batches) = (8u32, 16u32);
    let mut kicks = 0;
    for _ in 0..batches {
        for _ in 0..batch {
            driver
                .add_chain(
                    &mut mem,
                    &[
                        Buffer { addr: a, len: 1, device_writable: false },
                        Buffer { addr: r, len: 4, device_writable: true },
                    ],
                )
                .unwrap();
            if driver.needs_kick(&mem).unwrap() {
                kicks += 1;
            }
        }
        device.process(&mut mem, |v| v.to_vec()).unwrap();
        device.needs_notification(&mem).unwrap();
        device.enable_notification(&mut mem).unwrap();
        driver.collect_used(&mem).unwrap();
    }
    assert_eq!(kicks, batches, "exactly one kick per batch of {batch}");
}

#[test]
fn need_event_is_the_same_function_in_both_directions() {
    // Not a behavioural test - a statement about the design. The driver's kick decision and the
    // device's interrupt decision are one predicate applied to two different pairs of counters.
    for (event, new, old) in [(0u16, 1u16, 0u16), (100, 106, 100), (0xffff, 0, 0xffff)] {
        assert_eq!(need_event(event, new, old), need_event(event, new, old));
    }
}

// ---------------------------------------------------------------------------------------------
// Malformed - the part that matters
// ---------------------------------------------------------------------------------------------

fn first_rejection(descs: &[(u64, u32, u16, u16)]) -> RejectReason {
    let (mut mem, _driver, mut device, layout) = setup(false);
    publish_raw(&mut mem, layout, descs);
    let stats = device.process(&mut mem, |r| r.to_vec()).unwrap();
    assert_eq!(stats.chains, 1, "a rejected chain must still be completed");
    *stats.rejected.first().map(|(_, r)| r).expect("chain should have been rejected")
}

#[test]
fn self_referential_descriptor_terminates() {
    assert_eq!(
        first_rejection(&[(0x2000, 16, VIRTQ_DESC_F_NEXT, 0)]),
        RejectReason::Chain(ChainError::TooLong { limit: QS })
    );
}

#[test]
fn two_descriptor_cycle_terminates() {
    assert_eq!(
        first_rejection(&[
            (0x2000, 16, VIRTQ_DESC_F_NEXT, 1),
            (0x2000, 16, VIRTQ_DESC_F_NEXT, 0),
        ]),
        RejectReason::Chain(ChainError::TooLong { limit: QS })
    );
}

#[test]
fn next_outside_the_table_is_refused() {
    assert_eq!(
        first_rejection(&[(0x2000, 16, VIRTQ_DESC_F_NEXT, 60_000)]),
        RejectReason::Chain(ChainError::IndexOutOfRange { index: 60_000, queue_size: QS })
    );
}

#[test]
fn chain_claiming_more_than_4_gib_is_refused() {
    // Two descriptors of 2 GiB each. Short, in range, and asking the device to move 4 GiB.
    // `yielded_bytes` is the only bound that catches this.
    let r = first_rejection(&[
        (0x2000, 0x8000_0000, VIRTQ_DESC_F_NEXT, 1),
        (0x2000, 0x8000_0000, 0, 0),
    ]);
    assert_eq!(r, RejectReason::Chain(ChainError::TooManyBytes));
}

#[test]
fn readable_after_writable_is_refused() {
    assert_eq!(
        first_rejection(&[
            (0x2000, 16, VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT, 1),
            (0x2000, 16, 0, 0),
        ]),
        RejectReason::Chain(ChainError::ReadableAfterWritable)
    );
}

#[test]
fn buffer_outside_the_region_is_refused() {
    // Address chosen so that `addr + len` overflows u64. A bounds check written as
    // `addr + len <= region_len` rather than with checked arithmetic accepts this.
    match first_rejection(&[(u64::MAX - 8, 4096, 0, 0)]) {
        RejectReason::Buffer(_) => {}
        other => panic!("expected a buffer rejection, got {other:?}"),
    }
}

#[test]
fn a_rejected_chain_is_still_completed() {
    // The subtle one. Dropping a malformed chain instead of completing it leaks a descriptor and
    // leaves the driver waiting forever for a request that vanished. The queue degrades rather
    // than failing, which makes it hard to diagnose.
    let (mut mem, _driver, mut device, layout) = setup(false);
    publish_raw(&mut mem, layout, &[(0x2000, 16, VIRTQ_DESC_F_NEXT, 0)]);
    device.process(&mut mem, |r| r.to_vec()).unwrap();
    assert_eq!(device.next_used(), 1, "used.idx must advance even for a rejected chain");
    assert_eq!(mem.read_u32(layout.used_slot(0).1).unwrap(), 0, "with length 0");
}
