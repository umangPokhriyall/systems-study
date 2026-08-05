# Glossary

Terms defined once, linked from the rung documents. Only terms this repository actually uses.

**Rung 1 - KVM**

- **VMX root / non-root mode** - the second privilege dimension VT-x adds. The host (including KVM)
  runs in root mode; the guest, including its own ring 0, runs in non-root mode.
- **VM exit** - the hardware transition from non-root to root mode. Atomically saves guest state and
  restores host state. The only way a VMM regains control.
- **VMCS** - Virtual Machine Control Structure. The per-vCPU hardware structure holding guest state,
  host state, and the controls that decide which guest actions cause an exit.
- **EPT / NPT** - Extended / Nested Page Tables. Hardware second-level translation from guest
  physical to host physical, walked without an exit. Programmed by KVM from the memory slots.
- **Memory slot** - one `(guest_phys_addr, size, userspace_addr)` triple installed by
  `KVM_SET_USER_MEMORY_REGION`. Guest physical space is the union of the slots; everything else is a
  hole.
- **MMIO exit** - what happens when a guest touches guest-physical memory covered by no slot. The
  mechanism behind all memory-mapped device emulation.
- **`kvm_run`** - a page `mmap`'d from the vCPU fd and shared with the kernel, carrying the exit
  reason and its operands. Read without a syscall.
- **`complete_userspace_io`** - KVM's callback that finishes an emulated instruction on the *next*
  entry, which is why `rip` at an I/O exit still points at the faulting instruction.
- **Unrestricted guest** - the VT-x feature allowing a guest to run in real mode natively. Without
  it, KVM emulates real mode inside a protected-mode task, which is what `KVM_SET_TSS_ADDR` is for.
- **Real mode** - 16-bit x86 with `base = selector << 4` addressing and a 64 KiB segment limit. The
  state a vCPU is in at reset.
- **Unreal mode** - real mode with a hidden segment limit widened past 64 KiB. Reachable on real
  hardware only via protected mode; constructible directly through `kvm_sregs`.
- **Hidden descriptor cache** - the base/limit/permission fields the CPU caches per segment and
  software cannot address directly. KVM exposes them in `kvm_segment`.

**Measurement**

- **Coordinated omission** - the error introduced when a load generator waits for a response before
  sending the next request, so the samples systematically miss the periods when the system was slow.
- **Nearest-rank quantile** - a percentile computed by indexing into sorted samples with no
  interpolation, so every reported value is a sample that actually occurred.
- **Noise floor** - the run-to-run variation of the *same* configuration measured twice. A
  difference smaller than this is not a result.
- **Environment manifest** - the machine description that must accompany every committed
  measurement. See [`METHODOLOGY.md`](METHODOLOGY.md).

**To be filled in by later rungs**

Rung 2 will add: virtqueue, descriptor chain, available ring, used ring, `EVENT_IDX`, feature
negotiation, split versus packed queues. Rung 3 will add: `userfaultfd`, `UFFDIO_COPY`, demand
paging, prefaulting, postcopy.
