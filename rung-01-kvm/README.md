# Rung 1 - KVM from first principles

Two virtual machine monitors that boot a guest on Linux/KVM. The first uses nothing but `libc` and
hand-encoded ioctl numbers; the second is the same VMM rebuilt on `kvm-ioctls` and `vm-memory`, so
that the difference between them is visible rather than assumed.

```
cargo run -p toy-kvm-raw                  # boot, trace every exit, verify the output
cargo run -p toy-kvm-crates               # the same guest, via the rust-vmm crates
cargo test --workspace                    # correctness, including the full boot path
cargo run --release -p toy-kvm-raw -- --bench 200000 --out results/vmexit.csv
```

---

## 1. Learning objectives

After this rung I should be able to, without notes:

- Name the fd hierarchy `/dev/kvm` -> VM fd -> vCPU fd and say what each level owns.
- Explain what `KVM_SET_USER_MEMORY_REGION` actually does to the hardware, and why it means the
  host can read guest memory with a plain pointer dereference.
- Explain why a device is a *hole* in the guest physical address map rather than a registration.
- Describe the `kvm_run` shared page: why it is `mmap`'d rather than passed to an ioctl, and how a
  value produced by the VMM ends up in a guest register.
- State the cost of a userspace-handled VM exit on hardware I have measured, as a distribution.
- Explain why real mode is where a vCPU starts, and what a bootloader is escaping from.

**What this unlocks upstream:** this is the entry requirement, not a differentiator. Every patch to
Cloud Hypervisor's `vmm` crate, every Firecracker vCPU change, and every `kvm-ioctls` review assumes
this model is already in the reader's head. It is also the prerequisite for rung 2 - a virtqueue
notification is a VM exit, and its cost is the number measured here.

---

## 2. Background from first principles

### 2.1 What hardware virtualization actually is

Before VT-x and AMD-V, virtualizing x86 meant binary translation, because some privileged
instructions failed *silently* in user mode rather than trapping - you could not simply run the
guest and catch its mistakes.

Intel VT-x added a second dimension to the privilege model. The familiar rings 0-3 still exist, but
now they exist twice:

```
                    VMX root mode                 VMX non-root mode
                    ("the host")                  ("the guest")

    ring 0          Linux kernel, KVM      <-->   guest kernel
    ring 3          your VMM process              guest userspace

                              ^                          |
                              |    VM exit               |  VMLAUNCH / VMRESUME
                              +--------------------------+
```

A guest kernel runs in *ring 0 of non-root mode*. It executes privileged instructions natively and
at full speed. When it does something the host must mediate - touching an unmapped physical address,
executing `hlt`, accessing an I/O port the host asked to intercept - the hardware performs a **VM
exit**: it atomically saves the guest's register state, restores the host's, and resumes the host in
root mode at a fixed entry point.

A VM exit is the only mechanism by which a VMM ever regains control. Everything a VMM does is a
reaction to one.

### 2.2 Where the three participants sit

```
   +-------------------------------------------------------------+
   |  your VMM process (userspace, ring 3, root mode)             |
   |                                                              |
   |   guest RAM  <- an ordinary mmap in this process's address   |
   |                 space; the host can memcpy into it           |
   |   kvm_run    <- one page mmap'd from the vCPU fd, shared     |
   |                 with the kernel, no syscall to read          |
   |                                                              |
   |   loop { ioctl(vcpu_fd, KVM_RUN); dispatch(exit_reason); }   |
   +-----------------------------|--------------------------------+
                                 | ioctl
   +-----------------------------v--------------------------------+
   |  KVM (kernel, ring 0, root mode)                             |
   |   - programs EPT so guest-physical -> host-physical           |
   |   - VMRESUME into the guest                                   |
   |   - on exit: handle in-kernel if it can, else fill kvm_run   |
   |     and return from the ioctl                                 |
   +-----------------------------|--------------------------------+
                                 | VMRESUME / VM exit
   +-----------------------------v--------------------------------+
   |  the guest (non-root mode, its own rings 0-3)                |
   +-------------------------------------------------------------+
```

The important asymmetry: **not every VM exit reaches userspace.** KVM handles many of them in the
kernel and re-enters without ever returning from the ioctl - an EPT violation on a page that is
merely not yet faulted in, for example. Only exits KVM cannot or must not decide alone come back to
the VMM. So there are two costs, not one: a kernel-handled exit and a userspace-handled exit, and
they differ by roughly an order of magnitude. Section 6 measures the second.

