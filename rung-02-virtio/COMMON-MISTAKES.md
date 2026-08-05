# Common mistakes

Misconceptions, each with why it is wrong and what it looks like when it bites. The first two are
not hypothetical - they are what this rung actually hit, recorded from the inside.

---

## 1. "The mock framework is correct, so a disagreement means my code is wrong"

**What happened.** The hand-written device and the `virtio-queue` version produced identical output
bytes but disagreed on whether to interrupt: `true` versus `false`, for the same three completions.

The first assumption was that the hand-written `needs_notification` was wrong, because the other
side was a maintained crate with tests. It was not. `MockSplitQueue` places the used ring *inside*
the avail ring, so `used_event` was being read from the used ring's first `len` field. It returned
`12` - the length of `"HELLO VIRTIO"` - which made `need_event` compute a threshold in the future
and answer "no interrupt needed".

**How it was actually found.** Not by reading `mock.rs`. By printing the value:

```
PROBE avail=128 used=140 used_event_val=Ok(12) used_idx=Ok(3)
```

`used = 140` when the avail ring for 8 entries needs `4 + 16 + 2 = 22` bytes and therefore runs to
150. The two numbers being visibly inconsistent is what turned a vague "these disagree" into a
located bug in about a minute. See README §4 for the cause and the fix.

**The lesson.** A disagreement between two implementations is *information*, and the right next
move is to find out which one is wrong rather than to assume. Also: when comparing two
implementations, print the intermediate values, not just the outputs. The outputs matched perfectly
here and told you nothing.

## 2. "It produced the right bytes, so the layout is fine"

The demo publishes three chains. The mock's ring corruption starts at avail entry **6**. So the demo
was correct, repeatedly, on a queue whose memory layout was broken - and would have stayed correct
until something published seven chains.

This is the characteristic failure mode of ring-buffer bugs: they are latent until the ring is busy
enough to reach the damaged region, which is exactly when reproducing them is hardest.

## 3. "`avail.idx` is the number of entries in the queue"

It is the number of entries **ever published**, free-running, wrapping at 65,536. The number
outstanding is `avail.idx - next_avail` in wrapping arithmetic.

The mistake usually appears as a comparison:

```rust
if self.next_avail < avail_idx { /* there is work */ }
```

which works for exactly 65,536 operations and then stops seeing work forever. On a queue doing 50
requests per second that is a hang after 22 minutes - late enough that nobody connects it to the
queue, and rare enough that it never reproduces in a test.

## 4. "The descriptor table is the queue"

It is a pool. The avail ring is the queue. Descriptors are allocated from a free list, are not
consecutive, and are not consumed in order.

The consequence people trip over: **the used ring reports only the head index**, so a driver that
frees only the head leaks every other descriptor in every multi-descriptor chain. The queue slowly
stops accepting work, and the failure appears far from the cause.

## 5. "The device can trust `VIRTQ_DESC_F_WRITE`"

It is a promise the driver makes about its own memory, not a fact the device may assume. A chain
that interleaves readable and writable descriptors is illegal (VIRTIO 1.2, 2.7.5.3) - and a device
that trusts the ordering rather than checking it can be made to write output into a buffer the
driver is still reading.

Note that `virtio-queue`'s `readable()`/`writable()` **filter** a chain but do not object to one
that interleaves them. That is device policy, correctly left to the device - but it means "I used
the crate" is not an answer to "did you check the ordering?".

## 6. "One bound on the chain walk is enough"

Three bounds, three different attacks:

- Only `ttl`? A `next` of 60,000 still reads 16 bytes from past the descriptor table on the first
  step.
- Only the index check? `desc[0].next = 0` still loops forever.
- Only both of those? 128 in-range descriptors claiming 4 GiB each still asks the device to move
  half a terabyte.

A single test named "rejects malformed chains" would pass with any one of them removed. Exercise 4
is removing them one at a time and watching each distinct failure.

## 7. "`addr + len <= region_len` is a bounds check"

Not when `addr` is guest-controlled. `addr = u64::MAX - 8, len = 4096` wraps the sum to a small
number, and the check passes. Use `checked_add`.

This is the same class of bug as trusting `next`, and it is worth internalising as one rule:
**arithmetic on guest-supplied integers must be checked arithmetic, everywhere, without exception.**

## 8. "`used.len` is how much buffer the driver gave me"

It is how many bytes the device **wrote**. The driver takes it as the length of valid data.

Over-reporting hands the guest whatever was in the buffer before - which in a VMM that reuses
buffers is another guest's data. The demo deliberately provides a 64-byte reply buffer for a 12-byte
reply so that `len=12` is visible in the trace.

Note that an output-correctness test does **not** catch this: the first 12 bytes are still right.
Exercise 3 is breaking it on purpose and working out what assertion would.

## 9. "Dropping a malformed chain is the safe response"

It leaks a descriptor and leaves the driver waiting forever for a completion that will not come. The
queue degrades rather than failing, which is harder to diagnose than a clean error.

Complete it, with length 0. Refusing to do the work and refusing to answer are different things.

## 10. "The fences are for the compiler"

They are for the *hardware*. Without the release before `avail.idx` and the acquire after reading
it, the index can become visible before the descriptors it advertises, and the device processes a
well-formed-looking request from several thousand operations ago.

x86 does not reorder stores relative to other stores, so the mistake is invisible there. arm64 does.
"Works on x86, hangs on arm64" is the signature, and it is one of the most common serious bugs in
virtio implementations.

## 11. "`EVENT_IDX` always helps"

At batch size 1 it suppresses nothing and costs an extra read of `avail_event` per submission. It is
a mechanism for queues under load. README §3.2's first row says 0%, and that row is there
deliberately.

The related mistake is assuming it helps *both* directions equally. In the interleaving measured
here its entire benefit was on the kick path, because the device already batched its completions and
batching alone coalesced the interrupts.

## 12. "A benchmark that shows no effect is a failed benchmark"

The interrupt columns in README §3.2 show no difference at all. That is a statement about the
workload, not about `EVENT_IDX` - and identifying which is the actual skill.

Deleting the columns would have made the result look cleaner and would have been dishonest. Keeping
them, with the explanation, is what makes the kick-path number believable.
