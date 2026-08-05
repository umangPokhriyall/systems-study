//! A minimal x86-64 VMM on `/dev/kvm`, with no virtualization crates.
//!
//! Two modes:
//!
//! ```text
//! toy-kvm-raw                       boot a guest, trace every exit, verify its output
//! toy-kvm-raw --bench [N] [--out F] measure the cost of a VM exit, N samples, CSV to F
//! ```
//!
//! Guest output goes to stdout and the exit trace goes to stderr, so `2>/dev/null` leaves exactly
//! what the guest printed.

use std::io::{self, Write};

use toy_kvm_raw::device::{self, Devices};
use toy_kvm_raw::guest;
use toy_kvm_raw::vmm::{RunOptions, Vm};

/// 32 KiB. Enough for the programs, and small enough that the guest physical map is worth drawing:
/// RAM is [0, 0x8000) and the toy device begins exactly where RAM ends, which is what makes its
/// accesses exit. Deliberately kept under the 64 KiB real-mode segment limit so that everything the
/// guest can address is either RAM or the device, with no unreachable gap in between.
const GUEST_RAM: usize = 32 * 1024;

/// Default sample count. Above 100,000 makes p99.9 meaningful (`docs/METHODOLOGY.md`), and at a few
/// microseconds per exit it still completes in well under a second.
const DEFAULT_SAMPLES: u32 = 200_000;

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut bench = false;
    let mut samples = DEFAULT_SAMPLES;
    let mut out_path: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bench" => {
                bench = true;
                // Optional positional count immediately after the flag.
                if let Some(n) = args.get(i + 1).and_then(|s| s.parse::<u32>().ok()) {
                    samples = n;
                    i += 1;
                }
            }
            "--out" => {
                out_path = args.get(i + 1).cloned();
                i += 1;
            }
            "-h" | "--help" => {
                eprintln!("usage: toy-kvm-raw [--bench [N]] [--out FILE]");
                return Ok(());
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    // Rule 4 of the measurement standard: correctness first, unconditionally. A benchmark of a VMM
    // whose exits are mishandled measures the wrong thing while looking perfectly healthy.
    demo()?;

    if bench {
        eprintln!();
        run_bench(samples, out_path.as_deref())?;
    }
    Ok(())
}

/// Boot the demo guest, trace its exits, and check what it printed.
fn demo() -> io::Result<()> {
    eprintln!("== demo: boot a real-mode guest and handle its exits ==");
    eprintln!(
        "  guest RAM   [0x0000, {:#x})   program at {:#x}",
        GUEST_RAM,
        guest::LOAD_ADDR
    );
    eprintln!(
        "  toy device  [{:#x}, {:#x})  (unbacked, so accesses exit)",
        device::MMIO_BASE,
        device::MMIO_BASE + device::MMIO_LEN
    );

    let mut vm = Vm::new(GUEST_RAM)?;
    vm.load(guest::LOAD_ADDR, guest::DEMO_PROGRAM)?;
    vm.set_real_mode_regs(guest::LOAD_ADDR)?;

    let mut devices = Devices::default();
    let summary = vm.run(&mut devices, RunOptions { trace: true, timings: None })?;

    // Guest output on stdout, verbatim.
    io::stdout().write_all(&devices.serial_out)?;
    io::stdout().flush()?;

    eprintln!(
        "  {} exits: {} io, {} mmio, {} interrupted; {} unhandled accesses",
        summary.exits, summary.io_exits, summary.mmio_exits, summary.interrupted, devices.unhandled
    );

    // The vCPU survived the halt and is still inspectable. `rip` points at the byte *after* `hlt`,
    // because x86 reports the fault-free retirement of `hlt` rather than trapping on it: the
    // instruction completed, the CPU then had nothing to do, and KVM handed control back.
    let regs = vm.regs()?;
    eprintln!(
        "  final vCPU state: rip={:#x} (one past the hlt at {:#x}), rax={:#x}",
        regs.rip,
        guest::LOAD_ADDR + guest::DEMO_PROGRAM.len() as u64 - 1,
        regs.rax
    );

    if devices.serial_out != guest::DEMO_EXPECTED_OUTPUT {
        return Err(io::Error::other(format!(
            "guest output mismatch: got {:?}, expected {:?}",
            String::from_utf8_lossy(&devices.serial_out),
            String::from_utf8_lossy(guest::DEMO_EXPECTED_OUTPUT)
        )));
    }
    if devices.unhandled != 0 {
        return Err(io::Error::other(
            "guest performed accesses the device model does not describe",
        ));
    }
    eprintln!("  output verified: the MMIO read returned the byte the MMIO write latched");
    Ok(())
}

