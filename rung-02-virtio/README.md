# Rung 2 - virtio from first principles

A split virtqueue implemented by hand - both halves of it, driver and device - over a plain byte
buffer, with no dependencies at all. Then the device half rebuilt on `virtio-queue` and `vm-memory`,
producing identical bytes, so the difference between them is visible rather than assumed.

```
cargo run -p toy-virtq-raw                 # one queue, three requests, every ring transition traced
cargo run -p toy-virtq-raw -- --hostile    # chains a malicious guest would build, and the refusals
cargo run -p toy-virtq-crates              # the same device on virtio-queue + vm-memory
cargo test --workspace                     # 21 tests, most of them about malformed input
cargo run --release -p toy-virtq-raw -- --bench --out results/run
```

---

## 1. Learning objectives

After this rung I should be able to, without notes:

- Draw the three rings of a split virtqueue and say who writes each field.
- Explain why the descriptor table is a *pool* and the avail ring is the *queue*, and what that
  indirection buys.
- Explain why `avail.idx` and `used.idx` are free-running counters, and what breaks if you compare
  them with `<` instead of subtracting.
- State the release/acquire pairs on both sides and describe the bug that appears without them.
- Derive `EVENT_IDX` from the cost of a VM exit, and write `need_event` from memory.
- List the three bounds on a descriptor-chain walk and say what each one stops, individually.
- Say which parts of a virtio device `virtio-queue` provides and which remain device policy.

**What this unlocks upstream:** `virtio-queue` and `vm-virtio` review; Cloud Hypervisor's and
Firecracker's block, net and vsock device models, all of which are a queue plus a request format;
and most of rung 4's reading list, which is largely virtio device code.

---

## 2. Background from first principles

### 2.1 The problem virtio exists to solve

Rung 1 established two facts:

1. Guest memory is ordinary host memory. The host reads and writes it with a plain pointer, no
   syscall, no copy.
2. A VM exit that reaches the VMM costs **~1,610 ns** on this machine.

Naive device emulation ignores the first and pays the second. A guest driving an emulated IDE
controller does one port I/O per few bytes, and every one is an exit. At 1,610 ns each, a 4 KiB
transfer costs milliseconds of pure transition, moving no data.

Virtio is the design that falls out of taking both facts seriously:

> **Put the data where it is free to reach. Use the expensive thing only as a doorbell, and ring it
> as rarely as the protocol allows.**

Everything else - the ring layout, the descriptor chains, the two event fields - is machinery in
service of that sentence.

### 2.2 A virtqueue is not an object

It is an agreement about the meaning of bytes at three addresses in shared memory, plus two
doorbells. That is why `toy-virtq-raw` has no dependencies at all: there is nothing to depend on.

```
   descriptor table            avail ring (driver -> device)      used ring (device -> driver)
   16 bytes x queue_size       written by the driver              written by the device

   +---------------------+     +---------------------+            +---------------------+
 0 | addr len flags next |     | flags               | +0         | flags               | +0
   +---------------------+     | idx                 | +2         | idx                 | +2
 1 | addr len flags next |     +---------------------+            +---------------------+
   +---------------------+     | ring[0]  (le16)     | +4         | ring[0].id   (le32) | +4
 2 | addr len flags next |     | ring[1]             | +6         | ring[0].len  (le32) | +8
   +---------------------+     | ...                 |            | ring[1].id          | +12
   | ...                 |     | ring[qsize-1]       |            | ...                 |
   +---------------------+     +---------------------+            +---------------------+
   | addr len flags next |     | used_event   (le16) |            | avail_event  (le16) |
   +---------------------+     +---------------------+            +---------------------+
                                 written by driver,                 written by device,
                                 read by device                     read by driver
```

**The descriptor table is a pool, not a queue.** Its entries are not consumed in order and are not
ordered at all. The avail ring imposes order, by carrying the *index* of the head descriptor of each
chain the driver wants processed.

That indirection is what allows scatter-gather. One request can be a header in one buffer, a payload
in another and a status byte in a third, at unrelated addresses, linked by `next`. The device sees
one logical request; the guest never had to make it contiguous, and nothing was copied to arrange
it.

### 2.3 One request, end to end

