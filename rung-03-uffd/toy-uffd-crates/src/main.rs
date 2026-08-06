//! The same demand pager, rebuilt on the [`userfaultfd`](https://docs.rs/userfaultfd) crate.
//!
//! It restores the same 64 pages in the same scattered order and must produce identical bytes. As
//! in rungs 1 and 2, the value is the *diff*.
//!
//! # What the crate provides
//!
//! - **`UffdBuilder`.** The syscall flags and the mandatory `UFFDIO_API` handshake, including the
//!   awkward part: features must be *negotiated*, and asking for one the kernel lacks fails the
//!   whole handshake. `require_features` makes that an explicit, checkable request instead of a
//!   silent difference between kernels.
//! - **`user_mode_only(true)`,** which on this machine is not optional -
//!   `vm.unprivileged_userfaultfd` is 0, so the plain syscall returns `EPERM`.
//! - **A decoded `Event`.** `Event::Pagefault { kind, rw, addr, .. }` instead of a `uffd_msg` whose
//!   union must be read at the right offset, with `rw` as a two-variant enum rather than a bit in a
//!   flags word.
//! - **The `copy` out-parameter convention.** `UFFDIO_COPY` reports errors in a signed field
//!   *while returning success from the ioctl*; the crate turns that into a `Result`, and it also
//!   distinguishes `Error::PartiallyCopied`, which the raw version in this rung does not.
//!
//! # What it does not provide
//!
//! Everything that makes this a *pager*: which page belongs at which address, where the image comes
//! from, how many pages to install per fault, which thread the handler runs on, and what to do when
//! the guest asks for a page that is not in the image. That is the whole body below, and it is where
//! the engineering is - §3 of the README measures two of those decisions and finds a factor of 6.5
//! between the extremes.
//!
//! One asymmetry worth noticing: the crate's `copy` takes `(src, dst, ...)` while the kernel struct
//! orders the fields `dst, src`. Both are defensible and the mismatch is exactly the kind of thing
//! that produces a working-but-backwards pager, which is why the raw version keeps the kernel's
//! order.

use std::error::Error;
use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use std::os::fd::FromRawFd;

use userfaultfd::{Event, ReadWrite, Uffd, UffdBuilder};

const PAGE: usize = 4096;
const PAGES: usize = 64;

