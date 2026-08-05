# Open questions

Things the code did not explain to me. Kept because a repository that only records what its author
understood is indistinguishable from one whose author understood less than it appears.

Each entry: what I do not know, why it came up, and what would answer it. Entries are closed by
editing them in place with the answer and the date, not by deletion.

---

## Q1 - How many hardware VM exits does one `KVM_RUN` return correspond to?

**Raised by:** rung 1. One `KVM_RUN` return may hide several hardware exits that KVM handled
in-kernel and re-entered from. The README's ~1,600 ns is the cost of an exit that *reached
userspace*, and I do not know the ratio.

**Would be answered by:** `perf stat -e kvm:kvm_exit,kvm:kvm_entry` against the benchmark run, and
comparing the tracepoint count with the number of `KVM_RUN` returns. `perf_event_paranoid` is 1 on
this machine, so no root is needed. This is rung 1 exercise 7.

**Status:** open. Deliberately not claimed in the README.

---

## Q2 - How much of the ~1,600 ns is the hardware transition, and how much is Linux?

**Raised by:** rung 1. The measurement is a round trip: `ioctl` entry, KVM's save/restore, `VMRESUME`,
the guest instruction, the hardware exit, KVM's handling, the return to userspace. Intel documents
VM entry/exit as low hundreds of cycles on recent parts, which would be a small fraction of the
~3,800 cycles measured. If that is right, most of the cost is software.

**Would be answered by:** a flame graph of the benchmark under `perf record -e cycles`, plus the
`kvm_entry`/`kvm_exit` tracepoint timestamps to bracket the hardware portion.

**Status:** open. It matters because it determines whether "reduce exits" or "make exit handling
cheaper" is the productive axis, which is directly relevant to the block and vsock work in the OSS
roadmap.

---

## Q3 - Why does `KVM_GET_VCPU_MMAP_SIZE` live on the `/dev/kvm` fd rather than the vCPU fd?

**Raised by:** rung 1. It is queried before any vCPU exists in some VMMs and after in others. I
understand it is a kernel-build property rather than a per-vCPU one, but not why the ABI was not
simply "one page, and grow via a capability".

**Would be answered by:** the commit history around `kvm_vcpu_mmap_size` and the LKML thread that
introduced it.

**Status:** open, low priority. Recorded because "I accepted this because it works" is exactly the
kind of gap that surfaces later as a wrong assumption.

---

## Q4 - What is the complete state of a vCPU?

**Raised by:** rung 1 exercise 11. `KVM_GET_REGS` and `KVM_GET_SREGS` are obviously not everything -
MSRs, FPU/XSAVE, the pending-interrupt bitmap, the local APIC, and any in-kernel device state also
exist. I cannot yet enumerate the full set from memory, nor say which parts matter for a guest doing
real work versus a halted one.

**Would be answered by:** doing exercise 11, then reading Firecracker's snapshot state structs, which
are the authoritative practical enumeration.

**Status:** open. Load-bearing for rung 3 and for the restore-path work in the OSS roadmap.

---

## Q5 - Should `virtio-queue`'s `DescriptorChain` report *why* a chain was rejected?

**Raised by:** rung 2. Upstream's `DescriptorChain` is `Iterator<Item = Descriptor>` and ends the
iteration on any error via `.ok()?`. That is safe and it is what callers want most of the time. It
also makes a malformed chain indistinguishable from a short one, so a device model cannot report
that a guest sent something illegal, cannot count how often it happens, and cannot make policy about
it.

This rung's `ChainIter` yields `Result` instead. I do not yet know whether upstream would consider
that an improvement or a needless API break - `Iterator<Item = Result<_>>` is more awkward at every
call site, and there may be a reason the current shape was chosen that is not in the code.

**Would be answered by:** reading the commit history and review thread around `chain.rs`, and by
checking whether Cloud Hypervisor or Firecracker currently *want* this information - if neither
counts malformed chains today, the API is not what is stopping them.

**Status:** open. A candidate contribution, but only after the evidence above, not before.

---

## Q6 - How much of the descriptor-walk cost is the dependent load?

**Raised by:** rung 2. The walk costs roughly 16 ns fixed plus 6.1 ns per descriptor. Each step is a
16-byte read, a bounds check and three flag tests, which is maybe 15 cycles - but the next
descriptor's address depends on the current one's `next` field, so the loop cannot be pipelined and
should be latency-bound rather than throughput-bound.

If that is right, a chain whose descriptors are laid out consecutively should be measurably faster
than one whose indices are scattered, because the hardware prefetcher can help in the first case and
not the second. Real chains are scattered, because indices come off a free list.

**Would be answered by:** a variant of `bench_walk` that builds chains with consecutive versus
shuffled descriptor indices, plus `perf stat -e cycles,instructions,cache-misses`.

**Status:** open. It matters because if the walk is latency-bound on a dependent chain, then
descriptor layout is a real optimization axis, and that is the kind of finding that turns into an
upstream patch rather than a note.
