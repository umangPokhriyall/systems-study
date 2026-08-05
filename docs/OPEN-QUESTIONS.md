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