### 2.3 Guest memory is just your memory

This is the idea most worth internalising, because so much follows from it.

`KVM_SET_USER_MEMORY_REGION` does not allocate anything. Userspace `mmap`s a region and tells KVM
"the host virtual range at `userspace_addr` *is* guest physical range `guest_phys_addr`". KVM then
programs the **Extended Page Tables** (EPT on Intel, NPT on AMD), a second-level translation the
hardware walks after the guest's own page tables:

```
   guest virtual  --[ guest's own page tables, guest owns these ]-->  guest physical
   guest physical --[ EPT, KVM owns this                       ]-->  host physical
```

Both walks happen in hardware, with no exit. The consequences:

- The host can read and write guest memory by dereferencing its own pointer. No ioctl, no copy.
  **Every virtio implementation in every VMM is built on this**: the guest puts descriptors in
  shared memory, and the host reads them directly. Rung 2 is entirely about that.
- Guest physical space is *sparse*. Any guest-physical address not covered by a region has no EPT
  entry, so touching it exits. That is what makes MMIO device emulation possible, and it is why
  this VMM's toy device requires no registration step of any kind - it is simply an address nobody
  mapped.
- Guest memory is ordinary host memory, so it can be swapped, `madvise`d, backed by a file, or
  demand-paged with `userfaultfd`. Rung 3 depends on that.

### 2.4 The guest physical map in this VMM

```
    guest physical
    0x0000  +--------------------------------+
            | real-mode interrupt vector      |  RAM. Left as zeros; the demo never
            | table, then unused              |  raises an interrupt.
    0x1000  +--------------------------------+
            | the guest program (28 bytes)    |  loaded by Vm::load
            +--------------------------------+
            | unused RAM                      |
    0x8000  +================================+  <- end of the memory region
            | toy MMIO device (4 KiB window)  |  NOT BACKED. Accesses exit.
    0x9000  +--------------------------------+
            | nothing at all                  |  Accesses here are a VMM error and
            |                                 |  are reported, not absorbed.
            +--------------------------------+
```

Guest RAM is 32 KiB and the device sits at 0x8000 for a specific reason: a real-mode segment has a
64 KiB limit that the hardware enforces regardless of how an address is encoded, so everything the
guest can reach must live below 0x10000. See [`COMMON-MISTAKES.md`](COMMON-MISTAKES.md) for what
happens when you forget that - it is the bug this rung actually hit.

### 2.5 Why the guest starts in real mode

`KVM_CREATE_VCPU` hands back a vCPU in the architectural reset state, which on x86 is 16-bit real
mode with `cs.base = 0xffff0000` and `rip = 0xfff0` - the address a physical CPU fetches its first
instruction from, where the firmware ROM is decoded. There is no paging, no GDT, no IDT.

Reaching 64-bit long mode requires building a GDT and page tables and executing a mode-switch
sequence: about 200 lines that teach nothing about KVM. Real mode is the state the hardware gives
us, so it has the least ceremony between `KVM_CREATE_VCPU` and a running instruction. A real
bootloader's entire job is to escape it.

The one liberty this VMM takes is flattening every segment to base 0. Real hardware synthesises
`base = selector << 4` in real mode and caches base/limit/permissions in registers software cannot
address. KVM exposes that hidden cache directly through `kvm_segment`, so a VMM can *construct* a
state real hardware could only arrive at. Setting base 0 with selector 0 makes a guest offset equal
a guest physical address, which removes an entire class of confusion from the rest of the exercise.
The same mechanism is what lets a VMM restore a snapshot into the middle of a running guest.

---

## 3. Execution flow