/// Get a `Uffd`, working around the trap described in README §4.
///
/// `UffdBuilder::create()` prefers `/dev/userfaultfd` (Linux 6.1+) and, by an explicit decision in
/// the crate, does **not** fall back to the `userfaultfd(2)` syscall when the device exists but is
/// not accessible. On this machine the device is `crw------- root root` and the sysctl
/// `vm.unprivileged_userfaultfd` is 0, so:
///
/// - opening `/dev/userfaultfd` fails with `EACCES`, and the crate gives up;
/// - the syscall with `UFFD_USER_MODE_ONLY` succeeds, which is what `toy-uffd-raw` uses.
///
/// The two gates are independent, so refusing here does not enforce the device's access control -
/// it just declines a path the kernel would have allowed. This function does what a fallback would:
/// create and handshake the fd with the rung's own raw code, then hand it over.
fn open_uffd() -> Result<Uffd, Box<dyn Error>> {
    match UffdBuilder::new()
        .close_on_exec(true)
        .non_blocking(false)
        // Mandatory here. See COMMON-MISTAKES.md #1.
        .user_mode_only(true)
        .create()
    {
        Ok(u) => {
            println!("  fd obtained from /dev/userfaultfd via UffdBuilder");
            Ok(u)
        }
        Err(userfaultfd::Error::OpenDevUserfaultfd(e))
            if e.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            // `false`: this demo's handler blocks in `read_event`, so the fd must NOT be
            // O_NONBLOCK. Matching the builder's `non_blocking(false)` here is not cosmetic - see
            // COMMON-MISTAKES.md #2 for what the mismatch does.
            let (raw, restricted) = toy_uffd_raw::uffd::Uffd::new(false)?;
            println!(
                "  /dev/userfaultfd is not accessible ({e}); the crate does not fall back, so the
                   fd was created by the syscall{} and handed over with from_raw_fd",
                if restricted { " with UFFD_USER_MODE_ONLY" } else { "" }
            );
            // SAFETY: `into_raw_fd` transfers ownership of a live userfaultfd that has already
            // completed the UFFDIO_API handshake, which is exactly what `UffdBuilder::create`
            // would have returned.
            Ok(unsafe { Uffd::from_raw_fd(raw.into_raw_fd()) })
        }
        Err(e) => Err(e.into()),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    println!("== toy-uffd-crates: the same pager on the userfaultfd crate ==");
    let uffd = open_uffd()?;

    // SAFETY: a fresh anonymous mapping, not aliased anywhere else.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            PAGES * PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err("mmap failed".into());
    }
    let base = ptr as usize;

    let ioctls = uffd.register(ptr, PAGES * PAGE)?;

    println!("  registered {PAGES} pages at {ptr:p}; range supports {ioctls:?}");

    let stop = Arc::new(AtomicBool::new(false));
    let faults = Arc::new(AtomicU64::new(0));

    let handler = {
        let (stop, faults) = (stop.clone(), faults.clone());
        std::thread::spawn(move || {
            // The same page-numbered image as the raw version.
            let src: Vec<u8> =
                (0..PAGES).flat_map(|p| std::iter::repeat_n(p as u8, PAGE)).collect();
            while !stop.load(Ordering::Relaxed) {
                match uffd.read_event() {
                    Ok(Some(Event::Pagefault { rw, addr, .. })) => {
                        faults.fetch_add(1, Ordering::Relaxed);
                        let aligned = addr as usize & !(PAGE - 1);
                        let page = (aligned - base) / PAGE;
                        debug_assert_eq!(rw, ReadWrite::Read, "the demo only reads");
                        // SAFETY: `aligned` is page-aligned and inside the registered region; the
                        // source slice is in range for one page.
                        unsafe {
                            uffd.copy(
                                src.as_ptr().add(page * PAGE).cast::<c_void>(),
                                aligned as *mut c_void,
                                PAGE,
                                true,
                            )
                        }
                        .expect("UFFDIO_COPY");
                    }
                    Ok(Some(other)) => eprintln!("  unexpected event: {other:?}"),
                    // A blocking read only returns None at EOF, which happens when the last
                    // reference to the fd is dropped. Treat it as shutdown rather than spinning.
                    Ok(None) => return,
                    Err(e) => {
                        eprintln!("  handler: {e}");
                        return;
                    }
                }
            }
        })
    };

    let order = [7usize, 0, 63, 31, 7, 32, 1, 31];
    let mut ok = true;
    for &p in &order {
        let t0 = Instant::now();
        // SAFETY: `p` is in range and the mapping is readable. `read_volatile` because the value is
        // otherwise unused and a plain read would be elided - taking the page fault with it.
        let got = unsafe { (base as *const u8).add(p * PAGE).read_volatile() };
        let ns = t0.elapsed().as_nanos();
        let matched = got == p as u8;
        ok &= matched;
        println!(
            "  page {p:<3} -> byte {got:<3} in {ns:>6} ns{}",
            if matched { "" } else { "   MISMATCH" }
        );
    }

    stop.store(true, Ordering::Relaxed);
    // The handler is parked in a blocking `read_event`. Dropping the mapping does not wake it, so
    // the process would hang on join. Detaching is the honest simple answer for a demo; a real
    // handler multiplexes the uffd with a shutdown eventfd, which is what the raw version's
    // poll-with-timeout loop stands in for.
    drop(handler);

    let n = faults.load(Ordering::Relaxed);
    println!("  {} touches -> {n} faults (6 distinct pages)", order.len());
    // Checked explicitly, because the failure mode is not a hang. If the handler dies, the uffd is
    // closed and the kernel resolves every remaining fault with a zero page - fast, silent, and
    // wrong. A fault count below the number of distinct pages is the signal.
    if n != 6 {
        return Err(format!("expected 6 faults, saw {n}: did the handler exit early?").into());
    }
    // SAFETY: exactly what `mmap` returned.
    unsafe { libc::munmap(ptr, PAGES * PAGE) };

    if !ok {
        return Err("restored the wrong page".into());
    }
    println!("  output identical to toy-uffd-raw");
    Ok(())
}
