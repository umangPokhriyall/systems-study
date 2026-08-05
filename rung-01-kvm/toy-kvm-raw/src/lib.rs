//! A minimal x86-64 VMM built directly on `/dev/kvm`, with no virtualization crates.
//!
//! The binary in `main.rs` is a thin driver; everything worth reading is here:
//!
//! - [`kvm_sys`] - the raw ABI. ioctl numbers computed from the kernel's `_IOC` encoding, and the
//!   `#[repr(C)]` structures they carry, with their sizes asserted at compile time.
//! - [`guest`] - the guest programs, hand-assembled, one annotated byte at a time.
//! - [`device`] - the device model: a write-only serial port and a one-register MMIO device.
//! - [`vmm`] - the VMM: create, configure, run, and dispatch on exits.
//!
//! It is a library as well as a binary for two reasons: so the guest programs can be shared with
//! `toy-kvm-crates`, which reimplements the same VMM on `kvm-ioctls` and `vm-memory`, and so the
//! whole boot sequence can be exercised from an integration test rather than only by eye.

pub mod device;
pub mod guest;
pub mod kvm_sys;
pub mod vmm;
