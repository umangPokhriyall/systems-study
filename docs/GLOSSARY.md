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

**Rung 2 - virtio**

- **Virtqueue** - three rings in shared memory plus two doorbells. Not an object: an agreement about
  the meaning of bytes at three addresses.
- **Descriptor table** - a *pool* of `(addr, len, flags, next)` entries, 16 bytes each. Not ordered
  and not consumed in order.
- **Descriptor chain** - a linked list through the table's `next` fields, forming one logical
  request. Entirely guest-written, so entirely untrusted.
- **Available ring** - driver to device. Carries the *head index* of each chain to process, plus
  `used_event` at its end.
- **Used ring** - device to driver. Carries `(head_index, bytes_written)` per completion, plus
  `avail_event` at its end.
- **Free-running counter** - `avail.idx` and `used.idx`: total entries ever published, never reset,
  wrapping at 65,536. Slot for entry `i` is `ring[i % queue_size]`.
- **`used_event` / `avail_event`** - thresholds each side publishes for the other. A field lives in
  the ring written by whoever writes the field.
- **`EVENT_IDX`** - the negotiated feature making those thresholds meaningful. Its whole logic is
  `need_event(event, new, old) = (new - event - 1) < (new - old)`, in wrapping `u16`, used
  identically in both directions.
- **Kick / doorbell** - driver to device notification. In a real guest an MMIO or port I/O store,
  hence one VM exit, hence ~1,610 ns on this machine.
- **Split versus packed virtqueue** - split is the three-ring layout above; packed (VIRTIO 1.1)
  folds them into one array with a wrap counter, for cache reasons. Cloud Hypervisor and Firecracker
  use split for the devices this study targets.
- **Indirect descriptor** (`VIRTQ_DESC_F_INDIRECT`) - a descriptor whose buffer is itself a
  descriptor table, letting a chain exceed the queue size. Not implemented in rung 2.

**To be filled in by later rungs**

Rung 3 will add: `userfaultfd`, `UFFDIO_COPY`, demand paging, prefaulting, postcopy.
