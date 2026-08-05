//! A split virtqueue implemented by hand, both halves of it, over a plain byte buffer.
//!
//! Rung 1 established that guest memory is ordinary host memory and that a VM exit costs ~1.6 µs.
//! Virtio is the protocol that follows from those two facts: put the data in the shared memory,
//! where it costs nothing to reach, and use the expensive exit only as a *doorbell*, as rarely as
//! the protocol allows.
//!
//! - [`mem`] - the shared region, with the bounds checking every guest-supplied address needs.
//! - [`layout`] - where the three rings live and what each field means, plus `need_event`, which is
//!   the whole of `EVENT_IDX`.
//! - [`driver`] - the guest half: build chains, publish them, decide whether to kick.
//! - [`device`] - the VMM half: walk chains without trusting them, complete them, decide whether to
//!   interrupt.
//!
//! `toy-virtq-crates` reimplements the device half on `virtio-queue` and `vm-memory` against this
//! same layout, so the two can be compared directly.

pub mod device;
pub mod driver;
pub mod layout;
pub mod mem;