```
   DRIVER (in the guest)                          DEVICE (in the VMM)

   1. write descriptors 0,1,2  ------ shared memory ------>
      desc[0] = "scatter ", NEXT->1
      desc[1] = "gather",   NEXT->2
      desc[2] = reply buf,  WRITE

   2. avail.ring[idx % 8] = 0        (the head)

   3. --------- RELEASE fence ---------

   4. avail.idx = 1                  ------------------->   5. read avail.idx  (ACQUIRE)
                                                            6. walk the chain from head 0
   7. kick? need_event(avail_event,...)                     7. gather 14 readable bytes
      -> a doorbell write = ONE VM EXIT                     8. do the work
                                                            9. write 14 bytes into desc[2]

                                                           10. used.ring[0] = (id=0, len=14)
                                                           11. ------ RELEASE fence ------
                          <---------------------------     12. used.idx = 1
  13. read used.idx (ACQUIRE)
  14. read used.ring[0]                                    13. interrupt? need_event(used_event,..)
  15. free descriptors 0,1,2
```

Note what is *not* in that diagram: any copy of the payload between guest and host, and any exit
except the doorbell at step 7 and the interrupt at step 13. The 14 bytes were written once, by the
driver, into memory the device could already see.

### 2.4 Free-running counters, and the bug everybody writes once

`avail.idx` and `used.idx` **count total entries ever published and are never reset.** They wrap at
65,536. The ring slot for entry `i` is `ring[i % queue_size]`.

There is no empty/full flag and no separate count. A consumer knows how much work is outstanding by
subtracting its own position from the published index, **in wrapping arithmetic**:

```rust
let outstanding = (Wrapping(avail_idx) - self.next_avail).0;   // right
let outstanding = avail_idx - self.next_avail;                 // wrong: underflows
if self.next_avail < avail_idx { /* work */ }                  // wrong: fails after 65,536 ops
```

The third form works perfectly for the first 65,536 requests and then stops seeing work forever. On
a queue doing 50,000 requests per second that is a hang after 1.3 seconds; on one doing 50 per
second it is a hang after 22 minutes, which is much worse, because it will not reproduce on demand.

`tests/roundtrip.rs` pushes 70,000 requests through specifically to cross that boundary.

### 2.5 Why the queue size must be a power of two

So that `i % queue_size` is a mask rather than a division. But also, less obviously, so the wrap
works out: a 16-bit counter wraps at 65,536, which is a multiple of any power-of-two queue size, so
the slot mapping stays consistent across the wrap. With a queue size of 100, entry 65,535 would map
to slot 35 and entry 0 to slot 0, and the ring would skip slots at every wrap.

### 2.6 Ordering, which is the part that bites

Two release/acquire pairs, one per direction:

```
   Driver publishing:    write descriptors ---> RELEASE ---> bump avail.idx
   Device consuming:     read avail.idx    ---> ACQUIRE ---> read descriptors

   Device completing:    write used elem   ---> RELEASE ---> bump used.idx
   Driver collecting:    read used.idx     ---> ACQUIRE ---> read used elem
```

Without them, the *index* can become visible before the data it advertises. The reader sees a
valid-looking index pointing at a descriptor that has not been written yet, and processes whatever
was in that slot the last time it was used - which is a real, well-formed-looking request from
several thousand operations ago.

This is the classic "virtio works on x86 and hangs on arm64" bug. x86 does not reorder stores with
respect to other stores, so the mistake is invisible; arm64 does, so it is not. The fences are in
`mem.rs` with the reasoning beside them. In this single-threaded study they are unobservable - they
are written because a real implementation needs them, and because the places they are needed are not
obvious.

### 2.7 `EVENT_IDX`, derived rather than described

Without it, each side signals every time: one doorbell exit per request submitted, one interrupt per
request completed. At 1,610 ns per doorbell, a queue doing 50,000 requests per second spends
**80 ms of every second** in doorbell exits alone - 8% of a core, moving no data.

`EVENT_IDX` lets each side publish a threshold the other reads:

- The **driver** writes `used_event` into the avail ring: *"do not interrupt me until `used.idx`
  passes this."*
- The **device** writes `avail_event` into the used ring: *"do not kick me until `avail.idx` passes
  this."*

And the predicate is **the same function in both directions**:

```rust
pub fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}
```

In words: *has the counter crossed `event + 1` since we last checked?* `old` is where the counter
stood at the previous check. All three subtractions are wrapping, and they must be: the terms are
*distances*, which stay meaningful across a 16-bit wrap where absolute comparisons do not.

