//! End-to-end tests for the demand pager.
//!
//! Each skips cleanly if `userfaultfd` is unavailable - the syscall can be disabled entirely
//! (`vm.unprivileged_userfaultfd = 0` without `UFFD_USER_MODE_ONLY` support, a seccomp filter, a
//! container without the capability), and a study repository that fails to build in those
//! environments is less useful than one that says why it skipped.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use toy_uffd_raw::uffd::{PAGE, Region, Uffd};
use toy_uffd_raw::uffd_sys::{UFFD_EVENT_PAGEFAULT, UFFD_PAGEFAULT_FLAG_WRITE};

fn uffd_available() -> bool {
    Uffd::new(true).is_ok()
}

/// Spawn a handler that installs `batch` pages per fault from a page-numbered source image.
fn spawn_handler(
    uffd: Arc<Uffd>,
    start: usize,
    pages: usize,
    batch: usize,
    stop: Arc<AtomicBool>,
    faults: Arc<AtomicU64>,
    write_faults: Arc<AtomicU64>,
) -> std::thread::JoinHandle<()> {
    let src: Vec<u8> = (0..pages).flat_map(|p| std::iter::repeat_n(p as u8, PAGE)).collect();
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            let Ok(Some(msg)) = uffd.read_event(50) else { continue };
            assert_eq!(msg.event, UFFD_EVENT_PAGEFAULT);
            faults.fetch_add(1, Ordering::Relaxed);
            if msg.pagefault_flags & UFFD_PAGEFAULT_FLAG_WRITE != 0 {
                write_faults.fetch_add(1, Ordering::Relaxed);
            }
            let addr = msg.pagefault_address as usize & !(PAGE - 1);
            let page = (addr - start) / PAGE;
            let run = batch.min(pages - page);
            // SAFETY: page-aligned destination inside the registered region; source in range.
            unsafe { uffd.copy(addr as *mut u8, src.as_ptr().add(page * PAGE), run * PAGE, true) }
                .expect("UFFDIO_COPY");
        }
    })
}

#[test]
fn every_page_restores_its_own_content() {
    if !uffd_available() {
        eprintln!("skipping: userfaultfd unavailable");
        return;
    }
    const PAGES: usize = 64;
    let (uffd, _) = Uffd::new(true).unwrap();
    let region = Region::new(PAGES).unwrap();
    uffd.register_missing(region.ptr(), region.len()).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let faults = Arc::new(AtomicU64::new(0));
    let writes = Arc::new(AtomicU64::new(0));
    let h = spawn_handler(
        Arc::new(uffd),
        region.ptr() as usize,
        PAGES,
        1,
        stop.clone(),
        faults.clone(),
        writes.clone(),
    );

    // Scattered order: a sequential walk would not distinguish "installed the right page" from
    // "installed pages in order".
    for p in [7usize, 0, 63, 31, 32, 1, 45, 12] {
        assert_eq!(region.touch(p), p as u8, "page {p} restored the wrong content");
    }
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();

    assert_eq!(faults.load(Ordering::Relaxed), 8);
    // Every touch was a read. If this were non-zero the handler would be seeing write faults for
    // read accesses, which would mean the flag decoding is wrong.
    assert_eq!(writes.load(Ordering::Relaxed), 0);
}

#[test]
fn a_page_faults_once_no_matter_how_often_it_is_touched() {
    if !uffd_available() {
        eprintln!("skipping: userfaultfd unavailable");
        return;
    }
    const PAGES: usize = 8;
    let (uffd, _) = Uffd::new(true).unwrap();
    let region = Region::new(PAGES).unwrap();
    uffd.register_missing(region.ptr(), region.len()).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let faults = Arc::new(AtomicU64::new(0));
    let writes = Arc::new(AtomicU64::new(0));
    let h = spawn_handler(
        Arc::new(uffd),
        region.ptr() as usize,
        PAGES,
        1,
        stop.clone(),
        faults.clone(),
        writes.clone(),
    );

    for _ in 0..1000 {
        assert_eq!(region.touch(3), 3);
    }
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();

    // Once installed, the page is ordinary memory. This is the property that makes demand paging
    // pay for itself: the cost is per page, not per access.
    assert_eq!(faults.load(Ordering::Relaxed), 1);
}

