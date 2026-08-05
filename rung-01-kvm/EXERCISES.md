# Exercises

Ordered easy to hard. Each states what it teaches, because an exercise that only produces working
code has been wasted.

Status is recorded honestly. "not done" is a legitimate final state; a repository where every box is
ticked is indistinguishable from one where the boxes were the goal.

| # | Exercise | Status |
|---|---|---|
| 1 | Change the guest's message | not done |
| 2 | Make the device do arithmetic | not done |
| 3 | Add a second MMIO register | not done |
| 4 | Measure MMIO exits against port I/O exits | not done |
| 5 | Break `rflags` on purpose | not done |
| 6 | Break the `data_offset` base on purpose | not done |
| 7 | Count exits that never reach userspace | not done |
| 8 | Give the guest a second memory region with a hole | not done |
| 9 | Reach a device above 64 KiB (unreal mode) | not done |
| 10 | Run two vCPUs | not done |
| 11 | Snapshot and restore the vCPU | not done |
| 12 | Boot in 64-bit long mode | not done |

---

### 1. Change the guest's message

Make the guest print something other than `KVM`. Update `DEMO_EXPECTED_OUTPUT` and keep the test
passing.

*Teaches:* that the guest is just bytes, and that the expected-output assertion is what makes the
whole thing a test rather than a demo.

### 2. Make the device do arithmetic

Have `mmio_read` return `latch + 1` instead of `latch`. Predict the printed character before
running.

*Teaches:* the VMM controls what the guest observes. This is the entire basis of device emulation,
and the moment it stops feeling like a trick is the moment the model has landed.

### 3. Add a second MMIO register

Implement offset 4 as a read-only counter of how many times the device has been read. Extend the
guest program to read both.

*Teaches:* address decoding inside a device window - the thing every real device model spends most
of its code on.

### 4. Measure MMIO exits against port I/O exits

Write a second benchmark program that loops on `mov [0x8000], al` instead of `out dx, al`, and
compare the distributions.

Predict the direction first and write the prediction down. MMIO goes through KVM's instruction
emulator - the kernel must decode the faulting instruction to find the operand - while port I/O has
a fast path. Then check whether the size of the difference matches the prediction.

*Teaches:* not all exits cost the same, and why virtio-pci and virtio-mmio notification differ in
cost. This is the first exercise producing a number worth committing, with a manifest.

### 5. Break `rflags` on purpose

Remove `rflags: 0x2` from `set_real_mode_regs` and run.

Expect `KVM_EXIT_FAIL_ENTRY`. Then confirm what the hardware actually complained about by reading
`kvm_run.fail_entry.hardware_entry_failure_reason` - which requires adding that union variant to
`kvm_sys.rs`.

*Teaches:* invalid guest state is refused by the hardware before a single instruction runs, and the
failure looks like nothing is wrong. Also: the union has more variants than this VMM decodes.

### 6. Break the `data_offset` base on purpose

Change the I/O payload pointer from `run_map.ptr + data_offset` to `union_ptr + data_offset` and
observe the output.

*Teaches:* the classic hand-written-VMM bug, from the inside. It compiles, it runs, and it produces
plausible garbage - which is why the bounds check exists and why the crate version's decoded enum is
worth something.

### 7. Count exits that never reach userspace

Use `perf` to count hardware VM exits during a benchmark run and compare with the number of
`KVM_RUN` returns:

```
perf stat -e kvm:kvm_exit,kvm:kvm_entry -- ./target/release/toy-kvm-raw --bench 200000
```

`perf_event_paranoid` is 1 on this machine, so the tracepoints are readable without root.

*Teaches:* one `KVM_RUN` return can hide several hardware exits, and the in-kernel path is roughly
an order of magnitude cheaper. The README explicitly declines to claim this ratio because it has not
been measured; this exercise is what would let it be claimed.

### 8. Give the guest a second memory region with a hole

Install a second slot at guest physical `0x10000` and leave `[0x8000, 0x10000)` unmapped. Confirm
the device still exits and the new region does not.

*Teaches:* slots, sparseness, and why `GuestMemoryMmap` is a *list* of regions. This is also the
shape of a real guest memory map, which has a hole below 4 GiB for PCI.

Note that a real-mode guest cannot reach the new region without exercise 9.

### 9. Reach a device above 64 KiB (unreal mode)

Move the toy device to guest physical `0x10_0000` and make the guest access it.

A real-mode segment limit is 64 KiB and the hardware enforces it regardless of how the address is
encoded, so an address-size override prefix alone is not enough - the access raises `#GP`. The fix
is to widen the hidden segment limit: set `ds.limit = 0xffffffff` and `ds.g = 1` in `kvm_sregs`
before entering. That is "unreal mode", and real hardware can only reach it via a trip through
protected mode.

*Teaches:* segment limits are real and enforced; the hidden descriptor cache is real state; and KVM
lets a VMM construct architectural states that hardware could only *arrive at*. This is the bug this
rung actually hit - see [`COMMON-MISTAKES.md`](COMMON-MISTAKES.md) #1.

### 10. Run two vCPUs

Create vCPU 1, run each in its own thread, and have them cooperate through a byte in shared guest
RAM.

`KVM_RUN` must be called from the thread owning the vCPU fd. Note what the host does *not* provide:
no coherence protocol, no ordering guarantees beyond what the hardware gives. The guests are two
real cores sharing real memory.

*Teaches:* why VMMs are one-thread-per-vCPU, and the first hint of the shared-memory discipline rung
2 formalises.

### 11. Snapshot and restore the vCPU

After the demo halts, capture `KVM_GET_REGS` + `KVM_GET_SREGS` + a copy of guest RAM. Create a fresh
VM, restore all three, and resume. It should re-execute from where it stopped.

Then find out what is still missing: MSRs (`KVM_GET_MSRS`), FPU/XSAVE state, the pending-interrupt
bitmap, and any device state. Write down which of them would matter for a guest that was doing real
work.

*Teaches:* what a snapshot actually consists of, and why the list is longer than it first appears.
This is the direct ancestor of rung 3 and of the restore-path work in the OSS roadmap.

### 12. Boot in 64-bit long mode

Build a GDT and 4-level page tables in guest memory, set `cr0`/`cr3`/`cr4`/`efer`, and enter with
64-bit code.

Budget a day and expect `KVM_EXIT_FAIL_ENTRY` several times. Cloud Hypervisor's and Firecracker's
`gdt.rs` and `regs.rs` are the reference, and reading them *after* failing is the point.

*Teaches:* what a bootloader does, and why every VMM carries a few hundred lines of its own vCPU
setup that no crate provides.
