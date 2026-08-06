# Comprehension gate - rung 3

Rules: answered **from memory**, without re-reading the code, in writing, before the rung is
recorded as complete. A question answered by looking it up is a question failed.

Any question failed goes into [`../docs/OPEN-QUESTIONS.md`](../docs/OPEN-QUESTIONS.md) and the gate
is retaken no sooner than a week later.

**Status: not yet attempted.** Date attempted: _____ Date passed: _____

---

## Mechanism

1. Trace a demand fault from the faulting instruction to its completion. Name every thread involved,
   say which is running and which is blocked at each step, and identify the two context switches.

2. `UFFDIO_API` must be the first ioctl on the fd, and it is described as a negotiation rather than
   a query. Explain the difference, and describe the correct procedure for finding out whether a
   kernel supports a feature you want.

3. `UFFDIO_COPY` reports errors in a signed out-parameter while the ioctl returns success. Describe
   exactly what a handler that checks only the return value does wrong, and what the symptom is.

4. Why is `-EEXIST` from `UFFDIO_COPY` not an error? Construct the situation that produces it.

5. `MADV_DONTNEED` re-arms the fault. Explain what it does to the page tables and why the whole
   benchmark depends on it. Then name a production situation in which the same call is a disaster.

6. A userfaultfd is closed while faults are outstanding. What happens to the parked threads, and how
   fast? Why is that worse for a VMM than a hang?

## Security and deployment

7. Explain, in terms of what an attacker gains, why an unprivileged `userfaultfd` is a kernel
   security concern. What specific capability does it hand out?

8. `UFFD_USER_MODE_ONLY` restricts reporting to user-mode faults. Explain how that removes the
   capability in question 7 while leaving demand paging working.

9. This machine has both `vm.unprivileged_userfaultfd = 0` and a root-only `/dev/userfaultfd`.
   Describe what each gate controls, and say whether they are redundant.

10. A library refuses to fall back to the syscall when `/dev/userfaultfd` exists but is unreadable.
    Argue that this is correct, then argue that it is not, then say which you would take to the
    maintainer and how you would open the conversation.

11. Firecracker runs its UFFD handler in a separate, unprivileged process. Name two things that
    forces at `mmap` time and two failure modes it introduces.

## Measurement

12. A kernel anonymous fault costs ~465 ns here and the best demand fault ~3,510 ns. Account for the
    factor of 7.6 - name the components and say which you would expect to dominate.

13. With handlers parked in `poll`, the *same logical CPU* was the fastest placement. Explain why
    that conclusion is wrong, what experiment refuted it, and what the correct statement is.

14. That experiment changed two things at once. Name them, and design the experiment that changes
    only one.

15. The SMT sibling was the worst placement in both modes. Give the mechanism, and say why the
    L1/L2 locality it gains does not compensate.

16. At batch size 16 and above, p50 is 46 ns and p99 is 17,000 ns. Explain the distribution's shape,
    and say precisely why a mean of these samples would be a lie rather than merely imprecise.

17. Amortised cost falls 6.5× from batch 1 to batch 64 while p99 rises 4.7×. State the tradeoff in
    one sentence, then name the workload property that decides which side to take.

18. The benchmark walks pages in ascending order. Explain why that makes the batch column an upper
    bound rather than an estimate, and predict the direction and rough size of the correction.

19. `Instant::now()` costs ~16 ns. Identify the one number in §3 that is materially distorted by
    that, and by how much.

## Transfer

20. Cloud Hypervisor reports `restore_latency_time_ms` and nothing after it. Say what that number
    hides, and describe the smallest additional metric that would expose it.

21. Cloud Hypervisor v53 added background prefault threads with a configurable count. Using this
    rung's results, state what that knob trades and what you would measure to set it.

22. A guest resumes from a snapshot and runs 20% slower than the same guest booted normally, for the
    first few seconds. Give three hypotheses and the measurement that distinguishes them.

23. Rungs 1, 2 and 3 each measured something and each reported a tail that was unstable on this
    machine. Say what is common to those three tails, and what would have to change about the
    machine for any of them to become trustworthy.