#[test]
fn batching_reduces_the_fault_count_exactly() {
    if !uffd_available() {
        eprintln!("skipping: userfaultfd unavailable");
        return;
    }
    const PAGES: usize = 64;
    for batch in [1usize, 2, 4, 8, 16] {
        let (uffd, _) = Uffd::new(true).unwrap();
        let region = Region::new(PAGES).unwrap();
        uffd.register_missing(region.ptr(), region.len()).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let faults = Arc::new(AtomicU64::new(0));
        let writes = Arc::new(AtomicU64::new(0));
        let h = spawn_handler(
            Arc::new(uffd),
            region.ptr() as usize,
            PAGES,
            batch,
            stop.clone(),
            faults.clone(),
            writes.clone(),
        );

        for p in 0..PAGES {
            assert_eq!(region.touch(p), p as u8, "batch={batch} page={p}");
        }
        stop.store(true, Ordering::Relaxed);
        h.join().unwrap();

        // Deterministic for a forward sequential walk: each fault installs `batch` pages, so the
        // next `batch - 1` touches do not fault. If this number drifts, the handler is either
        // installing the wrong run or the walk is not sequential any more, and the benchmark's
        // ns/page would silently be measuring something else.
        assert_eq!(
            faults.load(Ordering::Relaxed) as usize,
            PAGES / batch,
            "batch={batch}"
        );
    }
}

#[test]
fn reset_re_arms_the_fault() {
    if !uffd_available() {
        eprintln!("skipping: userfaultfd unavailable");
        return;
    }
    const PAGES: usize = 4;
    let (uffd, _) = Uffd::new(true).unwrap();
    let region = Region::new(PAGES).unwrap();
    uffd.register_missing(region.ptr(), region.len()).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let faults = Arc::new(AtomicU64::new(0));
    let writes = Arc::new(AtomicU64::new(0));
    let h = spawn_handler(
        Arc::new(uffd),
        region.ptr() as usize,
        PAGES,
        1,
        stop.clone(),
        faults.clone(),
        writes.clone(),
    );

    // The whole benchmark depends on this: without MADV_DONTNEED re-arming the notification, each
    // page could be measured exactly once per process and there would be no distribution to report.
    for round in 1..=5u64 {
        region.reset().unwrap();
        for p in 0..PAGES {
            assert_eq!(region.touch(p), p as u8);
        }
        assert_eq!(faults.load(Ordering::Relaxed), round * PAGES as u64);
    }
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();
}

#[test]
fn a_write_fault_is_reported_as_a_write() {
    if !uffd_available() {
        eprintln!("skipping: userfaultfd unavailable");
        return;
    }
    const PAGES: usize = 4;
    let (uffd, _) = Uffd::new(true).unwrap();
    let region = Region::new(PAGES).unwrap();
    uffd.register_missing(region.ptr(), region.len()).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let faults = Arc::new(AtomicU64::new(0));
    let writes = Arc::new(AtomicU64::new(0));
    let h = spawn_handler(
        Arc::new(uffd),
        region.ptr() as usize,
        PAGES,
        1,
        stop.clone(),
        faults.clone(),
        writes.clone(),
    );

    // SAFETY: page 2 is inside the mapping, which is readable and writable.
    unsafe { region.ptr().add(2 * PAGE).write_volatile(0xEE) };
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();

    assert_eq!(faults.load(Ordering::Relaxed), 1);
    // The distinction matters to a real handler: a write fault means the page is about to be
    // dirtied, so a copy-on-write pager can skip installing the clean original.
    assert_eq!(writes.load(Ordering::Relaxed), 1);
    // SAFETY: as above.
    assert_eq!(unsafe { region.ptr().add(2 * PAGE).read_volatile() }, 0xEE);
}

#[test]
fn closing_the_uffd_resolves_faults_with_zero_pages_instead_of_hanging() {
    if !uffd_available() {
        eprintln!("skipping: userfaultfd unavailable");
        return;
    }
    // The most important behaviour in this rung, and the least intuitive.
    //
    // The expectation is that closing a userfaultfd with a registered region leaves the region
    // unserviceable, so a touch hangs forever. It does not. The kernel unregisters the range and
    // resolves every subsequent fault the ordinary way: a **zero page**, in a few hundred
    // nanoseconds.
    //
    // For a VMM restoring a snapshot that is silent corruption. A handler that dies - a panic, a
    // mistaken EAGAIN-as-EOF, an OOM kill of a separate handler process - does not produce a hung
    // guest that anyone would notice. It produces a guest whose memory is quietly zero from that
    // point on, restoring at full speed. The only signal is the fault count.
    const PAGES: usize = 4;
    let region = Region::new(PAGES).unwrap();
    {
        let (uffd, _) = Uffd::new(true).unwrap();
        uffd.register_missing(region.ptr(), region.len()).unwrap();
        // No handler is ever started, and the uffd is dropped at the end of this scope.
    }

    // Would hang here if the naive expectation were right. It does not.
    for p in 0..PAGES {
        assert_eq!(region.touch(p), 0, "expected a zero page after the uffd was closed");
    }
}