Recognising that the driver's kick decision and the device's interrupt decision are one predicate
applied to two different pairs of counters is most of understanding the feature. Reading only the
device half makes `used_event` look like a magic number from nowhere, which is why `driver.rs`
exists in this rung at all.

The spec (VIRTIO 1.2, 2.7.7) states the simpler rule "signal when `idx == event + 1`". Every real
implementation uses the inequality instead, so that a *batch* of entries added between two checks
cannot step over the exact equality and lose the signal entirely. `layout.rs` has a test for that
case.

### 2.8 The adversarial part

A descriptor chain is **a linked list whose nodes and pointers are written entirely by the guest.**
The host must traverse it without trusting a single field. `desc[0].next = 0` is an infinite loop in
the VMM that costs the attacker two stores.

`ChainIter` carries three independent bounds. They are independent because they fail differently and
none subsumes the others:

| Bound | What it stops | What happens without it |
|---|---|---|
| `ttl`, starting at `queue_size`, decremented per descriptor | A chain longer than the table, i.e. a **cycle** | The host loops forever. Free denial of service. |
| `next_index < queue_size` | A `next` pointing **outside the table** | Reads 16 bytes from past the table - in a real VMM, other guest memory or another queue's rings |
| `yielded_bytes` under 2^32 (VIRTIO 1.2, 2.7.5.2) | A short, in-range chain claiming **enormous buffers** | 128 descriptors of 4 GiB each: the device is asked to move half a terabyte |

Plus one not in the spec but required in any real implementation: every descriptor's
`addr..addr+len` must be inside the region, checked with overflow-safe arithmetic. A check written
as `addr + len <= region_len` accepts `addr = u64::MAX - 8, len = 4096`, because the sum wraps to a
small number. `mem.rs` uses `checked_add`, and there is a test for that exact input.

And one ordering rule: all device-readable descriptors must precede all device-writable ones (VIRTIO
1.2, 2.7.5.3). A device that trusts this instead of checking can be made to write output into a
buffer the driver is still reading.

`cargo run -p toy-virtq-raw -- --hostile` feeds the device one of each:

```
  self-referential descriptor (desc[0].next = 0) -> chain longer than 8 descriptors: it contains a cycle
  two-descriptor cycle (0 -> 1 -> 0)             -> chain longer than 8 descriptors: it contains a cycle
  next points outside the table                  -> descriptor index 60000 is outside a table of 8
  buffer outside the shared region               -> buffer rejected: access of 4096 bytes at 0xfff...ff7
  device-readable after device-writable          -> device-readable descriptor after a device-writable one
  all rejected, all completed with length 0, host still running
```

"**and completed**" is the subtle part. A device that silently drops a malformed chain leaks a
descriptor: the driver waits forever for a completion that will not come, and the queue slowly stops
accepting work. The failure appears far from the cause. Completing it with length 0 tells the driver
the request produced nothing.

---

## 3. Results

**Provisional - laptop measurement.** Manifest:
[`results/env-umang-Inspiron-3501-2026-08-06.txt`](results/). Intel i5-1135G7, one NUMA node,
`powersave` with turbo, Linux 7.0.0. Release build from commit `39d0f66`, clean tree. Three runs of
200,000 samples each, as in rung 1, because the first thing worth knowing about a measurement is how
much it moves when nothing changes.

### 3.1 Descriptor-chain walk cost

| descriptors | p50 (r1/r2/r3) ns | p50 spread | p99 spread | ns per descriptor |
|---:|---|---:|---:|---:|
| 1 | 24 / 22 / 22 | 9% | 44% | 22.0 |
| 2 | 29 / 27 / 27 | 7% | 31% | 13.5 |
| 4 | 40 / 39 / 38 | 5% | 40% | 9.8 |
| 8 | 62 / 64 / 62 | 3% | 4% | 7.8 |
| 16 | 108 / 114 / 108 | 6% | 31% | 6.8 |

Raw samples: [`results/r{1,2,3}-*-walk.csv`](results/).

Fitting the p50 column: roughly **16 ns fixed + 6.1 ns per descriptor**. The per-descriptor cost is
a 16-byte read plus a bounds check plus three flag tests, which at ~2.4 GHz is about 15 cycles -
plausible, and dominated by the dependent load of the next descriptor rather than by the checks.

**The comparison that matters:**

