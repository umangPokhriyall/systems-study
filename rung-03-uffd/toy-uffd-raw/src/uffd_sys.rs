//! The raw `userfaultfd` ABI: the syscall, the ioctl numbers, and the structures they carry.
//!
//! Nothing here is imported from a crate. Everything corresponds to
//! `include/uapi/linux/userfaultfd.h`, and as in rung 1 the ioctl numbers are *computed* from the
//! kernel's `_IOC` encoding rather than pasted, so that the size field is derived from
//! `size_of::<T>()` and a wrong structure layout is caught by the kernel with `ENOTTY` instead of
//! being interpreted as data.
//!
//! # What `userfaultfd` is
//!
//! A file descriptor that receives page faults. Normally a fault on anonymous memory is handled
//! entirely inside the kernel: allocate a zero page, install a PTE, resume. With a region registered
//! to a `userfaultfd` in *missing* mode, the kernel instead **parks the faulting thread** and
//! publishes a message on the fd. Some other thread reads it, decides what should be at that
//! address, installs the page with `UFFDIO_COPY`, and the kernel wakes the faulting thread, which
//! resumes as though memory had always been there.
//!
//! That is the whole mechanism behind demand-paged snapshot restore: a VM can start executing
//! against a memory image that has not been read off disk yet, and the pages arrive as the guest
//! asks for them.

#![allow(non_camel_case_types)]

// ---------------------------------------------------------------------------------------------
// ioctl request-number encoding (see rung 1's `kvm_sys.rs` for the full picture)
// ---------------------------------------------------------------------------------------------

const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = 8;
const IOC_SIZESHIFT: u32 = 16;
const IOC_DIRSHIFT: u32 = 30;

const IOC_READ: u32 = 2;
const IOC_WRITE: u32 = 1;

/// The type byte `userfaultfd` owns. Note it is also the value of [`UFFD_API`] - the same 0xAA,
/// used for two unrelated purposes, which is confusing exactly once.
const UFFDIO: u32 = 0xAA;

const fn ioc(dir: u32, nr: u32, size: usize) -> u64 {
    ((dir << IOC_DIRSHIFT)
        | ((size as u32) << IOC_SIZESHIFT)
        | (UFFDIO << IOC_TYPESHIFT)
        | (nr << IOC_NRSHIFT)) as u64
}

/// Kernel reads the struct and writes back into it.
const fn iowr(nr: u32, size: usize) -> u64 {
    ioc(IOC_READ | IOC_WRITE, nr, size)
}
/// Kernel reads the struct. (The kernel's own header spells `UFFDIO_WAKE` and `UFFDIO_UNREGISTER`
/// with `_IOR`, which reads backwards - the direction bits are named from userspace's point of view
/// and the header is inconsistent about it. Copied faithfully rather than corrected, because the
/// number has to match.)
const fn ior(nr: u32, size: usize) -> u64 {
    ioc(IOC_READ, nr, size)
}

// ---------------------------------------------------------------------------------------------
// The syscall
// ---------------------------------------------------------------------------------------------

/// Restrict this fd to faults taken in **user mode**. Faults taken while the kernel is touching the
/// region on the process's behalf - inside a `read()` into it, say - are then handled normally
/// instead of being reported.
///
/// This exists for security. A `userfaultfd` lets an unprivileged process *stall the kernel* at a
/// chosen instruction for an unbounded time, which turns a large class of hard-to-win kernel race
/// conditions into easy ones. Restricting to user-mode faults removes that primitive.
///
/// On this machine `/proc/sys/vm/unprivileged_userfaultfd` is **0**, which since Linux 5.11 means
/// an unprivileged process may create a `userfaultfd` *only* with this flag. Without it the syscall
/// returns `EPERM`. See `COMMON-MISTAKES.md`.
pub const UFFD_USER_MODE_ONLY: i32 = 1;

/// Create a userfaultfd. `flags` takes `O_CLOEXEC`, `O_NONBLOCK` and [`UFFD_USER_MODE_ONLY`].
///
/// # Safety
/// None beyond the syscall itself; the returned fd is owned by the caller.
pub unsafe fn userfaultfd(flags: i32) -> i32 {
    // SAFETY: a syscall with an integer argument and an integer return.
    unsafe { libc::syscall(libc::SYS_userfaultfd, flags) as i32 }
}

// ---------------------------------------------------------------------------------------------
// The ioctls
// ---------------------------------------------------------------------------------------------