```
   Vm::new
     open("/dev/kvm", O_RDWR|O_CLOEXEC)
     KVM_GET_API_VERSION            -> must be 12
     KVM_CHECK_EXTENSION(USER_MEMORY)
     KVM_CREATE_VM                  -> vm fd (owns a guest physical address space)
     KVM_SET_TSS_ADDR               -> real-mode scratch for parts without unrestricted guest
     mmap(32 KiB, anonymous)        -> guest RAM lives in this process
     KVM_SET_USER_MEMORY_REGION     -> that mapping IS guest physical [0, 0x8000)
     KVM_CREATE_VCPU(0)             -> vcpu fd
     KVM_GET_VCPU_MMAP_SIZE         -> size of the shared page (a kernel property)
     mmap(vcpu_fd, MAP_SHARED)      -> the kvm_run communication page

   Vm::load                          memcpy the program to guest physical 0x1000

   Vm::set_real_mode_regs
     KVM_GET_SREGS / flatten segments to base 0 / KVM_SET_SREGS
     KVM_SET_REGS                    rip = 0x1000, rflags = 0x2

   Vm::run   loop {
     ioctl(KVM_RUN)                  <-- the vCPU executes until it cannot
     read kvm_run.exit_reason from the shared page
       KVM_EXIT_IO    -> payload at kvm_run + data_offset; hand to the device model
       KVM_EXIT_MMIO  -> payload inline in the union; on a read, fill it and resume
       KVM_EXIT_HLT   -> done
       anything else  -> a bug in this VMM; report it
   }
```

The demo produces exactly nine exits:

```
  exit #1   KVM_EXIT_IO   port=0x03f8 out size=1 count=1 data=[4b]  rip=0x1005
  exit #2   KVM_EXIT_IO   port=0x03f8 out size=1 count=1 data=[56]  rip=0x1008
  exit #3   KVM_EXIT_IO   port=0x03f8 out size=1 count=1 data=[4d]  rip=0x100b
  exit #4   KVM_EXIT_IO   port=0x03f8 out size=1 count=1 data=[0a]  rip=0x100e
  exit #5   KVM_EXIT_MMIO addr=0x8000 write len=1   guest stored [42]
  exit #6   KVM_EXIT_MMIO addr=0x8000 read  len=1   VMM returns [42]
  exit #7   KVM_EXIT_IO   port=0x03f8 out size=1 count=1 data=[42]  rip=0x1017
  exit #8   KVM_EXIT_IO   port=0x03f8 out size=1 count=1 data=[0a]  rip=0x101a
  exit #9   KVM_EXIT_HLT   guest halted
KVM
B
  final vCPU state: rip=0x101c (one past the hlt at 0x101b), rax=0xa
```

The `B` on the second line is the whole test. `0x42` is ASCII `B`, and it only reaches the serial
port because the VMM's MMIO *read* handler produced it and KVM placed it into the guest's `al`
during the next entry. If any part of the MMIO round trip were wrong, a different character would
print. The trailing `rip` check confirms the second thing worth noticing: the vCPU survives the
halt and remains inspectable, which is what makes snapshotting possible at all.

Two details in that trace repay attention:

- `rip` at each I/O exit is the address *of* the `out`, not past it. KVM advances `rip` lazily, on
  the next entry, through its `complete_userspace_io` callback - because for an `in` it must first
  place the value userspace supplied into the destination register.
- Exits #5 and #6 report no `rip`, because an MMIO exit is serviced by KVM's instruction emulator
  rather than by the fast path, and the interesting state is the address and the data.

---

## 4. Important kernel concepts

| Concept | What it is | Where it appears here |
|---|---|---|
| **EPT / NPT** | Hardware second-level page tables: guest-physical to host-physical, walked without an exit | Programmed by `KVM_SET_USER_MEMORY_REGION` |
| **VM exit** | Atomic hardware transition from non-root to root mode | Every `KVM_RUN` return |
| **`kvm_run`** | A page `mmap`'d from the vCPU fd, shared with the kernel | `vmm.rs`, `header()` and `union_ptr()` |
| **The exit union** | One variant per exit reason, at byte offset 32 | `kvm_sys::KVM_RUN_UNION_OFFSET`, asserted at compile time |
| **`data_offset`** | I/O payload location, relative to the **mapping**, not the union | The sharpest edge in the raw VMM |
| **`complete_userspace_io`** | KVM's callback that finishes an emulated instruction on the next entry | Why `rip` looks "behind" in the trace |
| **Unrestricted guest** | VT-x feature allowing a guest to run in real mode natively | `unrestricted_guest = Y` in the environment manifest |
| **Hidden segment cache** | Base/limit/permissions the CPU caches per segment | `kvm_segment`, and the flattening in `set_real_mode_regs` |
| **`O_CLOEXEC`** | Do not leak this fd across `exec` | Opening `/dev/kvm`; the reason Firecracker's jailer exists |

---

## 5. Memory layout, host side