> One VM exit (rung 1): **1,610 ns**.
> One 16-descriptor chain walk: **108 ns**.
>
> The host can walk **fifteen** full 16-descriptor chains in the time it takes to leave and re-enter
> the guest once. For a single-descriptor chain the ratio is **73:1**.

That ratio is the entire argument for the virtio design, and it is why the answer to "should the
device do more work per exit?" is always yes. It is also why `EVENT_IDX` is worth its complexity:
the notification, not the work, is the expensive part.

Note the noise structure is the mirror image of rung 1's. Here p50 is stable to 3-9% but p99 moves
by up to 44%, because these operations are ~20-100 ns - short enough that a single scheduler tick or
cache miss dominates a tail sample. As in rung 1: **p50 is the number to build on; p99 on this
machine is not.**

### 3.2 `EVENT_IDX` notification suppression

Not a timing. The protocol is deterministic, so for a given interleaving the kick count is *exact*
and reproducible rather than measured. 4,096 requests per configuration, queue size 128.

| batch | kicks, `EVENT_IDX` off | kicks, on | suppressed | interrupts off | interrupts on |
|---:|---:|---:|---:|---:|---:|
| 1 | 4096 | 4096 | **0%** | 4096 | 4096 |
| 2 | 4096 | 2048 | 50% | 2048 | 2048 |
| 4 | 4096 | 1024 | 75% | 1024 | 1024 |
| 8 | 4096 | 512 | 88% | 512 | 512 |
| 16 | 4096 | 256 | 94% | 256 | 256 |
| 32 | 4096 | 128 | 97% | 128 | 128 |
| 64 | 4096 | 64 | **98%** | 64 | 64 |

Exactly one kick per batch, which is the result the mechanism is designed to produce.

**Modelled** saving on the kick path, at rung 1's 1,610 ns per exit - modelled is the right word,
because no VM exit was performed here:

| batch | kicks suppressed | guest CPU not spent exiting |
|---:|---:|---:|
| 8 | 3,584 | 5.77 ms |
| 64 | 4,032 | 6.49 ms |

That is over 4,096 requests. Scaled to a queue sustaining 50,000 requests per second at batch 8, it
is **~79 ms per second of a core** not spent on doorbell exits.

### 3.3 Two honest negatives

**`EVENT_IDX` saves nothing at batch 1.** When the queue is idle enough that each request is
submitted and drained alone, every request still needs its kick, and the feature costs an extra read
of `avail_event` for no benefit. It is a mechanism for queues under load, and the first row of the
table says so.

**The interrupt columns show no difference at all.** Not a bug in the measurement - a property of
the interleaving. The device already batches: it adds every completion of a drain before deciding
once whether to interrupt. Batching alone coalesces the interrupts, and `EVENT_IDX` adds nothing on
top. Its interrupt-side benefit needs a driver that is polling or already awake, which this
simulation does not model. So the honest summary is: **in this interleaving, `EVENT_IDX`'s entire
benefit is on the kick path.** Exercise 8 is the interleaving that would show the other half.

---

## 4. What was found on the way: a bug in `virtio-queue`'s mock framework

The two implementations produce identical bytes, but their *interrupt decisions* disagreed - the raw
device says `true`, the crate-based one says `false`, for the same three completions.

The cause is a layout bug in `virtio-queue`'s `mock.rs` (`test-utils` feature), verified against
0.18.0 from crates.io on 2026-08-06:

```rust
// SplitQueueRing::end()
self.start()                       // = ring.addr, i.e. base + 4, skipping flags and idx
    .checked_add(self.ring.len)    // ring.len is an ELEMENT COUNT, added as if it were bytes
```

For an avail ring of N entries the true size is `4 + 2N + 2` bytes; `end()` reports `4 + N`. And
`MockSplitQueue::create` derives the used ring's address from `avail.end()`. So **for every queue
size, the used ring is placed inside the avail ring.**

With `queue_size = 8`: avail at 128, used at 140, where used should be at 150.

Two consequences, both reproduced in
[`toy-virtq-crates/tests/mock_layout.rs`](toy-virtq-crates/tests/mock_layout.rs):

1. The avail ring's `used_event` field (offset 148) *is* the used ring's `ring[0].len` field. So
   `needs_notification` reads a completion length where a threshold should be. That is precisely the
   discrepancy that led here: `used_event` read back as `12`, the length of `"HELLO VIRTIO"`.
