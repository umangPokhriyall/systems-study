//! Demo and measurements for the hand-written split virtqueue.
//!
//! ```text
//! toy-virtq-raw                     one queue, three requests, every ring transition traced
//! toy-virtq-raw --hostile           feed the device malformed chains and watch it refuse them
//! toy-virtq-raw --bench [--out F]   descriptor-walk cost, and EVENT_IDX suppression
//! ```

use std::hint::black_box;
use std::io::{self, Write};
use std::time::Instant;

use toy_virtq_raw::device::Device;
use toy_virtq_raw::driver::{Buffer, Driver};
use toy_virtq_raw::layout::{VirtqLayout, VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_WRITE};
use toy_virtq_raw::mem::{GuestAddr, SharedMem};

const REGION: usize = 64 * 1024;
const QUEUE_SIZE: u16 = 8;

/// Cost of one userspace-handled VM exit on this machine, measured in rung 1 (p50 of 200,000
/// samples, three runs agreeing to 0.5%). Used only to *model* what suppressed kicks are worth.
/// It is not re-measured here and nothing below claims to have measured it again.
const RUNG1_EXIT_NS: u64 = 1610;

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut out_path: Option<String> = None;
    let mut bench = false;
    let mut hostile = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--bench" => bench = true,
            "--hostile" => hostile = true,
            "--out" => {
                out_path = args.get(i + 1).cloned();
                i += 1;
            }
            "-h" | "--help" => {
                eprintln!("usage: toy-virtq-raw [--hostile] [--bench [--out FILE]]");
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
    if hostile {
        eprintln!();
        hostile_chains();
    }
    if bench {
        eprintln!();
        bench_walk(out_path.as_deref())?;
        eprintln!();
        bench_notification(out_path.as_deref())?;
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------
// Demo
// ------------------------------------------------------------------------------------------------

/// One round trip through the queue, traced at every ring transition.
///
/// The work the "device" does is uppercasing, chosen because it makes the gather/scatter visible:
/// the request arrives split across two descriptors and the response has to land in a third.
fn demo() -> io::Result<()> {
    eprintln!("== demo: one virtqueue, three requests ==");

    let layout = VirtqLayout::new(GuestAddr(0), QUEUE_SIZE);
    eprintln!(
        "  layout   desc={} avail={} used={}  ({} bytes for {} entries)",
        layout.desc_table,
        layout.avail_ring,
        layout.used_ring,
        layout.total_size(),
        QUEUE_SIZE
    );
    eprintln!(
        "  events   used_event={} (driver writes)   avail_event={} (device writes)",
        layout.used_event(),
        layout.avail_event()
    );

    let mut mem = SharedMem::new(REGION);
    let mut driver = Driver::new(layout, REGION as u64, true);
    let mut device = Device::new(layout, true);

    // Three requests. The middle one is deliberately split across two readable descriptors, which
    // is what a real request looks like: a header the driver already had, plus a payload somewhere
    // else, never made contiguous.
    let requests: [&[&str]; 3] = [&["hello virtio"], &["scatter ", "gather"], &["third"]];

    eprintln!("\n  -- driver publishes --");
    for (n, parts) in requests.iter().enumerate() {
        let mut buffers = Vec::new();
        for part in *parts {
            let addr = driver.alloc(&mut mem, part.as_bytes()).expect("arena");
            buffers.push(Buffer { addr, len: part.len() as u32, device_writable: false });
        }
        // One device-writable buffer for the reply. Deliberately generous, so that `used.len`
        // reporting bytes-written rather than buffer-size is observable.
        let reply = driver.alloc_uninit(64).expect("arena");
        buffers.push(Buffer { addr: reply, len: 64, device_writable: true });

        let head = driver.add_chain(&mut mem, &buffers).expect("add chain");
        eprintln!(
            "  req {n}: {} descriptors, head={head}, avail.idx={}   {:?}",
            buffers.len(),
            driver.avail_idx(),
            parts
        );
    }

    let kick = driver.needs_kick(&mem).expect("kick decision");
    eprintln!(
        "  kick needed? {kick}   (avail_event={}, so one doorbell exit covers all three requests)",
        mem.load_relaxed(layout.avail_event()).unwrap()
    );

    eprintln!("\n  -- device processes --");
    let stats = device
        .process(&mut mem, |req| req.to_ascii_uppercase())
        .expect("process");
    eprintln!(
        "  {} chains, {} descriptors, {} bytes in, {} bytes out, {} errors",
        stats.chains, stats.descriptors, stats.bytes_read, stats.bytes_written, stats.errors()
    );

    let interrupt = device.needs_notification(&mem).expect("notify decision");
    eprintln!(
        "  interrupt needed? {interrupt}   (used_event={}, used.idx={})",
        mem.load_relaxed(layout.used_event()).unwrap(),
        device.next_used()
    );

    eprintln!("\n  -- driver collects --");
    let completions = driver.collect_used(&mem).expect("collect");
    let mut ok = true;
    for (n, (head, len)) in completions.iter().enumerate() {
        // The reply buffer is the last descriptor of the chain. Find it by walking the chain the
        // device just finished with - which is also the reason a driver must remember its chains.
        let reply_addr = device
            .chain(&mem, *head)
            .filter_map(|d| d.ok())
            .find(|d| d.is_write_only())
            .map(|d| d.addr)
            .expect("every chain here has one writable descriptor");
        let got = mem.read_slice(GuestAddr(reply_addr), *len as u64).expect("read reply");
        let want = requests[n].concat().to_ascii_uppercase();
        let matched = got == want.as_bytes();
        ok &= matched;
        eprintln!(
            "  used[{n}]: head={head} len={len:<3} {:?}{}",
            String::from_utf8_lossy(got),
            if matched { "" } else { "   MISMATCH" }
        );
    }

    eprintln!(
        "\n  free descriptors back to {} of {}",
        driver.free_descriptors(),
        QUEUE_SIZE
    );

    // Correctness before speed. Three assertions, each catching a different class of bug.
    if !ok {
        return Err(io::Error::other("device output did not match"));
    }
    if driver.free_descriptors() != QUEUE_SIZE as usize {
        return Err(io::Error::other(
            "descriptors leaked: the driver freed only chain heads",
        ));
    }
    if stats.errors() != 0 {
        return Err(io::Error::other("device rejected a well-formed chain"));
    }
    eprintln!("  verified: gather/scatter correct, used.len is bytes written, no descriptor leak");
    Ok(())
}

// ------------------------------------------------------------------------------------------------
// Hostile chains
// ------------------------------------------------------------------------------------------------

/// Hand the device the malformed chains a hostile guest would, and show it refusing each one.
///
/// Every one of these is a two-store attack: the "guest" writes a descriptor and an avail entry.
/// None of them requires a bug in the driver, because a hostile guest is not running the driver.
fn hostile_chains() {
    eprintln!("== hostile: chains a malicious guest would build ==");

    let layout = VirtqLayout::new(GuestAddr(0), QUEUE_SIZE);
    // One case is (name, builder). The builder writes descriptors directly rather than going
    // through the driver, because the driver would refuse to build them - which is the point.
    type Case<'a> = (&'a str, &'a dyn Fn(&mut SharedMem));

    let cases: [Case<'_>; 5] = [
        ("self-referential descriptor (desc[0].next = 0)", &|m: &mut SharedMem| {
            write_desc(m, layout, 0, 0x2000, 16, VIRTQ_DESC_F_NEXT, 0);
        }),
        ("two-descriptor cycle (0 -> 1 -> 0)", &|m: &mut SharedMem| {
            write_desc(m, layout, 0, 0x2000, 16, VIRTQ_DESC_F_NEXT, 1);
            write_desc(m, layout, 1, 0x2000, 16, VIRTQ_DESC_F_NEXT, 0);
        }),
        ("next points outside the table", &|m: &mut SharedMem| {
            write_desc(m, layout, 0, 0x2000, 16, VIRTQ_DESC_F_NEXT, 60_000);
        }),
        ("buffer outside the shared region", &|m: &mut SharedMem| {
            write_desc(m, layout, 0, u64::MAX - 8, 4096, 0, 0);
        }),
        ("device-readable after device-writable", &|m: &mut SharedMem| {
            write_desc(m, layout, 0, 0x2000, 16, VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT, 1);
            write_desc(m, layout, 1, 0x2000, 16, 0, 0);
        }),
    ];

    for (name, build) in cases {
        let mut mem = SharedMem::new(REGION);
        let mut device = Device::new(layout, false);
        build(&mut mem);
        // Publish descriptor 0 as an available chain.
        mem.write_u16(layout.avail_slot(0), 0).unwrap();
        mem.store_idx_release(layout.avail_idx(), 1).unwrap();

        let stats = device.process(&mut mem, |r| r.to_vec()).expect("no host error");
        // Every case must be rejected, and the chain must still be completed so the driver is not
        // left waiting forever for a request that vanished.
        assert_eq!(stats.chains, 1, "{name}: chain was not completed");
        let reason = match stats.rejected.first() {
            Some((_, r)) => r.to_string(),
            None => "NOT REJECTED - this is a bug".to_string(),
        };
        eprintln!("  {name:<44} -> {reason}");
    }
    eprintln!("  all rejected, all completed with length 0, host still running");
}

fn write_desc(
    m: &mut SharedMem,
    layout: VirtqLayout,
    index: u16,
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
) {
    let at = layout.desc(index).expect("index in range");
    m.write_slice(at, &addr.to_le_bytes()).unwrap();
    m.write_u32(GuestAddr(at.0 + 8), len).unwrap();
    m.write_u16(GuestAddr(at.0 + 12), flags).unwrap();
    m.write_u16(GuestAddr(at.0 + 14), next).unwrap();
}

// ------------------------------------------------------------------------------------------------
// Measurement 1: descriptor walk cost
// ------------------------------------------------------------------------------------------------

/// Time the descriptor-chain walk as a function of chain length.
///
/// This is the cost the device pays *per request* to find out what it has been asked to do, and it
/// is entirely userspace work on shared memory - no syscall, no exit. Comparing it against rung 1's
/// exit cost is the whole argument for the virtio design, so it is worth an actual number rather
/// than an assertion that it is "cheap".
fn bench_walk(out_path: Option<&str>) -> io::Result<()> {
    eprintln!("== bench 1: descriptor-chain walk cost ==");
    if cfg!(debug_assertions) {
        eprintln!("  WARNING: debug build. Numbers from this build are not comparable to anything.");
    }

    const ITERS: usize = 200_000;
    let mut rows: Vec<(usize, Vec<u64>)> = Vec::new();

    for &len in &[1usize, 2, 4, 8, 16] {
        // A queue big enough to hold a chain of this length.
        let qs = (len as u16).next_power_of_two().max(2);
        let layout = VirtqLayout::new(GuestAddr(0), qs);
        let mut mem = SharedMem::new(REGION);
        let mut driver = Driver::new(layout, REGION as u64, true);
        let device = Device::new(layout, true);

        let mut buffers = Vec::new();
        for _ in 0..len - 1 {
            let a = driver.alloc(&mut mem, &[b'x'; 64]).expect("arena");
            buffers.push(Buffer { addr: a, len: 64, device_writable: false });
        }
        let a = driver.alloc_uninit(64).expect("arena");
        buffers.push(Buffer { addr: a, len: 64, device_writable: true });
        let head = driver.add_chain(&mut mem, &buffers).expect("chain");

        let mut samples = Vec::with_capacity(ITERS);
        for _ in 0..ITERS {
            let t0 = Instant::now();
            let mut total = 0u64;
            for d in device.chain(&mem, head) {
                // `black_box` so the walk cannot be optimised away: the loop has no other effect,
                // and without it LLVM is entitled to delete the whole thing and time nothing.
                total += black_box(d.expect("well-formed").len) as u64;
            }
            black_box(total);
            samples.push(t0.elapsed().as_nanos() as u64);
        }
        rows.push((len, samples));
    }

    eprintln!("  {:>6}  {:>8}  {:>8}  {:>8}  {:>10}", "descs", "p50 ns", "p99 ns", "ns/desc", "vs 1 exit");
    for (len, samples) in &mut rows {
        let s = summarise(samples);
        eprintln!(
            "  {:>6}  {:>8}  {:>8}  {:>8.1}  {:>9.1}x",
            len,
            s.p50,
            s.p99,
            s.p50 as f64 / *len as f64,
            RUNG1_EXIT_NS as f64 / s.p50.max(1) as f64
        );
    }
    eprintln!(
        "  last column: how many chain walks fit in the cost of one VM exit ({} ns, rung 1)",
        RUNG1_EXIT_NS
    );

    if let Some(path) = out_path {
        let p = format!("{path}-walk.csv");
        let mut f = io::BufWriter::new(std::fs::File::create(&p)?);
        writeln!(f, "kind,index,ns")?;
        for (len, samples) in &rows {
            for (i, ns) in samples.iter().enumerate() {
                writeln!(f, "walk_{len}_desc,{i},{ns}")?;
            }
        }
        f.flush()?;
        eprintln!("  raw samples written to {p}");
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------
// Measurement 2: notification suppression
// ------------------------------------------------------------------------------------------------

/// Count how many doorbell kicks `EVENT_IDX` suppresses, as a function of batch size.
///
/// **This is a count, not a timing.** The protocol is deterministic, so the number of kicks for a
/// given interleaving is exact and reproducible rather than measured. What is *modelled* is the
/// time that saves, using rung 1's measured exit cost - and modelled is the right word, because no
/// VM exit was performed here.
///
/// The interleaving simulated is the one every real device sees: the driver submits a burst of
/// requests while the device is busy with the previous burst, then the device drains everything
/// available in one pass. That is not a favourable case chosen to make `EVENT_IDX` look good, it is
/// what a queue under load does - and when the queue is *not* under load, batch size is 1 and the
/// table below shows the feature saving nothing, which is the honest half of the result.
fn bench_notification(out_path: Option<&str>) -> io::Result<()> {
    eprintln!("== bench 2: EVENT_IDX notification suppression ==");

    const TOTAL: u32 = 4096;
    const MAX_BATCH: u32 = 64;
    // Two descriptors per request, and a whole burst must fit before the device drains it - the
    // driver cannot reclaim a descriptor until the completion comes back. A queue too small to hold
    // the burst does not merely perform worse, it forces a kick per request by running out of
    // descriptors, which would quietly turn this into a measurement of queue depth instead.
    const NOTIFY_QUEUE_SIZE: u16 = (2 * MAX_BATCH) as u16;
    let mut rows = Vec::new();

    for &batch in &[1u32, 2, 4, 8, 16, 32, MAX_BATCH] {
        let mut result = [(0u64, 0u64); 2]; // [(kicks, interrupts); event_idx off/on]
        for (slot, event_idx) in [false, true].into_iter().enumerate() {
            let layout = VirtqLayout::new(GuestAddr(0), NOTIFY_QUEUE_SIZE);
            let mut mem = SharedMem::new(REGION);
            let mut driver = Driver::new(layout, REGION as u64, event_idx);
            let mut device = Device::new(layout, event_idx);
            let payload = driver.alloc(&mut mem, b"x").expect("arena");
            let reply = driver.alloc_uninit(8).expect("arena");

            let (mut kicks, mut interrupts) = (0u64, 0u64);
            let mut submitted = 0u32;

            while submitted < TOTAL {
                // The driver arms its interrupt threshold before submitting, which is what a
                // driver about to wait does.
                driver.arm_used_event(&mut mem).expect("arm");

                let n = batch.min(TOTAL - submitted);
                for _ in 0..n {
                    driver
                        .add_chain(
                            &mut mem,
                            &[
                                Buffer { addr: payload, len: 1, device_writable: false },
                                Buffer { addr: reply, len: 8, device_writable: true },
                            ],
                        )
                        .expect("chain");
                    // The decision is made per submission, exactly as a real driver makes it: it
                    // does not know a burst is coming.
                    if driver.needs_kick(&mem).expect("kick") {
                        kicks += 1;
                    }
                    submitted += 1;
                }

                // Device drains everything available in one pass, then re-arms.
                device.process(&mut mem, |_| b"y".to_vec()).expect("process");
                if device.needs_notification(&mem).expect("notify") {
                    interrupts += 1;
                }
                device.enable_notification(&mut mem).expect("enable");
                driver.collect_used(&mem).expect("collect");
            }
            result[slot] = (kicks, interrupts);
        }

        let (off_k, off_i) = result[0];
        let (on_k, on_i) = result[1];
        rows.push((batch, off_k, on_k, off_i, on_i));
    }

    eprintln!("  {TOTAL} requests per configuration, queue size {NOTIFY_QUEUE_SIZE}");
    eprintln!(
        "  {:>5}  {:>10}  {:>10}  {:>9}  {:>10}  {:>10}",
        "batch", "kicks off", "kicks on", "saved", "irqs off", "irqs on"
    );
    for &(batch, off_k, on_k, off_i, on_i) in &rows {
        eprintln!(
            "  {:>5}  {:>10}  {:>10}  {:>8.0}%  {:>10}  {:>10}",
            batch,
            off_k,
            on_k,
            100.0 * (1.0 - on_k as f64 / off_k.max(1) as f64),
            off_i,
            on_i
        );
    }

    eprintln!("\n  Modelled saving on the kick path only, at {RUNG1_EXIT_NS} ns per exit (rung 1):");
    for &(batch, off_k, on_k, _, _) in &rows {
        let saved_ns = (off_k - on_k) * RUNG1_EXIT_NS;
        eprintln!(
            "    batch {:>3}: {:>5} kicks suppressed = {:>7.2} ms of guest CPU not spent exiting",
            batch,
            off_k - on_k,
            saved_ns as f64 / 1e6
        );
    }
    eprintln!(
        "  The interrupt columns are counts only. An interrupt into the guest is a different\n  \
         mechanism from a doorbell exit and its cost was not measured, so it is not priced here."
    );

    if let Some(path) = out_path {
        let p = format!("{path}-notify.csv");
        let mut f = io::BufWriter::new(std::fs::File::create(&p)?);
        writeln!(f, "batch,event_idx,kicks,interrupts,requests")?;
        for &(batch, off_k, on_k, off_i, on_i) in &rows {
            writeln!(f, "{batch},off,{off_k},{off_i},{TOTAL}")?;
            writeln!(f, "{batch},on,{on_k},{on_i},{TOTAL}")?;
        }
        f.flush()?;
        eprintln!("  counts written to {p}");
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------------

struct Summary {
    p50: u64,
    p99: u64,
}

fn summarise(v: &mut [u64]) -> Summary {
    v.sort_unstable();
    let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    Summary { p50: q(0.50), p99: q(0.99) }
}
