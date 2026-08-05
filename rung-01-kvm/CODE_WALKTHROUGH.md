# Code walkthrough

The code in execution order. Every ioctl and syscall is explained where it first appears, including
what the kernel does in response.

Files:

```
toy-kvm-raw/src/
  kvm_sys.rs   the raw ABI: ioctl numbers and the C structures
  guest.rs     the guest programs, hand-assembled
  device.rs    the device model
  vmm.rs       the VMM
  main.rs      the driver: demo mode and benchmark mode
  ../tests/boot.rs   the end-to-end test
toy-kvm-crates/src/
  main.rs      the same VMM on kvm-ioctls and vm-memory
```

---

## Part 0 - before anything runs: `kvm_sys.rs`

### The `_IOC` encoding

Linux packs four fields into a 32-bit ioctl request number:

```
 31 30 29                16 15             8 7              0
+-----+--------------------+----------------+---------------+
| dir |        size        |      type      |      nr       |
+-----+--------------------+----------------+---------------+
```

`ioc()`, `io()`, `iow()` and `ior()` compute these at compile time. Nothing is pasted as hex.

That is not stylistic. The `size` field is derived from `size_of::<T>()` of the struct being passed,
so if a structure in this file were laid out wrongly, the *request number itself* would be wrong and
the kernel would reject the call with `ENOTTY` instead of reading the wrong bytes. Pasting
`0x4020AE46` would throw that check away.

- `_IO(nr)` - no payload; the third `ioctl` argument is an integer.
- `_IOW(nr, T)` - userspace writes, kernel reads a `T`.
- `_IOR(nr, T)` - userspace supplies a buffer, kernel fills a `T`.

`KVMIO` is `0xAE`, the type byte KVM owns.

### The structures

`kvm_userspace_memory_region`, `kvm_segment`, `kvm_dtable`, `kvm_sregs`, `kvm_regs`, and the header
of `kvm_run`. All `#[repr(C)]`, all with field order taken from `include/uapi/linux/kvm.h`. **Field
order is ABI**: the kernel accesses these by offset, so a reordering is not a compile error, it is a
silently wrong VM.

The block at the bottom of the file is the safety net:

```rust
const _: () = {
    assert!(size_of::<kvm_segment>() == 24);
    assert!(size_of::<kvm_sregs>()   == 312);
    assert!(size_of::<kvm_regs>()    == 144);
    assert!(size_of::<kvm_run_header>() == KVM_RUN_UNION_OFFSET);   // 32
};
```

The last one is the load-bearing assertion. The exit-information union in `kvm_run` begins
immediately after the header; if the header were 24 or 40 bytes because of a missing field or wrong
padding, every exit would be read from the wrong offset and the failure would look like nonsense
rather than like a layout bug.

### Why the union is not a Rust `union`

`kvm_run` in C ends with a large anonymous union, one variant per exit reason. Declaring that as a
Rust `union` would be faithful but would hide the offset behind the compiler's layout rules. Instead
each variant is a separate `#[repr(C)]` struct, reached by pointer arithmetic from
`KVM_RUN_UNION_OFFSET`. Same memory, explicit offset, and the offset is asserted.

---

## Part 1 - `Vm::new`: bring-up, in order

### 1. `open("/dev/kvm", O_RDWR | O_CLOEXEC)`

Opens the KVM control device. This fd is not a VM - it is the handle on the subsystem, used for
global queries and for creating VMs.

`O_CLOEXEC` matters more in a VMM than in most programs. Without it, any process this one later
`exec`s inherits a KVM handle. That is a privilege-escalation surface, and reasoning about exactly
this class of fd leak is a large part of why Firecracker has a jailer at all.

### 2. `ioctl(kvm_fd, KVM_GET_API_VERSION)`

Must return exactly `12`. KVM froze this number in 2007 and has used per-capability queries for
everything since. It is the cheapest possible check that the fd really is KVM.

### 3. `ioctl(kvm_fd, KVM_CHECK_EXTENSION, KVM_CAP_USER_MEMORY)`

Asks whether userspace may supply its own guest memory. Every VMM written this century requires it.
`KVM_CHECK_EXTENSION` is the general mechanism for feature discovery: pass a capability id, get back
0 for absent or a positive value that sometimes encodes a limit.