2. Writing one used element **zeroes avail entries 6 and 7**:
   ```
   avail before: [aa00, aa01, aa02, aa03, aa04, aa05, aa06, aa07]
   avail after : [aa00, aa01, aa02, aa03, aa04, aa05, 0000, 0000]
   ```

Proposed fix - count bytes, and include the header and the trailing event field:

```rust
pub fn end(&self) -> GuestAddress {
    self.ring.addr
        .checked_add((self.ring.len * size_of::<T>()) as GuestUsize)
        .and_then(|a| a.checked_add(size_of::<u16>() as GuestUsize))
        .unwrap()
}
```

**Scope, stated carefully:** `mock.rs` is behind `test-utils`, so this affects test harnesses, not
any shipping VMM. It is still worth fixing - a harness that silently corrupts the structure under
test is worse than no harness, and it hides exactly the class of bug (`EVENT_IDX` behaviour, ring
wrap) that a harness exists to catch. The demo above produced correct bytes only because it
publishes three chains and the corruption starts at avail entry 6.

The tests are `#[ignore]`d so `cargo test --workspace` stays green. Run them with:

```
cargo test -p toy-virtq-crates --test mock_layout -- --ignored --nocapture
```

If either starts passing, upstream has fixed it and the file should be deleted.

---

## 5. Relation to Cloud Hypervisor, Firecracker and rust-vmm

| This rung | Upstream |
|---|---|
| `layout.rs` - ring offsets, alignment, `Descriptor` | `virtio-queue`'s `defs.rs` and `desc/split.rs` |
| `ChainIter` with its three bounds | `virtio-queue`'s `DescriptorChain::next` - same `ttl`, same index check, same 2^32 cap |
| `Device::needs_notification` | `virtio-queue`'s `needs_notification`: same expression, same Linux provenance |
| `Device::enable_notification`'s double-check | `virtio-queue`'s `enable_notification` return value |
| `mem.rs` bounds checking on guest-supplied addresses | `vm-memory`'s `GuestAddress` and its checked accessors |
| `driver.rs` | Linux's `drivers/virtio/virtio_ring.c`. A VMM never contains this half, which is why a partial model is easy to end up with. |
| `Device::process` - gather, work, scatter, `add_used` | Cloud Hypervisor `virtio-devices/src/block.rs`; Firecracker `src/vmm/src/devices/virtio/block/` and `.../vsock/` |

The important structural point: **`virtio-queue` gives you the queue, not the device.** The gather,
the request format, the response, and `used.len` are yours, and that is where the bugs that matter
live. `toy-virtq-crates` is barely shorter than the raw device's `process()` for exactly that
reason - what the crate removed was the layout and the bounds, which is the part that is hard to get
right and boring once it is.

One difference worth carrying into review: upstream's `DescriptorChain` is
`Iterator<Item = Descriptor>` and ends iteration on any error via `.ok()?`. Safe, and what callers
want - but it makes a malformed chain indistinguishable from a short one, so a device model cannot
report that a guest sent something illegal, nor count how often it happens. `ChainIter` here yields
`Result` instead. Whether upstream should is a real question, recorded in
[`../docs/OPEN-QUESTIONS.md`](../docs/OPEN-QUESTIONS.md).

---

## 6. References

- [VIRTIO 1.2 specification](https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html),
  chapter 2.7 "Split Virtqueues". Normative for everything in `layout.rs`.
- Linux `drivers/virtio/virtio_ring.c`, `vring_need_event()` - the origin of the inequality form of
  the notification predicate.
- [`virtio-queue`](https://github.com/rust-vmm/vm-virtio/tree/main/virtio-queue): `queue.rs` (1,578
  lines), `chain.rs` (563), `mock.rs` (525).
- Cloud Hypervisor `virtio-devices/`, Firecracker `src/vmm/src/devices/virtio/` - the same queue
  under real device models.
- Rung 1's [`README.md`](../rung-01-kvm/README.md#results) for the exit cost every number here is
  denominated against.

---

## 7. The rest of this rung

- [`CODE_WALKTHROUGH.md`](CODE_WALKTHROUGH.md) - the code in execution order.
- [`EXERCISES.md`](EXERCISES.md) - modifications to implement, easy to hard.
- [`GATE.md`](GATE.md) - the comprehension gate.
- [`COMMON-MISTAKES.md`](COMMON-MISTAKES.md) - misconceptions, including the two this rung hit.