```
   VMM process address space
   +----------------------------------------------------+
   |  .text / .data / heap  - ordinary process           |
   |                                                      |
   |  mmap #1: 32 KiB anonymous, PROT_READ|PROT_WRITE     |
   |     -> handed to KVM as guest physical [0, 0x8000)   |
   |     -> writable by this process AND by the guest,    |
   |        concurrently, with no synchronisation the     |
   |        hardware provides. Rung 2 is about the        |
   |        protocol that makes that safe.                |
   |                                                      |
   |  mmap #2: kvm_run, MAP_SHARED from the vCPU fd       |
   |     -> written by the kernel, read here, no syscall  |
   |     +--------------------------------------------+   |
   |     | offset  0  request_interrupt_window        |   |
   |     | offset  8  exit_reason      <- the dispatch|   |
   |     | offset 32  union: io { port, data_offset } |   |
   |     |            or mmio { phys_addr, data[8] }  |   |
   |     | offset data_offset  the I/O payload        |   |
   |     +--------------------------------------------+   |
   +----------------------------------------------------+
```

`MAP_SHARED` on the second mapping is mandatory. With `MAP_PRIVATE` the VMM would get a
copy-on-write snapshot and would read a stale `exit_reason` forever.

---

## 6. Results

**Provisional - laptop measurement.** See [`../docs/METHODOLOGY.md`](../docs/METHODOLOGY.md) for
what a laptop number may and may not be used for. Environment manifest:
[`results/env-umang-Inspiron-3501-2026-08-05.txt`](results/). Machine: Intel i5-1135G7 (Tiger Lake),
4 cores / 8 threads, one NUMA node, `powersave` governor with turbo enabled, Linux 7.0.0,
`unrestricted_guest = Y`, `perf_event_paranoid = 1`. Release build from commit `b6f78e6`, clean tree.

### Cost of one userspace-handled VM exit

Three runs of the *same* configuration, back to back. Three rather than one because the first thing
worth knowing about a measurement is how much it moves when nothing changes - that is the noise
floor, and without it no later comparison means anything.

| | run 1 | run 2 | run 3 | spread |
|---|---:|---:|---:|---:|
| timer overhead (two `Instant::now()`) | 16 | 16 | 15 | - |
| min | 1,562 | 1,545 | 1,572 | 1.7% |
| **p50** | **1,610** | **1,618** | **1,610** | **0.5%** |
| p90 | 1,748 | 1,763 | 1,625 | 8% |
| p99 | 2,316 | 2,462 | 1,774 | 39% |
| p99.9 | 4,235 | 5,768 | 3,529 | 63% |
| max | 121,206 | 232,853 | 78,755 | 3.0× |

All in nanoseconds. 200,000 samples per run. Raw samples:
[`results/vmexit-cost-umang-Inspiron-3501-2026-08-05-run{1,2,3}.csv`](results/), summarised with
[`../tools/summarise.py`](../tools/summarise.py).

### The noise floor, which is the actual result

On this machine, in this configuration:

- **p50 is trustworthy to about 1%.** Three independent runs landed on 1,610 / 1,618 / 1,610 ns.
- **p90 is trustworthy to about 10%.**
- **p99 is trustworthy to about 40%, and p99.9 to no better than a factor of 1.6.**
- **The max is noise.** It varied by 3× across runs and is one sample in 200,000.

So a change to this VMM that moved p50 by less than ~2%, or p99 by less than ~40%, would not be
detectable here no matter how many samples were collected - the variance is between runs, not within
them. Collecting more samples per run would not help; it would only make each run's own tail
estimate more confidently wrong about the next run. That is a limitation of the *machine*, and the
fix is a quiet, pinned, fixed-frequency host, not a bigger `n`.

Reporting a single run's p99 as though it were a property of the system would have been the easiest
mistake available here, and it would have been wrong by 40%.

### Reading the numbers themselves

- **Timer overhead is 1% of the median.** Stated rather than subtracted; at this ratio it changes no
  conclusion, and subtracting it would make the numbers slightly less honest.
