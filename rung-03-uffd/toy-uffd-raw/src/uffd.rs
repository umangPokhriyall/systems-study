//! A thin owned wrapper over a userfaultfd, and the region it watches.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::uffd_sys::*;

/// 4 KiB. Read from the system rather than assumed, in `Region::new`.
pub const PAGE: usize = 4096;

/// An owned userfaultfd, already through the API handshake.
pub struct Uffd {
    fd: OwnedFd,
    /// Which `_UFFDIO_*` ioctls the kernel reported as available on this fd.
    pub supported_ioctls: u64,
}

impl Uffd {
    /// Create a userfaultfd and perform the mandatory API handshake.
    ///
    /// `UFFD_USER_MODE_ONLY` is tried second rather than first so the failure mode is visible: on a
    /// machine with `vm.unprivileged_userfaultfd = 1` the plain call works, and on one with it set
    /// to 0 the plain call returns `EPERM` and only the restricted form is permitted. Reporting
    /// which path was taken matters, because the restricted fd genuinely cannot do everything the
    /// unrestricted one can.
    /// `non_blocking` decides whether [`read_event`](Self::read_event) can be used (it polls, so it
    /// needs `O_NONBLOCK`) or whether the caller intends to block in `read()`.
    ///
    /// It is a parameter rather than a constant because getting it wrong is silent and severe: a
    /// handler that assumes a blocking fd, gets a non-blocking one, sees `EAGAIN`, mistakes it for
    /// end-of-file and exits - and then the userfaultfd is *closed*. The kernel does not hang the
    /// faulting threads at that point. It resolves every outstanding and future fault with a **zero
    /// page**. In a VMM that is silent corruption of the restored guest's memory, and it looks
    /// exactly like a successful restore. See `COMMON-MISTAKES.md` #2.
    pub fn new(non_blocking: bool) -> io::Result<(Self, bool)> {
        let mut base = libc::O_CLOEXEC;
        if non_blocking {
            base |= libc::O_NONBLOCK;
        }

        // SAFETY: plain syscall.
        let mut restricted = false;
        let mut raw = unsafe { userfaultfd(base) };
        if raw < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
            // SAFETY: as above.
            raw = unsafe { userfaultfd(base | UFFD_USER_MODE_ONLY) };
            restricted = true;
        }
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh fd owned by nobody else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        // The handshake. This must be the first ioctl on the fd; anything else before it fails with
        // EINVAL. `features: 0` asks for nothing, which makes the call a pure query - the kernel
        // writes back what it supports. Requesting a feature the kernel lacks fails the whole
        // ioctl, so "ask for nothing, read what you get" is the only safe way to discover.
        let mut api = uffdio_api { api: UFFD_API, features: 0, ioctls: 0 };
        // SAFETY: `api` is a correctly sized, fully initialised structure of the type encoded in
        // the request number.
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), UFFDIO_API as _, &mut api) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok((Uffd { fd, supported_ioctls: api.ioctls }, restricted))
    }

    pub fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Give up ownership of the fd.
    ///
    /// Exists so `toy-uffd-crates` can hand an already-handshaked fd to the `userfaultfd` crate on
    /// a machine where that crate declines to open one itself. See that crate's module comment.
    pub fn into_raw_fd(self) -> RawFd {
        use std::os::fd::IntoRawFd;
        self.fd.into_raw_fd()
    }

    /// Ask the kernel to report *missing-page* faults for `[start, start+len)`.
    ///
    /// From this call onward, any thread touching an unbacked page in that range is parked and a
    /// message appears on the fd. Note that registration does not evict anything: pages already
    /// present stay present and never fault.
    pub fn register_missing(&self, start: *mut u8, len: usize) -> io::Result<u64> {
        let mut reg = uffdio_register {
            range: uffdio_range { start: start as u64, len: len as u64 },
            mode: UFFDIO_REGISTER_MODE_MISSING,
            ioctls: 0,
        };
        // SAFETY: correctly sized structure; the range is a live mapping owned by the caller.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), UFFDIO_REGISTER as _, &mut reg) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(reg.ioctls)
    }

    /// Wait for a fault message, up to `timeout_ms`. `Ok(None)` means the timeout expired.
    ///
    /// The fd is non-blocking and this polls first, which is one extra syscall per fault compared
    /// with a blocking `read()`. That is deliberate: it is the shape every real VMM uses, because a
    /// handler multiplexes the userfaultfd with a shutdown pipe and its own control channel. The
    /// cost is stated in the README rather than optimised away.
    pub fn read_event(&self, timeout_ms: i32) -> io::Result<Option<uffd_msg>> {
        let mut pfd = libc::pollfd { fd: self.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if n < 0 {
            let e = io::Error::last_os_error();
            // EINTR is not an error; the caller loops.
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(None);
            }
            return Err(e);
        }
        if n == 0 {
            return Ok(None);
        }

        let mut msg = uffd_msg::default();
        // SAFETY: reading exactly one message-sized buffer into a correctly sized struct.
        let got = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                (&raw mut msg).cast::<libc::c_void>(),
                size_of::<uffd_msg>(),
            )
        };
        if got < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EAGAIN) {
                // Another handler thread got there first. Normal, not an error.
                return Ok(None);
            }
            return Err(e);
        }
        if got as usize != size_of::<uffd_msg>() {
            return Err(io::Error::other(format!("short read from userfaultfd: {got} bytes")));
        }
        Ok(Some(msg))
    }

    /// Read a fault message by spinning on the non-blocking fd instead of parking in `poll`.
    ///
    /// This exists to answer one question: how much of a cross-CPU fault's cost is the *wakeup* -
    /// the inter-processor interrupt plus, on an idling core, the exit from a C-state - rather than
    /// the fault handling itself? A handler that never sleeps never enters an idle state, so the
    /// difference between this and [`read_event`](Self::read_event) isolates that cost.
    ///
    /// It burns a whole core. No production handler does this on a general-purpose host, and every
    /// production handler on a latency-critical one considers it.
    pub fn read_event_spin(&self, stop: &std::sync::atomic::AtomicBool) -> io::Result<Option<uffd_msg>> {
        loop {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(None);
            }
            let mut msg = uffd_msg::default();
            // SAFETY: reading exactly one message-sized buffer into a correctly sized struct.
            let got = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    (&raw mut msg).cast::<libc::c_void>(),
                    size_of::<uffd_msg>(),
                )
            };
            if got as usize == size_of::<uffd_msg>() {
                return Ok(Some(msg));
            }
            if got < 0 {
                let e = io::Error::last_os_error();
                match e.raw_os_error() {
                    // Nothing yet. Hint to the core that this is a spin loop, so an SMT sibling
                    // gets the execution resources instead of losing them to a tight poll.
                    Some(libc::EAGAIN) => std::hint::spin_loop(),
                    Some(libc::EINTR) => {}
                    _ => return Err(e),
                }
            }
        }
    }

    /// Install `len` bytes from `src` at `dst`, resolving whatever faults that covers.
    ///
    /// Returns the bytes copied.
    ///
    /// `-EEXIST` in the out-parameter is translated to `Ok(0)` rather than an error: it means
    /// another thread installed the page first, which is normal with several handler threads and
    /// must not abort the handler. Every other negative value is a real error.
    ///
    /// # Safety
    /// `dst` must be page-aligned and inside a registered range; `src` must be page-aligned and
    /// readable for `len` bytes.
    pub unsafe fn copy(&self, dst: *mut u8, src: *const u8, len: usize, wake: bool) -> io::Result<usize> {
        let mut c = uffdio_copy {
            dst: dst as u64,
            src: src as u64,
            len: len as u64,
            mode: if wake { 0 } else { UFFDIO_COPY_MODE_DONTWAKE },
            copy: 0,
        };
        // SAFETY: delegated to the caller by the contract above.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), UFFDIO_COPY as _, &mut c) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EEXIST) {
                return Ok(0);
            }
            return Err(e);
        }
        if c.copy < 0 {
            // The ioctl succeeded and reported an error in the out-parameter. Missing this is a
            // classic userfaultfd bug: the handler believes it installed a page, does not wake
            // anyone, and the faulting thread hangs forever.
            if c.copy == -(libc::EEXIST as i64) {
                return Ok(0);
            }
            return Err(io::Error::from_raw_os_error(-c.copy as i32));
        }
        Ok(c.copy as usize)
    }

    /// Wake any threads parked in `[start, start+len)` without installing anything.
    ///
    /// Used to finish a batch of `UFFDIO_COPY` calls issued with `DONTWAKE`: one wakeup for the
    /// whole run instead of one per page.
    pub fn wake(&self, start: *mut u8, len: usize) -> io::Result<()> {
        let mut r = uffdio_range { start: start as u64, len: len as u64 };
        // SAFETY: correctly sized structure.
        let rc = unsafe { libc::ioctl(self.fd.as_raw_fd(), UFFDIO_WAKE as _, &mut r) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// An anonymous mapping that can be reset to "nothing here" and faulted in again.
pub struct Region {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the pointer is an owned private mapping; sending it between threads is sound as long as
// the region is not aliased mutably from two threads at once, which the callers here uphold by
// construction (the handler writes only through UFFDIO_COPY, which is a kernel operation).
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    pub fn new(pages: usize) -> io::Result<Self> {
        // SAFETY: query only.
        let sys_page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        assert_eq!(sys_page, PAGE, "this rung assumes 4 KiB pages");
        let len = pages * PAGE;
        // SAFETY: a fresh anonymous mapping.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                // MAP_PRIVATE is what a single-process demand-pager uses. A VMM whose fault handler
                // lives in a *separate process* - which is how both Cloud Hypervisor and
                // Firecracker deploy this, so the handler can be sandboxed away from the VMM -
                // needs MAP_SHARED and must pass the uffd over a unix socket with SCM_RIGHTS.
                // That choice is made at allocation time and cannot be revised later.
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Region { ptr: ptr.cast(), len })
    }

    pub fn ptr(&self) -> *mut u8 {
        self.ptr
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn pages(&self) -> usize {
        self.len / PAGE
    }

    /// Throw away every page, so the next touch faults again.
    ///
    /// `MADV_DONTNEED` on private anonymous memory is destructive: it frees the pages and the range
    /// reverts to unbacked. On a `userfaultfd`-registered range that re-arms the missing-fault
    /// notification, which is what makes a repeatable microbenchmark possible at all - otherwise
    /// each page could only be measured once per process.
    ///
    /// Worth noticing that this is also a footgun in production: `MADV_DONTNEED` on guest memory
    /// silently discards guest data.
    pub fn reset(&self) -> io::Result<()> {
        // SAFETY: the range is this mapping, in full.
        let rc = unsafe { libc::madvise(self.ptr.cast(), self.len, libc::MADV_DONTNEED) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Read one byte from page `i`, without letting the compiler elide the access.
    ///
    /// `read_volatile` is essential rather than stylistic: the value is unused, so a normal read
    /// would be deleted and the page would never be touched, and the benchmark would measure
    /// nothing while reporting a very good number.
    pub fn touch(&self, page: usize) -> u8 {
        debug_assert!(page < self.pages());
        // SAFETY: `page` is in range, and the mapping is readable for its whole length.
        unsafe { self.ptr.add(page * PAGE).read_volatile() }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: exactly what `mmap` returned, unchanged.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}
