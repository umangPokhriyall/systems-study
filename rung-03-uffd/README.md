# Rung 3 - `userfaultfd` and demand paging

A demand pager built on the raw `userfaultfd(2)` syscall: register a region, catch its page faults
in a handler thread, and decide what belongs at each address. Then the same pager on the
`userfaultfd` crate. Then a measurement of the two decisions a real pager actually has to make -
**where the handler runs** and **how many pages it installs per fault** - neither of which any VMM
currently reports.

```
cargo run -p toy-uffd-raw                  # restore 64 pages on demand, verify every byte
cargo run -p toy-uffd-crates               # the same pager on the userfaultfd crate
cargo test --workspace                     # 6 tests, including the one that matters most
cargo run --release -p toy-uffd-raw -- --bench --out results/run.csv
cargo run --release -p toy-uffd-raw -- --bench --reverse   # drift control
```

---

## 1. Learning objectives

After this rung I should be able to, without notes:

- Explain what `userfaultfd` does to a page fault, and which thread is blocked where.
- Name the ioctl sequence - `UFFDIO_API`, `UFFDIO_REGISTER`, `UFFDIO_COPY`, `UFFDIO_WAKE` - and say
  what each does to kernel state.
- Explain why `UFFDIO_COPY` reports errors in an out-parameter while the ioctl returns success, and
  what happens to a handler that misses that.
- Say what happens when a userfaultfd is closed with faults outstanding, and why that is worse than
  a hang.
- Explain why `UFFD_USER_MODE_ONLY` exists, in terms of what a userfaultfd lets an unprivileged
  process do to the kernel.
- State the cost of a demand fault on hardware I have measured, next to the kernel's own anonymous
  fault, and account for the difference.
- Explain why handler placement matters and what the measurement actually attributes it to.

**What this unlocks upstream:** Cloud Hypervisor's `memory_restore_mode` and its v53 background
prefault threads; Firecracker's UFFD snapshot support and its separate handler process; and the
restore-path fault tail that is the wedge in `OSS-ROADMAP.md` §7. This is the rung with the shortest
distance to an actual contribution.

---

## 2. Background from first principles

### 2.1 What a page fault normally is

A process touches an address whose page-table entry is not present. The CPU traps to the kernel,
which works out what should be there - a zero page for fresh anonymous memory, a page of a file, a
page swapped out - installs a PTE, and returns to the instruction, which re-executes and succeeds.

The process never learns any of this happened. That transparency is the point of virtual memory.

### 2.2 What `userfaultfd` changes

Register a range with a `userfaultfd` in **missing** mode and the kernel stops deciding. On a fault
in that range it instead:

1. **parks the faulting thread** - it is not runnable, it is not spinning, it is asleep in the
   kernel;
2. publishes a `uffd_msg` on the file descriptor saying which address and whether it was a write;
3. waits.

Some other thread - in this process or, in real deployments, in another process entirely - reads the
message, decides what belongs there, and writes it in with `UFFDIO_COPY`. The kernel installs the
page, wakes the faulting thread, and that thread resumes as though memory had always been there.

```
   FAULTING THREAD                 KERNEL                        HANDLER THREAD

   read *p  ------------------->  page not present
                                  range is uffd-registered
                                  park this thread
                                  queue a uffd_msg  ----------->  poll(uffd) returns
       (asleep)                                                   read(uffd) -> addr, flags
       (asleep)                                                   decide what belongs there
       (asleep)                   <--------------------------     ioctl(UFFDIO_COPY)
                                  install PTE, copy page
                                  wake the faulting thread
   <-------------------------- resume
   the load completes normally
```

**The mechanism is a handoff between two threads.** That single sentence explains every number in
§3: what is being measured is not really "a page fault", it is a thread wakeup with a 4 KiB copy
attached.

### 2.3 Why a VMM wants this

Rung 1 established that guest memory is an ordinary host mapping. So a VMM restoring a snapshot has
a choice:

- **Eager**: read the whole memory image off disk, then start the vCPUs. Restore time is
  proportional to guest memory size, whether or not the guest ever touches it.
- **Demand**: `mmap` the memory, register it to a `userfaultfd`, and start the vCPUs *immediately*.
  Pages arrive as the guest asks for them.

Demand restore is what makes a microVM resume in single-digit milliseconds regardless of how much
memory it was configured with. It is what Cloud Hypervisor shipped in v52 and what Firecracker's
UFFD support does.

It is not free. It trades a fast, bounded start for a **fault tail** spread across the guest's early
execution - and the guest experiences that tail as latency at unpredictable moments. §3 measures
both sides of that trade.

### 2.4 The ioctl sequence

```
   userfaultfd(O_CLOEXEC | UFFD_USER_MODE_ONLY)     -> fd
   ioctl(fd, UFFDIO_API,      &uffdio_api)          mandatory handshake, must be first
   ioctl(fd, UFFDIO_REGISTER, &uffdio_register)     start reporting faults for a range
   loop {
       poll(fd); read(fd) -> uffd_msg               which address, read or write
       ioctl(fd, UFFDIO_COPY, &uffdio_copy)         install content, wake the waiter
   }
```

Three details that are easy to get wrong and are all in the code:

- **`UFFDIO_API` is a negotiation, not a query.** Asking for a feature the kernel lacks fails the
  whole ioctl. The only safe way to *discover* is to ask for nothing and read what comes back.
- **`UFFDIO_COPY` reports errors in an out-parameter** (`copy: i64`) while the ioctl itself returns
  success. A handler that checks only the return value believes it installed a page, does not wake
  anyone, and the faulting thread waits forever.
- **`-EEXIST` in that field is normal**, not fatal: another handler thread installed the page first.

### 2.5 `UFFD_USER_MODE_ONLY`, and why this machine requires it

A `userfaultfd` lets an unprivileged process **stall a thread inside the kernel, at an address of
its choosing, for as long as it likes.** That turns a large class of hard-to-win kernel race
conditions into easy ones: park the kernel exactly between a check and a use, then take your time.
It has been the enabling primitive in a long line of exploits.

Two mitigations, and this machine has both:

- `/proc/sys/vm/unprivileged_userfaultfd = 0`. Since Linux 5.11 that means an unprivileged process
  may create a userfaultfd **only** with `UFFD_USER_MODE_ONLY`, which restricts reporting to faults
  taken in user mode. Faults the kernel takes on the process's behalf - inside a `read()` into the
  region, say - are handled normally and cannot be stalled.
- `/dev/userfaultfd` (Linux 6.1+), mode `crw------- root root`. A device node is a better access
  control than a global sysctl: an administrator can hand it to one container and not another.

So on this machine `userfaultfd(O_CLOEXEC)` returns `EPERM` and
`userfaultfd(O_CLOEXEC | UFFD_USER_MODE_ONLY)` succeeds. Both toys report which path they took.

This is not a detail of local configuration - it is the deployment reality for anyone shipping a
UFFD-based restore path, and §4 is what happens when a library does not account for it.

---

## 3. Results

**Provisional - laptop measurement.** Manifest:
[`results/env-umang-Inspiron-3501-2026-08-07.txt`](results/). Intel i5-1135G7, 4 cores / 8 threads,
one NUMA node, `powersave` with turbo, Linux 7.0.0. Release build from commit `6496417`, clean tree.
16 MiB region, 4,096 pages, 25 rounds, **102,400 samples per configuration**, three runs - and
**run 2 executed the whole sweep in reverse order** as a control for thermal or frequency drift. It
agreed with the forward runs, so configuration order was not a confound.

Per-configuration p50 agreed to within 0.3-4% across the three runs. The tail did not, as in rungs 1
and 2: p99.9 and max move by factors and are reported, not built on.

### 3.1 Handler placement - and a wrong conclusion, then the right one