/// Handshake. Must be the first ioctl on the fd; nothing else is accepted before it.
pub const UFFDIO_API: u64 = iowr(0x3F, size_of::<uffdio_api>());
/// Start reporting faults for a range.
pub const UFFDIO_REGISTER: u64 = iowr(0x00, size_of::<uffdio_register>());
pub const UFFDIO_UNREGISTER: u64 = ior(0x01, size_of::<uffdio_range>());
/// Wake threads parked on a range without installing anything. Used with `DONTWAKE` batching.
pub const UFFDIO_WAKE: u64 = ior(0x02, size_of::<uffdio_range>());
/// Install page content at a faulting address and (by default) wake whoever was waiting.
pub const UFFDIO_COPY: u64 = iowr(0x03, size_of::<uffdio_copy>());
/// Install zeroed pages. Cheaper than `UFFDIO_COPY` because there is no source to read.
pub const UFFDIO_ZEROPAGE: u64 = iowr(0x04, size_of::<uffdio_zeropage>());

/// The API version. Confusingly equal to the ioctl type byte; unrelated.
pub const UFFD_API: u64 = 0xAA;

/// Report faults on pages that have no backing yet. The mode this rung uses, and the mode snapshot
/// restore uses.
pub const UFFDIO_REGISTER_MODE_MISSING: u64 = 1 << 0;
/// Report faults on *writes to present pages* - write protection. The basis of dirty tracking for
/// live migration, and not used here.
pub const UFFDIO_REGISTER_MODE_WP: u64 = 1 << 1;

/// Install the page but do **not** wake the faulting thread yet.
///
/// The point is batching: a handler servicing many faults can install several pages and then issue
/// one `UFFDIO_WAKE` over the whole range, paying one wakeup instead of one per page.
pub const UFFDIO_COPY_MODE_DONTWAKE: u64 = 1 << 0;

pub const UFFD_EVENT_PAGEFAULT: u8 = 0x12;

/// The fault was a write. Absent, it was a read.
pub const UFFD_PAGEFAULT_FLAG_WRITE: u64 = 1 << 0;

// ---------------------------------------------------------------------------------------------
// Structures
// ---------------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct uffdio_api {
    /// In: the version userspace speaks. Out: unchanged.
    pub api: u64,
    /// In: features requested. Out: features the kernel supports.
    ///
    /// This is a negotiation, not a query: passing a feature the kernel does not have fails the
    /// whole ioctl with `EINVAL`. The way to *discover* features is to pass 0 and read what comes
    /// back, then re-do the handshake on a fresh fd asking for what you want.
    pub features: u64,
    /// Out: bitmask of which `_UFFDIO_*` ioctls this fd supports.
    pub ioctls: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct uffdio_range {
    pub start: u64,
    pub len: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct uffdio_register {
    pub range: uffdio_range,
    /// In: `UFFDIO_REGISTER_MODE_*`.
    pub mode: u64,
    /// Out: which ioctls are valid for *this range*. Worth checking rather than assuming - a range
    /// registered in WP mode does not accept `UFFDIO_COPY`.
    pub ioctls: u64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct uffdio_copy {
    /// Destination in the registered region. Must be page-aligned.
    pub dst: u64,
    /// Source, anywhere in this process. Must be page-aligned.
    pub src: u64,
    /// Length, a multiple of the page size. May cover many pages in one call.
    pub len: u64,
    pub mode: u64,
    /// Out: bytes copied, or a negative errno.
    ///
    /// A negative value here is **not** an ioctl failure - the ioctl returns 0 and reports the
    /// error in this field. `-EEXIST` in particular means another thread already installed the
    /// page, which is normal in a multi-threaded handler and must not be treated as fatal.
    pub copy: i64,
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct uffdio_zeropage {
    pub range: uffdio_range,
    pub mode: u64,
    /// Out: bytes zeroed, or a negative errno. Same convention as `uffdio_copy::copy`.
    pub zeropage: i64,
}

/// A message read from the userfaultfd.
///
/// The kernel's version ends in a union with one variant per event type; as in rung 1 the union is
/// flattened here to the only variant this rung handles, with the total size asserted so a layout
/// mistake is caught at build time rather than as a wrong fault address.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct uffd_msg {
    pub event: u8,
    pub reserved1: u8,
    pub reserved2: u16,
    pub reserved3: u32,
    // union begins here, at offset 8. `pagefault` variant:
    /// `UFFD_PAGEFAULT_FLAG_*`.
    pub pagefault_flags: u64,
    /// The faulting address, **rounded down to a page boundary** by the kernel.
    pub pagefault_address: u64,
    /// Faulting thread id, only meaningful with `UFFD_FEATURE_THREAD_ID`.
    pub pagefault_ptid: u32,
    pub _pad: u32,
}

const _: () = {
    assert!(size_of::<uffdio_api>() == 24);
    assert!(size_of::<uffdio_range>() == 16);
    assert!(size_of::<uffdio_register>() == 32);
    assert!(size_of::<uffdio_copy>() == 40);
    assert!(size_of::<uffdio_zeropage>() == 32);
    // The kernel writes exactly 32 bytes per message and `read()` returns a multiple of that. A
    // wrong size here would silently mis-frame every message after the first.
    assert!(size_of::<uffd_msg>() == 32);
};
