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

**Rung 3 - `userfaultfd`**

- **`userfaultfd`** - a file descriptor that receives page faults. Registered over a range, it makes
  the kernel park the faulting thread and hand the decision to userspace instead of resolving the
  fault itself.
- **Missing mode** (`UFFDIO_REGISTER_MODE_MISSING`) - report faults on pages with no backing. The
  mode demand-paged restore uses.
- **WP mode** (`UFFDIO_REGISTER_MODE_WP`) - report faults on *writes to present pages*. The basis of
  dirty tracking for live migration.
- **`UFFDIO_API`** - the mandatory first ioctl. A feature *negotiation*, not a query: asking for a
  feature the kernel lacks fails the whole call.
- **`UFFDIO_COPY`** - install page content at a faulting address and wake the waiter. Reports errors
  in a signed out-parameter while the ioctl itself returns success.
- **`UFFDIO_ZEROPAGE`** - install zeroed pages. Cheaper than a copy: there is no source to read.
- **`UFFDIO_WAKE`** / **`DONTWAKE`** - install without waking, then wake a whole run at once.
- **`UFFD_USER_MODE_ONLY`** - restrict reporting to faults taken in user mode. Exists because a
  userfaultfd otherwise lets an unprivileged process stall a thread *inside the kernel* at a chosen
  address for an unbounded time.
- **`/dev/userfaultfd`** - the Linux 6.1+ device-node gate, an alternative to the
  `vm.unprivileged_userfaultfd` sysctl. Root-only by default on Ubuntu.
- **Demand paging** - starting a workload against memory that has not been read yet, and fetching
  pages as they are asked for. What makes microVM restore time independent of guest memory size.
- **Prefaulting** - installing a run of pages per fault rather than one, trading tail latency for
  amortised throughput. Cloud Hypervisor's v53 background prefault threads.
- **Fault tail** - the stalls a demand-restored guest experiences during early execution. The cost
  that eager restore pays up front and demand restore spreads out.
- **`MADV_DONTNEED`** - destructive on private anonymous memory: frees the pages and re-arms the
  missing-fault notification. The mechanism that makes a repeatable fault benchmark possible, and a
  production footgun on guest memory.
