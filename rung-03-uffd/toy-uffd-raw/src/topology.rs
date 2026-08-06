//! CPU topology, and pinning a thread to one logical CPU.
//!
//! This exists for one experiment: **does it matter which CPU the fault handler runs on?**
//!
//! It should, and the reason is worth stating before the measurement rather than after. Servicing a
//! fault is a handoff between two threads. The faulting thread is parked; the handler wakes, does a
//! `UFFDIO_COPY`, and the faulting thread resumes. Three things vary with placement:
//!
//! - **Wakeup path.** Waking a thread on the *same* logical CPU means the waker must be descheduled
//!   first; waking one on another CPU is an inter-processor interrupt. Different costs entirely.
//! - **Cache locality.** The copied page is written by the handler and read by the faulter. On an
//!   SMT sibling they share L1 and L2; on a different physical core they share only L3.
//! - **Contention.** SMT siblings share execution resources, so a busy handler slows the faulter
//!   even when it is not holding anything.
//!
//! Cloud Hypervisor's v53 release added *background prefault threads* with a configurable count, and
//! Firecracker's UFFD handler is a separate process the operator places. Neither project reports
//! what placement costs. This is the miniature of that question, on a laptop, for free.

use std::io;

/// Pin the calling thread to exactly one logical CPU.
///
/// Pinning matters more than it looks for this measurement. Without it the scheduler is free to
/// migrate the handler onto the faulting thread's CPU or away from it *during* a run, which does not
/// add noise so much as silently average two different experiments together.
pub fn pin_to(cpu: usize) -> io::Result<()> {
    // SAFETY: zeroed `cpu_set_t` is a valid empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is a live, correctly sized cpu_set_t and `cpu` is checked below by the kernel.
    unsafe {
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
    }
    // SAFETY: pid 0 means the calling thread; the set is correctly sized.
    let rc = unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The logical CPU the calling thread is on right now.
pub fn current_cpu() -> usize {
    // SAFETY: no arguments, returns an integer.
    unsafe { libc::sched_getcpu() as usize }
}

/// Where to put the handler relative to the faulting thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Handler and faulter on the same logical CPU. Every fault forces a context switch, because
    /// there is only one CPU for two runnable threads. This is the pathological case, and it is
    /// included because it is also what happens by accident on an oversubscribed host.
    SameCpu,
    /// Handler on the SMT sibling of the faulter's core. Shares L1 and L2 with the faulter, and
    /// shares execution units with it.
    SmtSibling,
    /// Handler on a different physical core. Shares only L3 here (single socket, one L3).
    OtherCore,
}

impl Placement {
    pub fn name(self) -> &'static str {
        match self {
            Placement::SameCpu => "same logical CPU",
            Placement::SmtSibling => "SMT sibling",
            Placement::OtherCore => "different physical core",
        }
    }
}

/// The CPU numbers this machine offers for each placement, relative to a chosen faulter CPU.
#[derive(Debug, Clone, Copy)]
pub struct Topology {
    pub faulter: usize,
    pub smt_sibling: Option<usize>,
    pub other_core: Option<usize>,
}

impl Topology {
    /// Read the sibling map from sysfs and pick one CPU for each placement.
    ///
    /// Returns `None` for a placement this machine cannot offer - a machine without SMT has no
    /// sibling, and a single-core machine has no other core. Reporting the absence is better than
    /// silently substituting something else and labelling the result as though it were the thing
    /// asked for.
    pub fn detect(faulter: usize) -> Self {
        let siblings = read_list(&format!(
            "/sys/devices/system/cpu/cpu{faulter}/topology/thread_siblings_list"
        ));
        let smt_sibling = siblings.iter().copied().find(|&c| c != faulter);

        let my_core = read_first(&format!(
            "/sys/devices/system/cpu/cpu{faulter}/topology/core_id"
        ));
        let n = num_cpus();
        let other_core = (0..n).find(|&c| {
            c != faulter
                && Some(c) != smt_sibling
                && read_first(&format!("/sys/devices/system/cpu/cpu{c}/topology/core_id")) != my_core
        });

        Topology { faulter, smt_sibling, other_core }
    }

    pub fn cpu_for(&self, p: Placement) -> Option<usize> {
        match p {
            Placement::SameCpu => Some(self.faulter),
            Placement::SmtSibling => self.smt_sibling,
            Placement::OtherCore => self.other_core,
        }
    }
}

fn num_cpus() -> usize {
    // SAFETY: query only.
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n < 1 { 1 } else { n as usize }
}

/// Parse a sysfs CPU list such as `0,4` or `0-3`.
fn read_list(path: &str) -> Vec<usize> {
    let Ok(s) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for part in s.trim().split(',') {
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                out.extend(a..=b);
            }
        } else if let Ok(v) = part.trim().parse::<usize>() {
            out.push(v);
        }
    }
    out
}

fn read_first(path: &str) -> Option<usize> {
    read_list(path).first().copied()
}
