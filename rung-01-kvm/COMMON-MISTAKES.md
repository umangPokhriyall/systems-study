# Common mistakes

Misconceptions, each with why it is wrong and what it looks like when it bites. The first one is not
hypothetical: it is the bug this rung actually hit, recorded with its symptom because a mistake
described from the outside teaches much less than one described from the inside.

---

## 1. "The address-size prefix lets a real-mode guest reach any address"

**The mistake.** The first version of this VMM put guest RAM at `[0, 0x10000)` and the toy device at
`0x10000`. A 16-bit offset cannot express that, so the guest used the `0x67` address-size override
prefix to encode a 32-bit displacement:

```
67 A2 00 00 01 00     mov [0x00010000], al
```

The encoding is correct. The instruction still does not work.

**What actually happened.** The guest looped forever, printing `KVM` over and over. The trace showed
four I/O exits, then a fifth I/O exit that was `K` again with `rip` back at `0x1005`:

```
  exit #4   KVM_EXIT_IO   port=0x03f8 out size=1 count=1 data=[0a]  rip=0x100e
  exit #5   KVM_EXIT_IO   port=0x03f8 out size=1 count=1 data=[4b]  rip=0x1005
```

No MMIO exit ever occurred.

**Why.** A real-mode segment has a limit of 64 KiB, and the hardware enforces it against the
effective address regardless of how that address was *encoded*. The store raised `#GP`. Real mode
vectors exceptions through the interrupt vector table at guest physical 0, which was all zeros, so
the fault dispatched to `0000:0000` and the CPU began executing zeros. `00 00` is
`add [bx+si], al` - harmless, no exit - so it ran up through memory, wrapped, and eventually reached
the program at `0x1000` again. An infinite loop with no error reported anywhere.

**The lesson, which generalises well past real mode.** Encoding an address and being permitted to
access it are different questions, decided by different parts of the CPU. And a guest with no
exception handlers does not crash - it does something arbitrary and keeps going, which is why "the
guest is looping" is a symptom with a very wide differential.

**The fix here** was to move guest RAM to 32 KiB and the device to `0x8000`, inside the segment
limit. The proper fix - widening the hidden segment limit into "unreal mode" - is exercise 9,
because it is worth doing deliberately rather than as a workaround.

---

## 2. "KVM runs the guest"

KVM does not interpret, translate or execute guest instructions. The **hardware** runs the guest, at
native speed, in a mode where the guest kernel really is in ring 0. KVM sets up the VMCS, executes
`VMRESUME`, and then reacts to whatever brings control back.

Once that lands, several things stop being surprising: why virtualization overhead is measured in
exits rather than in a percentage; why a guest spinning in a tight computational loop costs the host
nothing at all; and why "reduce exits" is the only optimization axis that ever really matters.

## 3. "A VM exit is a syscall"

They are unrelated mechanisms that happen to both transfer control. A syscall goes ring 3 -> ring 0
within one privilege dimension. A VM exit goes non-root -> root, saving and restoring an entire
architectural state including control registers and segment descriptors.

The practical consequence is the cost: a `getpid` syscall is tens of nanoseconds; a userspace-handled
VM exit measured ~1,600 ns here. Roughly two orders of magnitude. That gap is why so much VMM design
is about *not* exiting.

## 4. "The VMM allocates guest memory"

Userspace `mmap`s ordinary memory and tells KVM to treat it as guest physical. The kernel allocates
nothing on the VMM's behalf.

Everything downstream follows from getting this right: the host can read guest memory with a
pointer, so virtio needs no copies; guest memory can be swapped, `madvise`d, file-backed or
demand-paged; and a stale host mapping is a use-after-free the *hardware* discovers.

## 5. "Devices are registered with KVM"

KVM has no idea a device exists. It reports "the guest touched an address with no memory behind it";
the VMM decides what that meant. A device is a hole in the guest physical address map plus a handler.

This is why `KVM_EXIT_MMIO` carries only an address, a length and a direction, and why every real
VMM has a `Bus` type mapping ranges to handlers.

A corollary that catches people: if you forget to install a memory slot you *meant* to install, the
guest's accesses to it become MMIO exits into a VMM that has no device there. The symptom is a
device that appears to exist and misbehaves, not a clean failure - which is why this VMM errors on
an MMIO access outside the toy device's window rather than absorbing it.

## 6. "`data_offset` is relative to the union"

It is relative to the start of the `kvm_run` **mapping**. This is the single most common bug in a
hand-written VMM, and it compiles, runs, and produces plausible garbage.

Worth noticing: the mistake is impossible to make in `kvm-ioctls`, because `VcpuExit::IoOut` hands
back a correctly-sized slice. That is a concrete example of what a safe wrapper is actually for -
not preventing memory unsafety in the abstract, but removing a specific arithmetic trap.

## 7. "`rip` in the exit trace is where the guest is now"

For an I/O exit, `rip` still points at the instruction that caused it. KVM advances it lazily on the
next entry, through `complete_userspace_io` - it must, because for an `in` it has to first place the
value userspace supplied into the destination register.

Reading the trace without knowing this makes it look as though the guest is stuck.

## 8. "`hlt` means the guest is finished"

It means the guest has nothing to do until an interrupt arrives. This VMM treats it as termination
only because it has no interrupt sources and no in-kernel irqchip.

With `KVM_CREATE_IRQCHIP`, `hlt` would not reach userspace at all: KVM would block the vCPU thread
in the kernel until an interrupt was injected. So "does `hlt` exit to userspace?" has the answer "it
depends on how you configured the VM", which is a good early example of how much KVM behaviour is
configuration rather than architecture.

## 9. "An unimplemented register should read as zero"

Return `0xff`. An unterminated bus floats high, so guest drivers are written to treat all-ones as
"device not present". Returning zero makes an absent device look present and answering with zeros,
and the guest hangs waiting for a device that was never there.

## 10. "Field order in a `#[repr(C)]` struct is a style question"

It is ABI. The kernel accesses these structures by offset. A reordered field is not a compile error;
it is a silently wrong VM, and the symptom appears far from the cause.

The defence used here is to derive the ioctl request number's size field from `size_of::<T>()` and
to assert the sizes at compile time. A layout error then produces `ENOTTY` at the first call rather
than corruption at an arbitrary later point.

## 11. "A benchmark's mean is the number to report"

The body of this rung's distribution is tight and the tail is not - p90 is 9% above p50, p99.9 is
2.7× it. A mean compresses all of that into one number that describes neither region.

The related mistake is treating the max as a finding. One sample in 200,000 on a machine that was
not quiet is a fact about the machine. Report it, do not build on it, and do not delete it either.

## 12. "It ran, so I understand it"

The failure mode this whole repository is arranged against. Running code is the weakest evidence of
comprehension available: mistake #1 above produced a program that ran, printed plausible output, and
was completely wrong about what the hardware was doing.

The gate exists for this reason, and it is taken from memory.