### 4. `ioctl(kvm_fd, KVM_CREATE_VM, 0)` -> **vm fd**

The kernel allocates a `struct kvm`: an empty guest physical address space, a memory-slot array, and
a list of vCPUs that is currently empty. The `0` is the machine type.

### 5. `ioctl(vm_fd, KVM_SET_TSS_ADDR, 0xfffbd000)`

Tells KVM where in guest physical space it may place three pages of scratch used to emulate a
task-state segment.

Historical: VMX originally could not enter a guest that was in real mode, so KVM emulated real mode
by running the guest inside a hidden protected-mode task, which needs a TSS somewhere the guest is
not using. Hardware with **unrestricted guest** (every Intel part since Westmere-ish, and confirmed
`Y` in this machine's environment manifest) runs real mode natively and never touches it. Setting it
costs nothing and makes the example work on old hardware, so a failure here is reported and
execution continues rather than aborting.

### 6. `mmap(NULL, 32768, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE, -1, 0)`

Ordinary anonymous memory in this process. Nothing about it is special yet.

`MAP_PRIVATE` is sufficient here. A VMM that needs to share guest memory with another process - a
vhost-user backend, or a `userfaultfd` handler living outside the VMM, which is rung 3 - must use
`MAP_SHARED`, and that decision is made at allocation time and cannot be revised later.

### 7. `ioctl(vm_fd, KVM_SET_USER_MEMORY_REGION, &region)`

The pivotal call.

```rust
kvm_userspace_memory_region {
    slot: 0,
    flags: 0,
    guest_phys_addr: 0,
    memory_size: 32768,
    userspace_addr: <what mmap returned>,
}
```

The kernel records the slot and programs the EPT so that guest-physical `[0, 0x8000)` translates to
the host physical pages behind that mapping. After this, the guest's own page-table walks resolve
into that memory in hardware, with no exit.

Three consequences worth stating explicitly:

- The host reads and writes guest memory with a plain pointer. `Vm::load` is a `memcpy`.
- Any guest-physical address *outside* every slot has no EPT entry, so touching it exits. That is
  the entire mechanism behind MMIO device emulation.
- `userspace_addr` must remain mapped for as long as the region exists. Unmapping it under a running
  guest is a use-after-free that the *hardware* discovers, not the compiler. `Mapping`'s `Drop` and
  the field order of `Vm` are what enforce this here; `kvm-ioctls` marks the equivalent call
  `unsafe` for exactly this reason.

`flags` is where `KVM_MEM_LOG_DIRTY_PAGES` and `KVM_MEM_READONLY` live. Dirty logging is what live
migration and snapshotting are built on. Unused in this toy, central to rung 3.

### 8. `ioctl(vm_fd, KVM_CREATE_VCPU, 0)` -> **vcpu fd**

Allocates a `struct kvm_vcpu` and its VMCS. The argument is the vCPU id.

A vCPU fd is effectively thread-affine: `KVM_RUN` must be issued from the thread that owns it,
because KVM saves and restores per-thread FPU and register state around the entry. Real VMMs run one
OS thread per vCPU for this reason, not for scheduling convenience.

### 9. `ioctl(kvm_fd, KVM_GET_VCPU_MMAP_SIZE)`

The size of the shared communication page. Note it is issued on the **`/dev/kvm` fd**, not the vCPU
fd: it is a property of the kernel build, not of any particular vCPU.

Using a hard-coded 4096 would work today and corrupt memory on a kernel that grew the structure -
the I/O payload area lives past the header, inside this size.

### 10. `mmap(NULL, run_size, PROT_READ|PROT_WRITE, MAP_SHARED, vcpu_fd, 0)`

Maps `struct kvm_run`. This is the zero-syscall channel between KVM and the VMM: the kernel writes
the exit reason and operands here, and for exits that need a value handed back, the VMM writes it
here before re-entering.

`MAP_SHARED` is mandatory. With `MAP_PRIVATE` the VMM would get a copy-on-write snapshot and would
read a stale `exit_reason` forever - a bug that produces an infinite loop with no error anywhere.

---

## Part 2 - `Vm::load`

A bounds check and a `copy_nonoverlapping` into the guest mapping. That is the whole of "loading a
kernel": a real VMM parses an ELF or a bzImage and writes segments to the addresses the header asks
for, plus a boot parameter block. The mechanism is this function.

The bounds check is not decoration - `tests/boot.rs` asserts that a load crossing the end of guest
RAM is refused. Without it a bad address is a host heap corruption.

---

## Part 3 - `Vm::set_real_mode_regs`

### `KVM_GET_SREGS` -> modify -> `KVM_SET_SREGS`

After `KVM_CREATE_VCPU` the vCPU is in the x86 reset state: `cs.selector = 0xf000`,
`cs.base = 0xffff0000`, `rip = 0xfff0`. That is where a physical CPU fetches its first instruction,
four gigabytes up, where the firmware ROM is decoded.

This VMM has no firmware, so it flattens `cs`, `ds`, `es`, `fs`, `gs` and `ss` to `base = 0` and
`selector = 0`. The type/present/limit fields from the reset state are kept deliberately: they
already describe a usable 64 KiB real-mode segment, and rebuilding them by hand is a way to get them
subtly wrong.

Read-modify-write rather than constructing `kvm_sregs` from scratch, for the same reason: the fields
this code does not care about are already correct.

### `KVM_SET_REGS`

```rust
kvm_regs { rip: 0x1000, rflags: 0x2, ..Default::default() }
```

`rflags` bit 1 is reserved and architecturally always 1. Entering with it clear is an invalid guest
state; the hardware refuses the entry and KVM reports `KVM_EXIT_FAIL_ENTRY` without executing a
single instruction. It is a confusing failure precisely because nothing in the VMM looks wrong.

---

## Part 4 - `Vm::run`, the main loop

Every VMM has this loop and they all look like this.

### `ioctl(vcpu_fd, KVM_RUN, 0)`

The kernel saves host state, loads guest state, and executes `VMLAUNCH` or `VMRESUME`. The CPU
enters non-root mode and runs guest instructions **natively**. Control returns to KVM on a VM exit.
KVM handles what it can in-kernel and re-enters transparently; what it cannot handle, it describes
in `kvm_run` and returns from the ioctl.

So one `KVM_RUN` return may hide many hardware VM exits. The number measured in the README is
specifically the cost of an exit that reaches userspace.

### `EINTR`

`KVM_RUN` returning `-1` with `EINTR` is normal, not exceptional: a signal delivered while the vCPU
was in guest mode kicks it out. Guest state is intact and re-entering resumes it. A VMM that treated
this as fatal would die whenever the host looked at it funny. The loop counts it and continues, and
the benchmark reports the count because those samples measure the host's interruption, not an exit.

### Reading the header

```rust
unsafe { std::ptr::read_volatile(self.run_map.ptr.cast::<kvm_run_header>()) }
```

`read_volatile` because the page is written by the kernel behind the compiler's back. A normal read
would be legal for the compiler to hoist or cache across loop iterations.

### `KVM_EXIT_IO`

```rust
struct kvm_run_io { direction, size, port, count, data_offset }
```

Note what is *not* in it: the data. Port I/O can be a string operation (`ins`/`outs`) moving `count`
items at once, so KVM places the payload elsewhere in the shared page and reports `data_offset`.

**`data_offset` is measured from the start of the `kvm_run` mapping, not from the union.** This is
the classic first bug in a hand-written VMM, and it is a bug that compiles and produces plausible
garbage. The code bounds-checks `data_offset + len` against the mapping size before forming the
slice.

`direction` is from the *guest's* point of view: `KVM_EXIT_IO_OUT` means the guest wrote, so the VMM
consumes `data`. `KVM_EXIT_IO_IN` means the guest read, so the VMM must fill `data`.

The demo's `out dx, al` produces `direction=OUT, size=1, count=1`.

### `KVM_EXIT_MMIO`

```rust
struct kvm_run_mmio { phys_addr, data: [u8; 8], len, is_write }
```

Here the payload *is* inline: MMIO accesses are at most 8 bytes, so KVM can carry them in the union.

The exit happened because `phys_addr` is not covered by any memory slot. Note the inversion: **KVM
does not know a device exists.** It reports "nothing is mapped here"; deciding which device, if any,
owns the address is entirely the VMM's job. This code range-checks against the toy device's window
and returns an error otherwise, because attributing a stray access to the only implemented device
would be a silent lie.

On a **write**, `data[..len]` is what the guest stored.

On a **read**, the VMM fills `data[..len]`. That store is not a message to the kernel - it *is* the
value the guest's load returns. During the next entry KVM places it in the guest's destination
register, and the guest cannot tell its load took a detour through userspace. The demo stores `0x42`
and loads it back precisely so this round trip is visible: the `B` printed on the second output line
exists only because the read handler produced it.

`write_volatile` into `(*p).data` for the same reason `read_volatile` is used above.

### `KVM_EXIT_HLT`

The guest executed `hlt`. Because this VMM never called `KVM_CREATE_IRQCHIP`, there is no in-kernel
local APIC, so KVM cannot decide when to wake the vCPU and hands the decision to userspace. With an
in-kernel irqchip this would not reach userspace at all - it would block in the kernel until an
interrupt arrived.

The demo treats it as "guest finished". `rip` afterwards is one byte *past* the `hlt`: the
instruction retired normally, and the CPU then had nothing to do.

### Everything else

Reported as an error. `KVM_EXIT_FAIL_ENTRY` in particular means the hardware refused the guest
state, which is almost always wrong register setup rather than anything the guest did. Silently
continuing past an unrecognised exit is how a VMM ends up spinning.

---

## Part 5 - `guest.rs`, the guest programs

### The demo program, 28 bytes

```
 offset  bytes        instruction         exit
 0x00    BA F8 03     mov dx, 0x3f8       -
 0x03    B0 4B        mov al, 'K'         -
 0x05    EE           out dx, al          KVM_EXIT_IO
 ...                  (V, M, newline)
 0x0F    B0 42        mov al, 0x42        -
 0x11    A2 00 80     mov [0x8000], al    KVM_EXIT_MMIO write
 0x14    A0 00 80     mov al, [0x8000]    KVM_EXIT_MMIO read
 0x17    EE           out dx, al          KVM_EXIT_IO   <- prints what the VMM returned
 0x18    B0 0A        mov al, '\n'        -
 0x1A    EE           out dx, al          KVM_EXIT_IO
 0x1B    F4           hlt                 KVM_EXIT_HLT
```

`A2` is `MOV moffs8, AL` and `A0` is `MOV AL, moffs8` - the "move to/from absolute offset" forms,
which take a displacement directly rather than a ModRM byte. In 16-bit mode the displacement is 16
bits, so `A2 00 80` stores `AL` at `DS:0x8000`, and `DS.base` is 0, so that is guest physical
`0x8000`.

Hand-assembled bytes rather than a `.S` file, deliberately: an assembler would hide the one thing
worth seeing, which is that the "guest kernel" a VMM boots is nothing more than bytes at an agreed
guest physical address with `rip` aimed at them.

### The benchmark program

```
 0x00    BA 80 00     mov dx, 0x80        diagnostic port; the handler does nothing
 0x03    B9 lo hi     mov cx, count       patched by bench_program()
 0x06    EE           out dx, al          one VM exit per iteration
 0x07    E2 FD        loop 0x06           dec cx; jump if cx != 0
 0x09    F4           hlt
```

The loop body is one byte on purpose: what is being measured is the round trip, so every guest-side
instruction that is not the `out` is a contaminant. The `loop` instruction is unavoidable and costs
a few guest cycles - single-digit nanoseconds against a 1,600 ns round trip, which is a floor on the
accuracy rather than a rounding error to ignore silently.

Port `0x80` is the BIOS POST diagnostic port. Writes to it are meaningless to any real device, which
is what is wanted: the handler must contribute nothing to the measurement.

`count` is 16 bits because `loop` decrements `cx`, so one entry yields at most 65,535 samples.
`bench_program` rejects `count == 0` rather than accepting it, because `loop` decrements first: zero
would wrap to 65,535 and run the maximum, the exact opposite of what the caller asked for. A unit
test covers that, and another covers the little-endian patch - a big-endian patch would silently run
a different number of iterations and quietly change every number in a results file.

---

## Part 6 - `device.rs`

Two devices, both one line of logic.

- `pio_write(0x3f8, data)` appends to `serial_out`. There is no UART; a byte written is a byte
  printed. Real VMMs implement the full 8250 register set because guest kernels probe it.
- `mmio_write(0x8000, data)` latches one byte. `mmio_read(0x8000, data)` returns it.

Two details are deliberate:

- **Unimplemented reads return `0xff`, not `0`.** An unterminated bus on real hardware floats high,
  so probing an absent device reads all-ones, and guest drivers are written to treat all-ones as
  "not present". Returning zero makes an absent device look like a present one answering with
  zeros, which is how a guest hangs on a device that is not there.
- **`unhandled` is a counter, not a log line.** A non-zero value at the end of a run means the guest
  saw a value the VMM invented, which is a finding. Both the demo and the test assert it is zero.

The point of the file is how little there is to it. "Emulating a device" means answering *what
should have happened when the guest touched this address*. Cloud Hypervisor's `Bus` and
Firecracker's MMIO device manager are range maps dispatching to a `read`/`write` pair with this
signature. The interesting engineering begins when the device is a virtio device, whose real work
happens in shared memory and whose exits are only notifications - which is rung 2.

---

## Part 7 - `main.rs`

`demo()` runs unconditionally, including before `--bench`. That is rule 4 of the measurement
standard: a benchmark of a VMM whose exits are mishandled measures the wrong thing while looking
perfectly healthy.

`run_bench()`:

1. `calibrate_timer()` first - rule 3, measure the measurement. Two `Instant::now()` calls with
   nothing between them, 10,000 times, median reported. It came out at 16 ns, about 1% of the median
   exit.
2. Collect in rounds of at most 65,535, resetting `rip` and `cx` between them. Worth seeing: a vCPU
   is a resumable object, and re-pointing `rip` at the program start is the same mechanism a
   snapshot restore uses at larger scale.
3. Drop the last sample of each round - it is the `hlt`, whose exit path differs. Leaving it in
   would put a handful of differently-shaped samples into a distribution a reader will read as
   uniform.
4. Report min/p50/p90/p99/p99.9/max. No mean, by policy.
5. Write raw samples, one per row, only when `--out` is given, with a reminder that a summary alone
   is not evidence.

`Stats::from` uses nearest-rank quantiles with no interpolation, so every number printed is a sample
that actually occurred and can be found in the raw file.

---

## Part 8 - `tests/boot.rs`

Three tests, each skipping cleanly when `/dev/kvm` is unavailable so the workspace is testable in a
container or on a non-x86 host.

- `demo_guest_boots_and_round_trips_mmio` - asserts the serial output, the exact exit counts (6 I/O,
  2 MMIO), zero unhandled accesses, and that `rip` lands one past the `hlt`.
- `loading_past_the_end_of_guest_ram_is_refused` - the bounds check.
- `bench_program_produces_the_requested_number_of_exits` - N I/O exits for a request of N, plus one
  timing sample per `KVM_RUN`. This is the assertion that would catch a big-endian count patch or a
  `cx` wrap *before* a wrong number reached a results file.

---

## Part 9 - `toy-kvm-crates/src/main.rs`

The same VMM on `kvm-ioctls` and `vm-memory`, sharing the guest programs and device model with the
raw crate so that only the VMM differs. It prints byte-identical output.

What the crates buy:

| Raw | Crates | What is bought |
|---|---|---|
| `ioc()` const fns, hand-transcribed structs | `Kvm::new()`, `kvm-bindings` | Layouts generated from kernel headers. The most valuable part, because a transcription error is silent. |
| `OwnedFd` + `Mapping` + a comment about field order | `Kvm`, `VmFd`, `VcpuFd` | The fd hierarchy in the type system. |
| `mmap` + one hand-written bounds check | `GuestMemoryMmap`, `write_slice` | A sparse, multi-region address space where `GuestAddress` is a distinct type from a host pointer - so confusing a guest address for a host address must be written deliberately. |
| `read_volatile` at offset 32 | `match vcpu.run()?` | The union decoded into an enum with correctly-sized slices. Removes the `data_offset` trap entirely. |

What they do not buy: the model. There is still a run loop, still an exit dispatch, still a device
defined by the absence of memory, and `set_user_memory_region` is still `unsafe`. Note also that
`set_real_mode_regs` is *no shorter* in the crate version - `kvm-ioctls` has nothing to say about
what a sensible initial vCPU state is, and every VMM writes its own. Cloud Hypervisor and
Firecracker each have a few hundred lines doing it properly for long mode, entirely their own code.
