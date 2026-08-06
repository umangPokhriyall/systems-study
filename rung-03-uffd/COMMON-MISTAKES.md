# Common mistakes

Misconceptions, each with why it is wrong and what it looks like when it bites. The first two are
not hypothetical - they are what this rung actually hit.

---

## 1. "The `userfaultfd` syscall just works"

**What happened.** The very first thing this rung did was fail:

```
plain            fd= -1  errno= 1   (EPERM)
USER_MODE_ONLY   fd=  4  errno= 0
```

`/proc/sys/vm/unprivileged_userfaultfd` is **0** on stock Ubuntu, which since Linux 5.11 means an
unprivileged process may create a userfaultfd only with `UFFD_USER_MODE_ONLY`.

Then the crate-based version failed differently:

```
Error: OpenDevUserfaultfd(Os { code: 13, kind: PermissionDenied })
```

because `UffdBuilder` prefers `/dev/userfaultfd` (Linux 6.1+), which is `crw------- root root` here,
and by an explicit decision does not fall back to the syscall.

**Why it matters beyond this laptop.** Both gates exist because a userfaultfd lets an unprivileged
process **stall a thread inside the kernel, at an address of its choosing, for as long as it
likes** - which converts a large class of hard-to-win kernel races into easy ones. Distributions
lock it down by default, and any VMM shipping a UFFD restore path meets this in the field. It is
configuration, not an accident, and code that treats "the syscall worked on my machine" as the
normal case will fail in exactly the deployments the feature is for.

**The lesson.** When a syscall has a sysctl and a device node gating it, find out which one you are
subject to *before* building on it, and report which path the code took.

## 2. "If nothing services the faults, the process hangs"

**What happened.** Mid-rung, `toy-uffd-crates` produced this:

```
page 7   -> byte 7   in 3582981 ns
page 0   -> byte 0   in   10602 ns
page 63  -> byte 0   in     538 ns   MISMATCH
page 31  -> byte 0   in     509 ns   MISMATCH
```

The cause was mine: the fallback path created the fd with `O_NONBLOCK` while the handler was written
to block in `read()`. It saw `EAGAIN`, my `Ok(None)` arm treated it as end-of-file, and the handler
returned - dropping the last reference to the uffd.

**What I expected next:** every subsequent touch hangs forever, because nothing is left to service
it. Loud, obvious, easy to debug.

**What actually happens:** closing a userfaultfd unregisters its ranges, and the kernel resolves
subsequent faults the ordinary way. The touches completed **in ~500 ns with zero-filled pages** -
the cost and the content of a normal anonymous fault.

**Why this is the worst failure mode in the rung.** For a VMM restoring a snapshot it is silent (no
error, no signal), *fast* (faster than a correct restore, so nothing looks wrong), and it hands the
guest zeroed memory where its saved state should be. The corruption surfaces much later and
somewhere else. A handler process that panics, is OOM-killed, or makes exactly the `EAGAIN` mistake
I made produces this.

The only available signal is the **fault count**, which is why both toys assert on it and why there
is a test named
`closing_the_uffd_resolves_faults_with_zero_pages_instead_of_hanging`.

The blocking flag is now a parameter of `Uffd::new` rather than a constant, with the reason in the
doc comment, because the mismatch is invisible at the call site.

## 3. "`UFFDIO_COPY` returning 0 means it worked"

It reports errors in the `copy` field - a signed `i64` out-parameter - **while the ioctl itself
returns success**. A handler that checks only the return value believes it installed a page, wakes
nobody, and the faulting thread waits forever.

The related half: `-EEXIST` in that field is *normal*, not fatal. It means another handler thread
installed the page first. A handler that treats it as an error dies the first time two faults on the
same page race, which is precisely the condition a multi-threaded handler exists to create.

## 4. "The `UFFDIO_API` handshake is a capability query"

It is a negotiation. `features` is an **in** field as well as an out field, and asking for a feature
the kernel lacks fails the whole ioctl, taking the fd's setup with it.

Discovery therefore has to be: ask for nothing, read what comes back, and if you need something
specific, re-do the handshake on a *fresh* fd asking for it. The `userfaultfd` crate's
`require_features` makes this explicit, which is a real improvement over doing it by hand.

## 5. "Registering a region makes its pages fault"

Registration changes what happens to faults; it does not create any. Pages already present stay
present and are never reported.

This is why the benchmark calls `MADV_DONTNEED` before every round, and why a VMM that registers
guest memory *after* touching some of it will find those pages silently exempt from its pager.

## 6. "`MADV_DONTNEED` is a hint"

The name suggests advice. On private anonymous memory it is **destructive**: the pages are freed and
their contents are gone.

That is exactly what makes it useful here - it re-arms the fault - and exactly what makes it
dangerous in a VMM, where the same call on guest memory silently discards guest data. It is how a
balloon driver returns memory to the host, which is a feature until it is aimed at the wrong range.

## 7. "The handler should run as far from the faulting thread as possible"

Measured naively, the opposite looked true: with handlers parked in `poll`, the *same logical CPU*
was the fastest placement and a different physical core was 38% worse.

Both readings are wrong. The real finding is that on an idle machine the dominant cost of a
cross-core fault is **waking the handler's CPU** - roughly 1,700 ns of it, mostly C-state exit - not
crossing cores. With a spinning handler that never sleeps, a different physical core becomes the
best placement, as the naive intuition predicted, and the same-CPU configuration barely changes
because it was never idling.

So placement advice without a statement about *load* is not advice. See README §3.1.

## 8. "A benchmark's median describes the benchmark"

At batch sizes of 4 and above, p50 is 46 ns and p99 is 10,000-45,000 ns. The distribution is
bimodal: three touches in four are hits costing ~46 ns, and the fourth waits for a multi-page copy.

There is no single number that describes that, and a mean would be the most misleading of the
available options - it would land in the gap between the two modes, describing a latency that never
occurs. This is the clearest case in the whole ladder for the measurement standard's rule about
distributions.

## 9. "Prefaulting is free throughput"

It improves amortised cost 6.5× and makes the tail 4.7× worse. Both, at the same time, from the same
change.

The related mistake is believing this rung's batch numbers as an estimate. The benchmark walks pages
in ascending order, so *every* speculatively installed page is used - the friendliest possible case.
A guest does not do that, so the column is an **upper bound**. Exercise 6 is the honest version.

## 10. "Demand paging makes restore fast"

It makes restore *start* fast. The work does not disappear; it moves into the guest's early
execution as a tail of stalls at unpredictable moments.

Whether that is better depends on whether the workload needs bounded start-up or bounded per-access
latency, and that is a question about the workload, not about the mechanism. A VMM that reports only
`restore_latency_time_ms` is reporting the half of the trade that its design improves.

## 11. "The first fault in the trace is the fault cost"

It is ~3.5 ms in the demo against ~6 µs for the rest. That is thread startup: the handler has not
been scheduled yet when the first fault arrives.

Real restore paths have the same shape, which is why they warm the handler before starting vCPUs. In
a measurement it is a warm-up sample and must not be in the distribution; in production it is a real
cost paid once.

## 12. "It works, so the flags are right"

The `O_NONBLOCK` mismatch in #2 produced a program that ran, printed plausible timings, restored the
first page correctly, and was completely wrong. The content assertion caught it; the timings did
not - the wrong answers were *faster*.

An assertion on the observable output is the only thing that separates a working pager from one that
has quietly stopped being a pager.
