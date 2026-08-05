# Measurement standard

Every number committed to this repository is held to the rules below. They exist because the
easiest way to destroy the credibility of a study repository is to publish one unreproducible
number, and because the habits here are the same ones a performance patch is reviewed against
upstream.

## The five rules

1. **A number without an environment manifest is not evidence.** Every results file names the
   manifest produced in the same session. No manifest, no commit.

2. **Report the distribution, not the mean.** Commit the raw samples. Summarise with min, p50, p90,
   p99, p99.9 and max. A mean may be reported alongside, never instead. This matters more here than
   in most places: the interesting behaviour of a VM exit, a virtqueue notification, and a page
   fault all live in the tail.

3. **Measure the measurement first.** Before timing anything, time the timer. `Instant::now()` costs
   something (roughly 20-25 ns through the vDSO on this class of machine); a measurement of a 1 µs
   operation must state that overhead so a reader can judge whether it matters. Where the operation
   is short enough that it does matter, subtract it and say so.

4. **Correctness before speed, always in that order.** A benchmark of code whose output has not been
   checked measures nothing. Every measuring binary here has a correctness mode that runs first.

5. **Publish negatives.** If a change did not help, or the effect was inside the noise, that goes in
   the README with the same prominence a positive result would have had. An "expected effect not
   observed" entry is worth more than a favourable number, because it is the one a reader cannot
   assume was cherry-picked.

## The environment manifest

`tools/capture-env.sh` emits, at minimum:

- kernel version and command line, distribution
- CPU model, core and thread count, base and max frequency
- the core-to-cache topology (`lscpu -e=CPU,CORE,SOCKET,NODE,L3`)
- NUMA layout (`numactl --hardware`), or a note that there is one node
- current CPU frequency governor and turbo state
- `perf_event_paranoid`, and whether the run had `CAP_PERFMON`
- transparent hugepage setting
- toolchain versions (`rustc`, `cargo`) and the git commit of this repository
- whether the tree was dirty at the time of the run

A run made on a laptop with an active scheduler, thermal limits and a running desktop is *not*
invalid - it is a valid measurement of that machine, and for ratios and for order-of-magnitude
questions it is often sufficient. It is invalid only if the manifest does not say so. The manifest
is what turns "measured on my laptop" from an apology into a specification.

## Laptop measurements versus bare-metal measurements

This repository is written on a laptop, deliberately: every rung is designed to cost nothing. That
constrains what may be claimed.

**Trustworthy on a laptop:**
- order of magnitude (is a VM exit 100 ns or 10 µs?)
- ratios between two mechanisms measured back to back in the same session
- the *shape* of a distribution - whether a tail exists at all
- anything about correctness

**Not trustworthy on a laptop, and must be labelled as provisional:**
- absolute latency at the tail, which is contaminated by scheduler preemption, frequency scaling,
  SMT contention from unrelated work, and thermal behaviour
- anything requiring PMU counters that unprivileged `perf` cannot read
- anything that depends on NUMA placement, since there is one node
- anything claiming to characterise a *server* microarchitecture

A provisional laptop number is still worth committing. It gets marked provisional in the results
table and is re-taken on a dedicated machine before it is used in an argument that matters.

## Statistics

Sample counts, not vibes:

- fewer than 1,000 samples: report min/p50/max only, and label the run exploratory
- 1,000 to 100,000: p99 is meaningful, p99.9 is not
- above 100,000: p99.9 is meaningful

When comparing two configurations, run them **interleaved** (A, B, A, B), not sequentially, so a
thermal or frequency drift during the session cannot be mistaken for an effect. Interleaving is
cheap and it removes the single most common source of a false result on a shared machine.

Do not report a difference smaller than the run-to-run variation of the same configuration measured
twice. Measure that variation first; it is the noise floor, and it is what a p-value would have been
approximating.

A worked example is in [`../rung-01-kvm/README.md`](../rung-01-kvm/README.md#the-noise-floor-which-is-the-actual-result):
three identical runs of 200,000 samples each agreed on p50 to within 0.5% and disagreed on p99 by
39%. Collecting more samples per run would not have narrowed that, because the variance was between
runs rather than within them - which is the case whenever the noise source is the machine rather
than the measurement. Reporting one run's p99 as a property of the system would have been wrong by
40%, and nothing inside that run would have revealed it.