/// Measure the round-trip cost of a userspace-handled VM exit.
///
/// What is being timed is one full iteration of the VMM's main loop with the cheapest possible
/// guest and the cheapest possible handler:
///
/// ```text
///   userspace          kernel (KVM)             hardware / guest
///   ---------          ------------             ----------------
///   ioctl(KVM_RUN) --> save host state
///                      VMRESUME             --> guest executes `out dx, al`
///                                               VM exit, reason I/O
///                  <-- vmexit handler
///                      cannot handle in kernel
///                      copy exit info to kvm_run
///   <-- ioctl returns  restore host state
///   dispatch, empty handler
/// ```
///
/// This is the number that makes virtio's `EVENT_IDX` notification suppression worth implementing,
/// and it is the unit every "reduce exits" optimization in every VMM is denominated in.
fn run_bench(samples: u32, out_path: Option<&str>) -> io::Result<()> {
    eprintln!("== bench: cost of one userspace-handled VM exit ==");
    if cfg!(debug_assertions) {
        eprintln!("  WARNING: debug build. Numbers from this build are not comparable to anything.");
    }

    // Rule 3: measure the measurement first. `Instant::now()` is not free, and at a few microseconds
    // per exit its cost is small but not negligible - the reader is entitled to the ratio.
    let timer_ns = calibrate_timer(10_000);

    let mut vm = Vm::new(GUEST_RAM)?;
    let mut devices = Devices::default();
    let mut timings: Vec<u64> = Vec::with_capacity(samples as usize);
    let mut interrupted = 0u64;

    // 65,535 is the ceiling on one entry, because the guest's loop counter is `cx`. More samples
    // are collected by resetting the vCPU and re-entering, which is worth seeing: a vCPU is a
    // resumable object, and re-pointing `rip` at the program start is exactly the mechanism a
    // snapshot restore uses on a larger scale.
    let mut remaining = samples;
    let mut rounds = 0;
    while remaining > 0 {
        let chunk = remaining.min(guest::BENCH_MAX_PER_ROUND);
        let prog = guest::bench_program(chunk);
        vm.load(guest::LOAD_ADDR, &prog)?;
        vm.set_real_mode_regs(guest::LOAD_ADDR)?;

        let before = timings.len();
        let summary = vm.run(&mut devices, RunOptions { trace: false, timings: Some(&mut timings) })?;
        interrupted += summary.interrupted;

        // The final sample of each round is the `hlt`, whose exit path differs from the `out`
        // exits. Dropping it keeps the population homogeneous; leaving it in would put a handful of
        // differently-shaped samples into a distribution the reader will read as uniform.
        let taken = timings.len() - before;
        debug_assert!(taken >= 1);
        timings.truncate(timings.len() - 1);
        eprintln!("  round {rounds}: {chunk} exits requested, {taken} KVM_RUN returns observed");

        remaining -= chunk;
        rounds += 1;
    }

    if interrupted > 0 {
        eprintln!(
            "  note: {interrupted} host interruptions during the run; those samples measure the \
             host, not an exit"
        );
    }

    let stats = Stats::from(&mut timings);
    eprintln!();
    eprintln!("  timer overhead (Instant::now x2):  {timer_ns:>8} ns  (median)");
    eprintln!("  samples:                           {:>8}", stats.n);
    eprintln!("  min                                {:>8} ns", stats.min);
    eprintln!("  p50                                {:>8} ns", stats.p50);
    eprintln!("  p90                                {:>8} ns", stats.p90);
    eprintln!("  p99                                {:>8} ns", stats.p99);
    eprintln!("  p99.9                              {:>8} ns", stats.p999);
    eprintln!("  max                                {:>8} ns", stats.max);
    eprintln!(
        "  p99/p50 ratio                      {:>8.1}x",
        stats.p99 as f64 / stats.p50.max(1) as f64
    );

    if let Some(path) = out_path {
        write_csv(path, timer_ns, &timings)?;
        eprintln!("\n  raw samples written to {path}");
    } else {
        eprintln!("\n  (pass --out FILE to commit the raw samples; a summary alone is not evidence)");
    }
    Ok(())
}

/// Median cost of the two `Instant::now()` calls that bracket each measured exit.
fn calibrate_timer(iters: usize) -> u64 {
    let mut v: Vec<u64> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t0 = std::time::Instant::now();
        // Nothing between the two reads: what is left is the cost of reading the clock twice,
        // which is exactly the overhead added to every sample in the run above.
        v.push(t0.elapsed().as_nanos() as u64);
    }
    v.sort_unstable();
    v[v.len() / 2]
}

/// Percentiles over a sample set. Deliberately not a mean: the whole reason to collect 200,000
/// samples is that the tail is the interesting part, and a mean is the statistic that hides it.
struct Stats {
    n: usize,
    min: u64,
    p50: u64,
    p90: u64,
    p99: u64,
    p999: u64,
    max: u64,
}

impl Stats {
    fn from(v: &mut [u64]) -> Stats {
        assert!(!v.is_empty(), "no samples");
        v.sort_unstable();
        let q = |p: f64| -> u64 {
            let idx = ((v.len() - 1) as f64 * p).round() as usize;
            v[idx]
        };
        Stats {
            n: v.len(),
            min: v[0],
            p50: q(0.50),
            p90: q(0.90),
            p99: q(0.99),
            p999: q(0.999),
            max: v[v.len() - 1],
        }
    }
}

/// Write the raw samples, one per row. Never pre-aggregated: a summary in a file cannot be
/// re-analysed, and the point of committing results is that someone else can disagree with the
/// analysis without repeating the run.
fn write_csv(path: &str, timer_ns: u64, timings: &[u64]) -> io::Result<()> {
    let mut f = io::BufWriter::new(std::fs::File::create(path)?);
    writeln!(f, "kind,index,ns")?;
    writeln!(f, "timer_overhead_median,0,{timer_ns}")?;
    for (i, ns) in timings.iter().enumerate() {
        writeln!(f, "vmexit_io,{i},{ns}")?;
    }
    f.flush()
}