- **p50 ≈ 1.6 µs.** At a nominal 2.4 GHz that is roughly 3,800 cycles for a round trip whose guest
  side is one single-byte instruction. Essentially none of it is work. How that splits between the
  hardware transition and Linux is open question
  [Q2](../docs/OPEN-QUESTIONS.md#q2---how-much-of-the-1600-ns-is-the-hardware-transition-and-how-much-is-linux)
  and is not claimed here.
- **The body is tight and the tail is not.** p90 sits ~8% above p50 while p99.9 is 2-3.5× it. On an
  unpinned laptop running a desktop, those tail samples are scheduler preemption and frequency
  transitions, not variance in the exit path. This is exactly the shape a mean would have flattened
  into one misleading number, and exactly why the raw samples are committed rather than a summary.
- **The maxima are reported anyway.** They are facts about the machine, not findings about KVM.
  Deleting them would be the choice that required justification.

### What this number is for

It is the denominator for every "reduce exits" argument in virtualization. If a virtqueue
notification costs ~1.6 µs on this machine, then suppressing 10,000 notifications per second saves
16 ms of CPU per second per queue - which is why virtio has `EVENT_IDX` at all, and why the
Firecracker vsock work discussed in the OSS roadmap is measured in exits rather than in lines. Rung
2 uses this number directly.

### What was not measured, and why

- **Kernel-handled exits.** Only exits that reach userspace are timed here. The in-kernel path is
  roughly an order of magnitude cheaper, and separating the two requires the `kvm:kvm_exit`
  tracepoint - which is available at `perf_event_paranoid = 1` and is listed as an exercise rather
  than done, so it is not claimed.
- **MMIO versus port I/O exit cost.** MMIO goes through KVM's instruction emulator and port I/O
  does not, so they should differ measurably. Exercise 4.
- **Anything about a server microarchitecture.** One NUMA node, mobile part, thermally limited.

---

## 7. Relation to Cloud Hypervisor, Firecracker and rust-vmm

| This rung | Upstream |
|---|---|
| `kvm_sys.rs` - hand-encoded ioctl numbers and `#[repr(C)]` structs | `kvm-bindings`, generated from kernel headers. Writing it by hand once is what makes a review comment about a wrong field offset possible. |
| `vmm.rs` `Vm::new` sequence | Cloud Hypervisor `vmm/src/vm.rs`; Firecracker `src/vmm/src/builder.rs`. Same sequence, more configuration. |
| `Vm::run`'s dispatch | Cloud Hypervisor `vmm/src/cpu.rs`; Firecracker `src/vmm/src/vstate/vcpu/`. Same `match`, more arms. |
| `Devices` - a device is a hole in the map | Cloud Hypervisor's `Bus`; Firecracker's MMIO device manager. Both are range maps dispatching to `read`/`write`. |
| `set_real_mode_regs` | Both projects' `regs.rs` / `gdt.rs`, doing the long-mode version properly. `kvm-ioctls` deliberately has nothing to say about initial vCPU state. |
| The exit-cost measurement | The unit both projects' performance work is denominated in, and the thing neither reports as a distribution. |
| `toy-kvm-crates` | `kvm-ioctls` and `vm-memory` used the way every rust-vmm consumer uses them. |

The `unsafe` on `VmFd::set_user_memory_region` is worth dwelling on: `kvm-ioctls` exists to be the
safe wrapper, and this is the call it declines to make safe, because the obligation - the host
mapping must outlive the region - is not one a type can discharge. Recognising which obligations
survive a safe wrapper is most of what memory-related VMM review consists of.

---

## 8. References

- Kernel: [`Documentation/virt/kvm/api.rst`](https://docs.kernel.org/virt/kvm/api.html) - the
  normative description of every ioctl used here.
- `include/uapi/linux/kvm.h` - the structures in `kvm_sys.rs`, transcribed from here.
- [Using the KVM API](https://lwn.net/Articles/658511/), LWN, 2015 - the canonical minimal example
  this rung's shape follows.
- Intel SDM Volume 3C, chapters 23-27 - VMX operation, VM exits, and the VMCS.
- [`kvm-ioctls`](https://github.com/rust-vmm/kvm-ioctls) and
  [`vm-memory`](https://github.com/rust-vmm/vm-memory) sources - short enough to read end to end,
  and much shorter once the raw version exists to compare against.
- Cloud Hypervisor `vmm/src/vm.rs`, Firecracker `src/vmm/src/builder.rs` - the same sequence at
  production scale.

---

## 9. The rest of this rung

- [`CODE_WALKTHROUGH.md`](CODE_WALKTHROUGH.md) - the code in execution order, every ioctl explained.
- [`EXERCISES.md`](EXERCISES.md) - modifications to implement, easy to hard.
- [`GATE.md`](GATE.md) - the comprehension gate.
- [`COMMON-MISTAKES.md`](COMMON-MISTAKES.md) - misconceptions, including the one this rung hit.
