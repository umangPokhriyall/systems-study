# Comprehension gate - rung 1

Rules: answered **from memory**, without re-reading the code, in writing, before the rung is
recorded as complete. A question answered by looking it up is a question failed.

Any question failed goes into [`../docs/OPEN-QUESTIONS.md`](../docs/OPEN-QUESTIONS.md) and the gate
is retaken no sooner than a week later, so that the second attempt tests retention rather than
short-term memory.

**Status: not yet attempted.** Date attempted: _____ Date passed: _____

---

## Mechanism

1. A guest executes a load from a guest physical address that is covered by a memory slot, and the
   guest's own page tables map it. How many address translations does the hardware perform, who owns
   each of the tables involved, and how many VM exits occur?

2. `KVM_SET_USER_MEMORY_REGION` returns success. Describe everything that has changed - in the
   kernel, in the hardware, and in your process - and everything that has *not*.

3. The VMM never registers a device anywhere. Explain precisely why an access to `0x8000` exits and
   an access to `0x7000` does not, and what KVM knows about the difference.

4. `kvm_run` is `mmap`'d from the vCPU fd rather than passed to an ioctl. Give two distinct reasons
   this is the right design, one about cost and one about the shape of the data.

5. On `KVM_EXIT_MMIO` with `is_write == 0`, the VMM writes into `kvm_run.mmio.data`. Trace what
   happens to those bytes from that store until the guest instruction completes. At what point does
   the guest's destination register change, and which component does it?

6. Why does `KVM_EXIT_IO` report a `data_offset` instead of carrying the payload inline, when
   `KVM_EXIT_MMIO` carries its payload inline? What property of the two mechanisms forces the
   difference?

## Design and consequence

7. `kvm-ioctls` is a safe wrapper, yet `set_user_memory_region` is `unsafe`. State the exact safety
   obligation, explain why no type can discharge it, and give a concrete sequence of operations that
   would violate it without any `unsafe` block in the caller's own code.

8. The `Vm` struct's field order is described as load-bearing. Say what would go wrong if `vcpu` were
   declared before `run_map`, and whether the resulting failure would be a Rust error, a kernel
   error, or something else.

9. `MAP_SHARED` is mandatory for the `kvm_run` mapping. Describe the exact observable behaviour of a
   VMM that used `MAP_PRIVATE`, and why it would be hard to diagnose.

10. Guest RAM here is `MAP_PRIVATE`. Name a feature that would force `MAP_SHARED` instead, and
    explain why that choice cannot be deferred until the feature is needed.

## Measurement

11. A userspace-handled VM exit measured ~1,600 ns at p50 on this machine. Timer overhead was 16 ns.
    Explain why the overhead is stated rather than subtracted, and identify the circumstance under
    which that choice would become wrong.

12. p90 is 9% above p50 but p99.9 is 2.7× it, and the max is 78×. Attribute each of those three
    regions to a mechanism, and say which of them would change on a tuned bare-metal machine and
    which would not.

13. One `KVM_RUN` return may correspond to more than one hardware VM exit. Explain how, and describe
    what you would measure to find the ratio.

14. Somebody reports that their VMM is "30% faster" after a change, citing mean exit latency over
    1,000 samples. Give three specific reasons that claim might be unsupported, in the order you
    would raise them in review.

## Transfer

15. Rung 2 is virtio. Given the number measured here, explain from first principles why virtqueues
    are built around *shared memory with occasional notifications* rather than around one exit per
    I/O operation, and estimate the cost of the naive design at 50,000 IOPS.

16. `EVENT_IDX` lets a guest tell the host "do not notify me until you have consumed up to index N".
    Using this rung's number, state what it saves and what it risks.

17. A colleague proposes reducing snapshot restore time by copying all guest memory in eagerly
    before resuming the vCPU. Using only what this rung establishes about guest memory, state what
    that trades away and what you would measure to decide.

18. Why is real mode where a vCPU starts, and what would a VMM that skipped straight to long mode
    have to construct in guest memory before its first instruction could run?

## Adversarial

19. A guest writes to a guest physical address that is neither RAM nor any implemented device. This
    VMM returns an error. Name two other defensible behaviours, and say which one a production VMM
    should choose and why.

20. You are reviewing a patch that adds a new MMIO device to a VMM. The `read` handler returns `0`
    for any offset it does not implement. Explain why that is a bug rather than a style preference,
    and what it would look like from inside the guest.
