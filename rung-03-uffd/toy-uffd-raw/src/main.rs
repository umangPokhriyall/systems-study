//! Demo and measurements for demand paging with `userfaultfd`.
//!
//! ```text
//! toy-uffd-raw                       restore 64 pages on demand and verify every byte
//! toy-uffd-raw --bench [--out F]     fault cost by handler placement and by copy batch size
//! toy-uffd-raw --bench --reverse     the same sweep in reverse order, as a control for drift
//! ```

use std::hint::black_box;
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use toy_uffd_raw::topology::{Placement, Topology, current_cpu, pin_to};
use toy_uffd_raw::uffd::{PAGE, Region, Uffd};
use toy_uffd_raw::uffd_sys::{UFFD_EVENT_PAGEFAULT, UFFD_PAGEFAULT_FLAG_WRITE};

/// 16 MiB. Big enough that 25 rounds give six figures of samples, small enough to stay resident on
/// a laptop with 3.5 GiB available alongside a source buffer of the same size.
const BENCH_PAGES: usize = 4096;
const BENCH_ROUNDS: usize = 25;

/// The logical CPU the faulting thread is pinned to for every configuration.
const FAULTER_CPU: usize = 0;

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut bench = false;
    let mut reverse = false;
    let mut out_path: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bench" => bench = true,
            "--reverse" => reverse = true,
            "--out" => {
                out_path = args.get(i + 1).cloned();
                i += 1;
            }
            "-h" | "--help" => {
                eprintln!("usage: toy-uffd-raw [--bench [--reverse] [--out FILE]]");
                return Ok(());
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    demo()?;
    if bench {
        eprintln!();
        bench_all(reverse, out_path.as_deref())?;
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------
// The handler
// ------------------------------------------------------------------------------------------------

/// What one handler thread did.
#[derive(Debug, Default)]
struct HandlerStats {
    faults: AtomicU64,
    pages_installed: AtomicU64,
    write_faults: AtomicU64,
}

/// Run a fault handler until told to stop.
///
/// This is the shape every demand-paging handler has, in Cloud Hypervisor, in Firecracker's
/// separate UFFD process, and in the kernel's own selftests:
///
/// ```text
///   loop {
///       poll(uffd)                 <- park until the kernel reports a fault
///       read(uffd) -> uffd_msg     <- which address, and was it a write?
///       decide what belongs there
///       UFFDIO_COPY                <- install it; the kernel wakes the faulting thread
///   }
/// ```
///
/// `batch_pages` is the interesting knob. Installing only the faulting page is the minimum work;
/// installing a run of pages ahead of it is *speculative prefaulting*, which is what Cloud
/// Hypervisor's v53 background prefault threads do. It trades a larger copy under one fault for
/// fewer faults overall, and §3.2 of the README measures that trade.
struct Handler {
    uffd: Arc<Uffd>,
    /// Host address of the registered region, so a fault address becomes a page index.
    region_start: usize,
    region_pages: usize,
    /// The "snapshot image": what belongs at each page.
    src: Arc<Vec<u8>>,
    /// Pages installed per fault. 1 is pure demand paging; more is speculative prefaulting.
    batch_pages: usize,
    /// Spin on the fd instead of parking in `poll`.
    spin: bool,
    stop: Arc<AtomicBool>,
    stats: Arc<HandlerStats>,
}

fn handler_loop(h: Handler) {
    let Handler { uffd, region_start, region_pages, src, batch_pages, spin, stop, stats } = h;
    while !stop.load(Ordering::Relaxed) {
        let ev = if spin { uffd.read_event_spin(&stop) } else { uffd.read_event(50) };
        let msg = match ev {
            Ok(Some(m)) => m,
            Ok(None) => continue, // timeout, shutdown, or another thread took it
            Err(e) => {
                eprintln!("  handler: read failed: {e}");
                return;
            }
        };
        if msg.event != UFFD_EVENT_PAGEFAULT {
            // Registration events (fork, remap, remove) only arrive if the matching feature was
            // negotiated. Reaching here means the handshake asked for something it does not handle.
            eprintln!("  handler: unexpected event {:#x}", msg.event);
            continue;
        }
        stats.faults.fetch_add(1, Ordering::Relaxed);
        if msg.pagefault_flags & UFFD_PAGEFAULT_FLAG_WRITE != 0 {
            stats.write_faults.fetch_add(1, Ordering::Relaxed);
        }

        // The kernel reports the faulting address rounded down to a page boundary. Rounding it
        // again is free insurance: an unaligned `dst` makes UFFDIO_COPY fail with EINVAL, and the
        // faulting thread then hangs forever with no error anywhere near it.
        let addr = msg.pagefault_address as usize & !(PAGE - 1);
        let page = (addr - region_start) / PAGE;

        // Install `batch_pages` starting at the faulting page, clamped to the region. Forward-only,
        // and the walk below is sequential, so every page in the run is guaranteed absent - which
        // is why no EEXIST handling is needed on this path. A random-access walk would overlap
        // previously installed runs and would need it.
        let run = batch_pages.min(region_pages - page);
        let len = run * PAGE;
        // SAFETY: `addr` is page-aligned and inside the registered region; `src` is a live buffer
        // of at least `region_pages * PAGE` bytes and the offset is in range.
        let copied = unsafe {
            uffd.copy(addr as *mut u8, src.as_ptr().add(page * PAGE), len, true)
        };
        match copied {
            Ok(n) => {
                stats.pages_installed.fetch_add((n / PAGE) as u64, Ordering::Relaxed);
            }
            Err(e) => {
                eprintln!("  handler: UFFDIO_COPY at page {page} failed: {e}");
                return;
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Demo
// ------------------------------------------------------------------------------------------------

/// A miniature snapshot restore: 64 pages of "image", faulted in on demand, every byte verified.
fn demo() -> io::Result<()> {
    eprintln!("== demo: restore 64 pages on demand ==");

    let (uffd, restricted) = Uffd::new(true)?;
    eprintln!(
        "  userfaultfd created{}",
        if restricted {
            " with UFFD_USER_MODE_ONLY (vm.unprivileged_userfaultfd is 0 on this machine)"
        } else {
            ""
        }
    );

    const PAGES: usize = 64;
    let region = Region::new(PAGES)?;

    // The "snapshot image": page N filled with byte N. Recognisable, so a wrong page is obvious
    // rather than merely wrong.
    let src: Vec<u8> = (0..PAGES).flat_map(|p| std::iter::repeat_n(p as u8, PAGE)).collect();

    let ioctls = uffd.register_missing(region.ptr(), region.len())?;
    eprintln!(
        "  registered {} pages at {:p} in MISSING mode; range supports ioctls {:#x}",
        region.pages(),
        region.ptr(),
        ioctls
    );

    let uffd = Arc::new(uffd);
    let src = Arc::new(src);
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(HandlerStats::default());

    let handler = {
        let h = Handler {
            uffd: uffd.clone(),
            region_start: region.ptr() as usize,
            region_pages: region.pages(),
            src: src.clone(),
            batch_pages: 1,
            spin: false,
            stop: stop.clone(),
            stats: stats.clone(),
        };
        std::thread::spawn(move || handler_loop(h))
    };

    // Touch in a scattered order, because a sequential walk would not distinguish "the handler
    // installed the right page" from "the handler installed pages in order".
    let order = [7usize, 0, 63, 31, 7, 32, 1, 31];
    eprintln!("\n  -- faulting --");
    for &p in &order {
        let t0 = Instant::now();
        let got = region.touch(p);
        let ns = t0.elapsed().as_nanos();
        let expect = p as u8;
        eprintln!(
            "  page {p:<3} -> byte {got:<3} (expected {expect:<3}) in {ns:>6} ns{}",
            if got == expect { "" } else { "   MISMATCH" }
        );
        if got != expect {
            stop.store(true, Ordering::Relaxed);
            let _ = handler.join();
            return Err(io::Error::other("restored the wrong page"));
        }
    }

    stop.store(true, Ordering::Relaxed);
    handler.join().map_err(|_| io::Error::other("handler panicked"))?;

    let faults = stats.faults.load(Ordering::Relaxed);
    let installed = stats.pages_installed.load(Ordering::Relaxed);
    let writes = stats.write_faults.load(Ordering::Relaxed);
    eprintln!(
        "\n  {} touches -> {faults} faults, {installed} pages installed, {writes} of them write faults",
        order.len()
    );

    // Six distinct pages among eight touches. The repeats must not fault again: once a page is
    // installed it is ordinary memory, and that is the entire point of demand paging - the cost is
    // paid once per page, not once per access.
    let distinct = {
        let mut v = order.to_vec();
        v.sort_unstable();
        v.dedup();
        v.len() as u64
    };
    if faults != distinct {
        return Err(io::Error::other(format!(
            "expected {distinct} faults for {distinct} distinct pages, got {faults}"
        )));
    }
    eprintln!(
        "  verified: {distinct} distinct pages faulted exactly once each; repeat touches did not fault"
    );
    Ok(())
}

// ------------------------------------------------------------------------------------------------
// Benchmarks
// ------------------------------------------------------------------------------------------------

struct Config {
    label: String,
    /// `None` means no userfaultfd at all - the kernel's own anonymous-page fault, as a baseline.
    handler_cpu: Option<usize>,
    batch: usize,
    /// Handler spins on the fd instead of parking in `poll`. Isolates wakeup and idle-exit cost.
    spin: bool,
}

fn bench_all(reverse: bool, out_path: Option<&str>) -> io::Result<()> {
    eprintln!("== bench: demand-fault cost ==");
    if cfg!(debug_assertions) {
        eprintln!("  WARNING: debug build. Numbers from this build are not comparable to anything.");
    }

    pin_to(FAULTER_CPU)?;
    eprintln!("  faulting thread pinned to cpu{FAULTER_CPU} (now on cpu{})", current_cpu());

    let topo = Topology::detect(FAULTER_CPU);
    eprintln!(
        "  placements available: same={:?} smt_sibling={:?} other_core={:?}",
        topo.cpu_for(Placement::SameCpu),
        topo.smt_sibling,
        topo.other_core
    );

    let mut configs = vec![Config {
        label: "no uffd (kernel anon fault)".into(),
        handler_cpu: None,
        batch: 0,
        spin: false,
    }];

    for p in [Placement::OtherCore, Placement::SmtSibling, Placement::SameCpu] {
        match topo.cpu_for(p) {
            Some(cpu) => {
                configs.push(Config {
                    label: format!("uffd batch=1, {} (poll)", p.name()),
                    handler_cpu: Some(cpu),
                    batch: 1,
                    spin: false,
                });
                configs.push(Config {
                    label: format!("uffd batch=1, {} (spin)", p.name()),
                    handler_cpu: Some(cpu),
                    batch: 1,
                    spin: true,
                });
            }
            // Say so rather than substituting something else and labelling it as the thing asked
            // for. A machine without SMT genuinely cannot answer this question.
            None => eprintln!("  skipping placement '{}': not available on this machine", p.name()),
        }
    }

    if let Some(cpu) = topo.other_core {
        for batch in [2usize, 4, 8, 16, 64] {
            configs.push(Config {
                label: format!("uffd batch={batch}, different physical core (poll)"),
                handler_cpu: Some(cpu),
                batch,
                spin: false,
            });
        }
    }

    if reverse {
        // A control, not a convenience. If configuration order mattered - thermal drift, frequency
        // ramp, page-cache state - a reversed run would disagree with a forward one systematically
        // rather than randomly, and that is visible in the committed results.
        configs.reverse();
        eprintln!("  running the sweep in REVERSE order (drift control)");
    }

    let mut rows = Vec::new();
    for cfg in &configs {
        let r = run_config(cfg)?;
        eprintln!(
            "  {:<52} faults={:<7} p50={:>6} p99={:>7} walk={:>7.1} ms",
            cfg.label,
            r.faults,
            r.stats.p50,
            r.stats.p99,
            r.walk_ns as f64 / 1e6
        );
        rows.push((cfg.label.clone(), cfg.batch, r));
    }

    eprintln!();
    eprintln!(
        "  {:<52} {:>7} {:>7} {:>7} {:>8} {:>9} {:>9}",
        "configuration", "p50", "p90", "p99", "p99.9", "max", "ns/page"
    );
    for (label, _, r) in &rows {
        eprintln!(
            "  {:<52} {:>7} {:>7} {:>7} {:>8} {:>9} {:>9.0}",
            label,
            r.stats.p50,
            r.stats.p90,
            r.stats.p99,
            r.stats.p999,
            r.stats.max,
            r.walk_ns as f64 / (BENCH_PAGES * BENCH_ROUNDS) as f64
        );
    }
    eprintln!(
        "\n  p50/p90/p99 are per-touch latencies. ns/page is the amortised cost of walking the\n  \
         whole region, which is the number a restoring guest actually experiences."
    );

    if let Some(path) = out_path {
        let mut f = io::BufWriter::new(std::fs::File::create(path)?);
        writeln!(f, "kind,index,ns")?;
        for (label, _, r) in &rows {
            let key: String = label
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            for (i, ns) in r.samples.iter().enumerate() {
                writeln!(f, "{key},{i},{ns}")?;
            }
        }
        f.flush()?;
        eprintln!("\n  raw samples written to {path}");
    }
    Ok(())
}

struct RunResult {
    samples: Vec<u64>,
    stats: Stats,
    faults: u64,
    walk_ns: u64,
}

fn run_config(cfg: &Config) -> io::Result<RunResult> {
    let region = Region::new(BENCH_PAGES)?;
    let mut samples = Vec::with_capacity(BENCH_PAGES * BENCH_ROUNDS);
    let mut walk_ns = 0u64;
    let mut faults = 0u64;

    match cfg.handler_cpu {
        // Baseline: no userfaultfd. The kernel resolves the fault itself by installing a zero page.
        // Everything else in this table is measured against this.
        None => {
            for _ in 0..BENCH_ROUNDS {
                region.reset()?;
                let t0 = Instant::now();
                for p in 0..BENCH_PAGES {
                    let t = Instant::now();
                    black_box(region.touch(p));
                    samples.push(t.elapsed().as_nanos() as u64);
                }
                walk_ns += t0.elapsed().as_nanos() as u64;
            }
        }
        Some(cpu) => {
            let (uffd, _) = Uffd::new(true)?;
            uffd.register_missing(region.ptr(), region.len())?;

            let src = Arc::new(vec![0xABu8; BENCH_PAGES * PAGE]);
            let uffd = Arc::new(uffd);
            let stop = Arc::new(AtomicBool::new(false));
            let stats = Arc::new(HandlerStats::default());

            let handler = {
                let h = Handler {
                    uffd: uffd.clone(),
                    region_start: region.ptr() as usize,
                    region_pages: region.pages(),
                    src: src.clone(),
                    batch_pages: cfg.batch,
                    spin: cfg.spin,
                    stop: stop.clone(),
                    stats: stats.clone(),
                };
                std::thread::spawn(move || {
                    // Pinning happens inside the thread: affinity is per-thread, so setting it from
                    // the parent would move the parent instead.
                    if let Err(e) = pin_to(cpu) {
                        eprintln!("  handler: could not pin to cpu{cpu}: {e}");
                    }
                    handler_loop(h)
                })
            };

            for _ in 0..BENCH_ROUNDS {
                region.reset()?;
                let t0 = Instant::now();
                for p in 0..BENCH_PAGES {
                    let t = Instant::now();
                    black_box(region.touch(p));
                    samples.push(t.elapsed().as_nanos() as u64);
                }
                walk_ns += t0.elapsed().as_nanos() as u64;
                // Every touch returned, so every fault it raised has been serviced. Resetting here
                // cannot race the handler.
            }

            stop.store(true, Ordering::Relaxed);
            handler.join().map_err(|_| io::Error::other("handler panicked"))?;
            faults = stats.faults.load(Ordering::Relaxed);
        }
    }

    let mut sorted = samples.clone();
    let stats = Stats::from(&mut sorted);
    Ok(RunResult { samples, stats, faults, walk_ns })
}

// ------------------------------------------------------------------------------------------------

struct Stats {
    p50: u64,
    p90: u64,
    p99: u64,
    p999: u64,
    max: u64,
}

impl Stats {
    fn from(v: &mut [u64]) -> Stats {
        assert!(!v.is_empty());
        v.sort_unstable();
        let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
        Stats { p50: q(0.50), p90: q(0.90), p99: q(0.99), p999: q(0.999), max: v[v.len() - 1] }
    }
}
