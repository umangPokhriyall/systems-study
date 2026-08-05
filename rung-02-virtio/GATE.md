# Comprehension gate - rung 2

Rules: answered **from memory**, without re-reading the code, in writing, before the rung is
recorded as complete. A question answered by looking it up is a question failed.

Any question failed goes into [`../docs/OPEN-QUESTIONS.md`](../docs/OPEN-QUESTIONS.md) and the gate
is retaken no sooner than a week later, so the second attempt tests retention.

**Status: not yet attempted.** Date attempted: _____ Date passed: _____

---

## Layout

1. Draw the three rings from memory, with every field and its offset, for `queue_size = 8`. State
   which side writes each field.

2. Why does `used_event` live in the avail ring and `avail_event` in the used ring? Give the general
   rule, not the two special cases.

3. The descriptor table is described as a pool rather than a queue. Explain what that buys, and what
   would be lost if the avail ring carried descriptors directly instead of indices into a table.

4. `queue_size` must be a power of two. Give two independent reasons, one about the modulo and one
   about the 16-bit counters.

## Counters and wrapping

5. `avail.idx` is 5 and a device's `next_avail` is 3. How many chains are outstanding? Now
   `avail.idx` is 2 and `next_avail` is 65,534. Same question, and explain why the arithmetic is the
   same in both cases.

6. A device checks `if self.next_avail < avail_idx` instead of subtracting. Describe precisely when
   it breaks, what the symptom is, and why the bug is worse on a lightly loaded queue than on a busy
   one.

7. Why is the ring slot `i % queue_size` while the index itself is never reduced? What would break
   if the index were stored already-modulated?

## Ordering

8. Name the two release/acquire pairs and say which side owns each. For one of them, describe the
   exact incorrect behaviour a reader would observe if the fence were removed - not "undefined
   behaviour", but what the reader would actually read.

9. `enable_notification` uses a `SeqCst` fence rather than release/acquire. Explain what pattern of
   accesses it is protecting and why the weaker orderings do not cover it.

10. The two event fields are read with relaxed ordering. Justify that, and state what the worst
    consequence of reading a stale value is.

## `EVENT_IDX`

11. Write `need_event` from memory. Then explain each of the three subtractions in words.

12. The spec says "signal when `idx == event + 1`" and every implementation uses an inequality
    instead. Construct the sequence of operations where the spec's rule loses a notification
    entirely.

13. The driver's kick decision and the device's interrupt decision are the same function. State the
    two different pairs of counters it is applied to, and say why recognising this is more than a
    curiosity.

14. Rung 1 measured a VM exit at 1,610 ns. A queue sustains 50,000 requests per second with an
    average batch of 8. Compute the doorbell cost with and without `EVENT_IDX`, and state which
    assumption in that calculation you are least confident about.

15. Under what workload does `EVENT_IDX` save nothing, and what does it cost in that case?

## The adversarial surface

16. List the three bounds on a chain walk. For each: the malformed chain that defeats the *other
    two*, and what the host does without it.

17. A colleague proposes replacing all three with a single check that the chain is no longer than 64
    descriptors. Say what that catches, what it misses, and whether you would accept it in review.

18. Why must a bounds check on a descriptor buffer use checked arithmetic rather than
    `addr + len <= region_len`? Give the exact `addr` and `len` that defeats the naive form.

19. The device rejects a malformed chain and still writes it to the used ring with length 0. Explain
    what goes wrong if it silently drops the chain instead, and how long that failure takes to
    become visible.

20. A device model reports `used.len` as the size of the writable buffer rather than the bytes it
    wrote. Describe the security consequence concretely, and say why an output-correctness test does
    not catch it.

## Transfer and judgement

21. `virtio-queue` gives you the queue but not the device. Name three things it does not do that a
    block device model must, and say which of them is most likely to contain a bug.

22. Upstream's `DescriptorChain` ends iteration on error; this rung's yields `Result`. Argue both
    sides, then say what you would actually propose upstream and why.

23. `MockSplitQueue` placed the used ring inside the avail ring. Describe how that surfaced, and
    explain why the demo still produced correct output despite the corruption.

24. Rung 1 found p50 stable and p99 noisy; rung 2 found p50 stable to 3-9% but p99 moving by up to
    44%, on the same machine. Explain why the tail is proportionally *worse* here, given that the
    operations are two orders of magnitude shorter.