p50 per-touch latency, batch = 1, in nanoseconds (run 1 / run 2 reversed / run 3):

| handler placement | handler parked in `poll` | handler spinning |
|---|---|---|
| different physical core | 5222 / 5376 / 5344 | **3543 / 3545 / 3510** |
| SMT sibling of the faulter | 6930 / 7039 / 6788 | 4348 / 4416 / 4398 |
| same logical CPU as the faulter | **3775 / 3782 / 3787** | 3589 / 3580 / 3523 |
| *(no uffd: the kernel's own anon fault)* | *465 / 466 / 464* | - |

**Read the `poll` column alone and you get the wrong answer.** It says the best place for a fault
handler is the *same logical CPU as the thread it is servicing* - which is absurd on its face, since
that CPU must now context-switch twice per fault - and that a different physical core is 38% worse.

The spinning column is the control that explains it. A spinning handler never sleeps, so it never
enters an idle state and never has to be woken. With that removed:

- **different physical core becomes the best placement** (3,510 ns), as expected;
- **same logical CPU barely changes** (3,775 -> 3,589, about 5%), because a handler there was never
  idling in the first place: the faulting thread had just blocked, so the handler was the only
  runnable thread on that CPU;
- **the cross-core penalty was ~1,700 ns of wakeup**, not ~1,700 ns of crossing cores.

Corroborating evidence, read from `/sys/devices/system/cpu/cpuN/cpuidle/*/usage` across one run: the
handler CPU entered `C1_ACPI` (1 µs exit latency) about 49,000 times against 102,400 faults, and
`C2_ACPI` (**253 µs** exit latency) about 630 times. The C2 entries are 0.6% of faults, which is
where the multi-millisecond maxima come from; the C1 entries are roughly half of them, which is
where the median difference comes from.

**So the finding is not "put the handler on the same core".** It is:

> On an otherwise idle machine, the dominant cost of a cross-core demand fault is **waking the
> handler's CPU**, not the fault handling. Whether placement helps or hurts depends entirely on
> whether the handler core is kept warm - and that is a property of the *load*, not of the topology.

**SMT sibling is the worst placement in both modes** - 4,398 ns even when spinning, ~800 ns worse
than a different core. Sharing execution resources with the thread you are trying to unblock costs
more than the L1/L2 locality gains.

### 3.2 Prefault batch size

Pages installed per `UFFDIO_COPY`, handler on a different physical core, parked in `poll`. This is
the knob Cloud Hypervisor's v53 background prefault threads turn.

| pages per fault | faults taken | p50 | p90 | p99 | p99.9 | ns/page (amortised) |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 102,400 | 5,222 | 6,047 | 9,553 | 25,283 | **5,620** |
| 2 | 51,200 | 5,221 | 6,445 | 9,618 | 30,895 | 3,262 |
| 4 | 25,600 | 48 | 7,408 | 10,593 | 25,968 | 2,056 |
| 8 | 12,800 | 46 | 9,080 | 12,389 | 34,151 | 1,438 |
| 16 | 6,400 | 46 | 60 | 16,884 | 41,793 | 1,179 |
| 64 | 1,600 | 45 | 50 | 44,607 | 68,976 | **866** |

Two things happen at once, in opposite directions:

- **Throughput improves 6.5×.** Amortised cost falls from 5,620 to 866 ns per page, because the
  per-fault handoff is paid once per 64 pages instead of once per page. At batch 64 the amortised
  cost is within **1.9×** of the kernel's own anonymous fault (493 ns) - most of the userfaultfd
  overhead has been engineered away.
- **The tail gets 4.7× worse.** p99 rises from 9,553 to 44,607 ns, because the touches that *do*
  fault now wait for a 256 KiB copy instead of a 4 KiB one.

**And the median stops meaning anything.** From batch 4 onward, p50 is 45-48 ns - the cost of a
touch that does *not* fault, because 3 touches in 4 are now hits. The distribution is bimodal: a
huge mass at ~46 ns and a small mass at multiple microseconds. Any single-number summary of that
distribution is a lie, and a mean would be the most misleading of the available lies. This is the
clearest example in the whole ladder of why the measurement standard says *distributions, not
averages*.

Concretely, for a guest that eventually touches all of 1 GiB (262,144 pages):

| strategy | total | what the guest experiences |
|---|---|---|
| demand, batch 1 | 1.47 s | 262,144 stalls of ~5.2 µs, scattered through early execution |
| demand, batch 64 | 0.23 s | 4,096 stalls of ~45 µs |
| eager (kernel anon faults only) | 0.13 s | all of it up front, before the guest runs |

Which is better is not a question this measurement answers - it depends on whether the workload
needs bounded start-up or bounded per-access latency. What the measurement does is put numbers on
both columns, which is exactly what is missing upstream.

### 3.3 An honest negative

**Nothing here measures a real VMM.** The faulting thread is a loop touching pages in ascending
order, which is the friendliest possible access pattern for prefaulting: every speculatively
installed page is used. A guest's access pattern is not sequential, so the batch column is an
**upper bound** on what prefaulting buys, not an estimate of it. Exercise 6 is the version with a
realistic access pattern, and I expect it to look considerably worse.

Two smaller ones. The handler polls before reading, which is one extra syscall per fault versus a
blocking `read()` - realistic, since every real handler multiplexes the uffd with a shutdown
channel, but it is in the number. And `Instant::now()` costs ~16 ns (rung 1), which is 0.3% of a
5 µs fault and 35% of a 46 ns hit - so the batch column's p50 is the one figure here that is
materially inflated by its own measurement.

---

## 4. What was found on the way: the `userfaultfd` crate cannot open an fd here

`toy-uffd-crates` failed immediately:

```
Error: OpenDevUserfaultfd(Os { code: 13, kind: PermissionDenied })
```

`UffdBuilder::create()` prefers `/dev/userfaultfd` (Linux 6.1+) and falls back to the syscall **only
when the device does not exist**. The crate says so deliberately:

```rust
// This means, that if the device exists but the calling process does not have access rights to
// it, this will fail, i.e. we will not fall back to calling the system call.
```

On this machine the device exists and is `crw------- root root`, so the crate gives up - while the
syscall path with `UFFD_USER_MODE_ONLY` works, which is what `toy-uffd-raw` uses and proves in the
same session.

Unlike rung 2's finding this is **not a bug**; it is a documented decision. But the consequence
looks wrong, and the argument is short: the two paths are independently gated. The device is gated
by its file permissions; the syscall is gated by `vm.unprivileged_userfaultfd` plus the
`UFFD_USER_MODE_ONLY` requirement. Refusing the syscall because the device was unreadable does not
enforce the device's access control - it declines a path the kernel had already decided to allow.
The result is that a UFFD-based restore path built on this crate fails on stock Ubuntu 26.04 with a
permission error naming a device the operator never intended to use.

This matters for the target projects specifically: Firecracker's UFFD handler runs as a separate,
deliberately unprivileged process, which is exactly the configuration that hits this.

`toy-uffd-crates` works around it by creating and handshaking the fd with the rung's own raw code
and handing it over through `Uffd::from_raw_fd` - which is the shape the fallback would take:

```rust
Err(userfaultfd::Error::OpenDevUserfaultfd(e)) if e.kind() == PermissionDenied => {
    let (raw, _) = toy_uffd_raw::uffd::Uffd::new(false)?;
    Ok(unsafe { Uffd::from_raw_fd(raw.into_raw_fd()) })
}
```

Because this is a behaviour question rather than a defect, the right first move upstream is an
issue asking *why*, not a patch. Recorded in
[`../docs/OPEN-QUESTIONS.md`](../docs/OPEN-QUESTIONS.md) as Q7.

---

## 5. The property that matters most: closing a uffd does not hang, it zeroes

While fixing a bug of my own (§ `COMMON-MISTAKES.md` #2) the pager produced this:

```
page 7   -> byte 7   in 3582981 ns
page 0   -> byte 0   in   10602 ns
page 63  -> byte 0   in     538 ns   MISMATCH
page 31  -> byte 0   in     509 ns   MISMATCH
```

The handler had exited after the first fault. The expectation is that the remaining touches hang
forever, because nothing is left to service them. They did not. They **completed in ~500 ns with
zero-filled pages** - the cost and the content of an ordinary kernel anonymous fault.

Closing a userfaultfd unregisters its ranges, and the kernel resolves subsequent faults the ordinary
way.

For a VMM restoring a snapshot this is the worst possible failure mode:

- it is **silent** - no error, no signal, no hang;
- it is **fast** - faster than a correct restore, so nothing looks wrong;
- and the guest gets **zeroed memory** where its saved state should be, which is corruption that
  will surface much later and somewhere else entirely.

A handler process that panics, is OOM-killed, or mistakes `EAGAIN` for end-of-file produces exactly
this. The only signal available is the fault count, which is why both toys assert on it and why
`tests/restore.rs::closing_the_uffd_resolves_faults_with_zero_pages_instead_of_hanging` exists.

---

## 6. Relation to Cloud Hypervisor, Firecracker and rust-vmm

| This rung | Upstream |
|---|---|
| `uffd_sys.rs` - the syscall, computed ioctl numbers, `#[repr(C)]` structs | `userfaultfd-sys`, generated from kernel headers |
| `Uffd` - handshake, register, read, copy, wake | the `userfaultfd` crate; Firecracker's handler process |
| The handler loop | Cloud Hypervisor's `memory_restore_mode`; Firecracker's UFFD example handler |
| `batch_pages` | Cloud Hypervisor v53's background prefault threads, and their thread-count knob |
| Handler placement | An operator decision in both projects, measured by neither |
| `Region::reset` (`MADV_DONTNEED`) | The same call that discards guest memory in a balloon driver |
| The zero-page-on-close behaviour | A failure mode both projects' handler processes are exposed to |

**The gap this rung was chosen to sit in:** Cloud Hypervisor's `performance-metrics` suite reports
`restore_latency_time_ms` and nothing about what happens after the restore returns. Firecracker has
both `test_restore_latency` and `test_post_restore_latency`, so it measures that the tail exists -
but neither project reports it as a distribution, and neither reports what placement or batch size
does to it. §3 is the shape of that missing metric, produced on a laptop for free.

---

## 7. References

- [`userfaultfd(2)`](https://man7.org/linux/man-pages/man2/userfaultfd.2.html) and
  [`ioctl_userfaultfd(2)`](https://man7.org/linux/man-pages/man2/ioctl_userfaultfd.2.html) - the
  normative description of everything in `uffd_sys.rs`.
- `Documentation/admin-guide/mm/userfaultfd.rst` in the kernel tree - the design rationale,
  including why `UFFD_USER_MODE_ONLY` exists.
- `tools/testing/selftests/mm/uffd-*.c` - the kernel's own tests, and the closest thing to a
  reference handler.
- The [`userfaultfd`](https://docs.rs/userfaultfd) crate, and `userfaultfd-sys` beneath it.
- Cloud Hypervisor release notes v52.0 and v53.0 - lazy restore, then prefault threads.
- Rung 1's [`README.md`](../rung-01-kvm/README.md) for what guest memory is, and rung 2's for the
  measurement discipline this rung leans on hardest.

---

## 8. The rest of this rung

- [`CODE_WALKTHROUGH.md`](CODE_WALKTHROUGH.md) - the code in execution order, every syscall and
  ioctl explained.
- [`EXERCISES.md`](EXERCISES.md) - modifications to implement, easy to hard.
- [`GATE.md`](GATE.md) - the comprehension gate.
- [`COMMON-MISTAKES.md`](COMMON-MISTAKES.md) - misconceptions, including the two this rung hit.
