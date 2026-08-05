# Code walkthrough

The code in execution order. Unlike rung 1 there are no syscalls here at all - a virtqueue is pure
shared-memory protocol - so the equivalent of "explain every ioctl" is "explain every field access
and every ordering constraint".

```
toy-virtq-raw/src/
  mem.rs       the shared region, and the bounds checking guest-supplied addresses need
  layout.rs    where the three rings live, what each field means, and need_event
  driver.rs    the guest half: build chains, publish, decide whether to kick
  device.rs    the VMM half: walk chains without trusting them, complete, decide whether to interrupt
  main.rs      demo, hostile-chain cases, and the two measurements
  ../tests/roundtrip.rs   21 tests, most about malformed input
toy-virtq-crates/src/
  main.rs      the device half on virtio-queue + vm-memory
  ../tests/mock_layout.rs the upstream bug found while comparing the two
```

---

## Part 0 - `mem.rs`: why every access returns a `Result`

`SharedMem` is a `Vec<u8>` indexed by guest physical address. In a real system it is the `mmap` rung
1 handed to KVM: the driver in the guest writes it with ordinary stores, and the device outside the
guest reads it by dereferencing a host pointer into the same physical pages.

The interesting part is `range()`, which every accessor funnels through:

```rust
let end = addr.checked_add(len).ok_or(OutOfBounds { .. })?;
if end.0 > self.len() { return Err(OutOfBounds { .. }); }
```

`checked_add` rather than `addr + len`. A descriptor's `addr` and `len` are attacker-controlled 64-
and 32-bit integers, and `addr + len` with `addr` near `u64::MAX` wraps to a small number - which
turns a bounds check into a bounds *pass*. `mem::tests::address_overflow_does_not_wrap_into_a_pass`
covers exactly that.

`GuestAddr` is a newtype rather than a bare `u64` so a guest-supplied value cannot be passed where a
host offset is expected without saying so. `vm-memory`'s `GuestAddress` exists for the same reason.

### The three accessor flavours

There is one bounds check but three ordering disciplines, and which one a field gets is a design
decision, not a detail:

| Accessor | Used for | Why |
|---|---|---|
| `load_idx_acquire` | reading `avail.idx`, `used.idx` | An acquire after the read makes every store the producer made *before* publishing the index visible |
| `store_idx_release` | writing `avail.idx`, `used.idx` | A release before the write guarantees the data is visible before the index advertising it |
| `load_relaxed` | reading `used_event`, `avail_event` | No ordering needed: these are *hints*. A stale read costs at most one unnecessary notification, never correctness. |

That last row is a deliberate property of the design and worth noticing. The notification thresholds
are advisory, so they can be read without synchronising - which is what makes `EVENT_IDX` nearly
free to evaluate.

In this single-threaded study the fences are unobservable. They are written where a real
implementation needs them because those places are not obvious, and getting them wrong produces a
bug that appears once in a few million operations on one machine and never on another.

---

## Part 1 - `layout.rs`: the agreement

### `VirtqLayout::new`

Computes three addresses from a base and a queue size:

```
desc_table = align_up(base, 16)
avail_ring = align_up(desc_table + 16*qs, 2)
used_ring  = align_up(avail_ring + 4 + 2*qs + 2, 4)
```

The `+ 4` is `flags` and `idx`; the `+ 2*qs` is the ring itself; the trailing `+ 2` is `used_event`.
**Forgetting that trailing field is the upstream bug in section 4 of the README** - it is easy to
miss because it lives at the far end of a structure whose name does not mention it.

Alignment is 16 / 2 / 4, which is the natural alignment of the largest field in each structure so
that no field straddles a boundary in a way that would make an access non-atomic on some
architecture. `layout::tests::layout_alignment_is_satisfied` starts from a deliberately misaligned
base (3) to check the `align_up`, and `rings_do_not_overlap` asserts what upstream's mock gets
wrong.

`new` panics unless the queue size is a power of two in `1..=32768`. See README §2.5 for why that is
not merely an optimisation.

### The accessors

`avail_slot(i)` and `used_slot(i)` take a *free-running counter* and apply `% queue_size`. Every
call site passes the counter, never a pre-modulated index, so there is one place where the wrap is
expressed. `layout::tests::ring_slots_wrap_at_queue_size_not_at_the_counter` pins that down.

`used_event()` lives at the end of the **avail** ring and `avail_event()` at the end of the **used**
ring. That looks backwards until you learn the rule: **a field lives in the ring written by whoever
writes the field.** The driver writes `used_event`, so it is in the driver's ring.

### `need_event`

```rust
pub fn need_event(event: u16, new: u16, old: u16) -> bool {
    new.wrapping_sub(event).wrapping_sub(1) < new.wrapping_sub(old)
}
```

One function, called from both `Driver::needs_kick` and `Device::needs_notification`. Five tests
cover it: the exact crossing, the case where it must not fire twice, the 16-bit wrap, and a batch
of six entries stepping over the target - which the spec's simpler `idx == event + 1` rule would
miss entirely.

---

## Part 2 - `driver.rs`: publishing

### `add_chain`, in four steps

```
1. write every descriptor          (desc[i].addr/len/flags/next)
2. write the avail ring slot        avail.ring[avail_idx % qs] = head
3. ---------- RELEASE ----------
4. bump avail.idx
```

Only step 4 makes the chain visible. If steps 1-2 could be reordered after step 4, the device would
read an index advertising a descriptor that has not been written and would process whatever was in
that slot from the last time it was used.

Two details in the descriptor loop:

- `VIRTQ_DESC_F_NEXT` is set on every descriptor except the last, and `next` names the following
  index. The chain is a linked list, not a run of consecutive entries - the indices come off a free
  list and need not be adjacent.
- `VIRTQ_DESC_F_WRITE` marks device-writable. The driver is making a promise about its own memory;
  the device checks the promise rather than trusting it, because in a real system these two halves
  do not trust each other.

`add_chain` also asserts readable-before-writable. A *driver* that violates the ordering is simply
broken, so it asserts; the *device* returns an error for the same condition, because it may not
assume a well-behaved driver. Same rule, two enforcement styles, for a reason.

### `outstanding`, and the leak that is easy to write

The used ring reports only the **head** index. A driver that frees only that leaks every other
descriptor in every multi-descriptor chain, and the queue silently stops accepting work after a
while - with the failure appearing far from the cause.

`Driver` keeps `HashMap<head, Vec<descriptor_index>>` and returns the whole chain in `collect_used`.
A real driver does the same thing more cheaply, by threading the free list through the unused
descriptors' own `next` fields. `tests::every_descriptor_returns_to_the_free_list` runs 20 cycles
and asserts the free list returns to full size each time.

### `needs_kick` and `arm_used_event`

`needs_kick` is `need_event(avail_event, avail_idx, last_kick)`, updating `last_kick` each call so
`old` is always "where the counter stood at the previous decision". It is called **per submission**,
not per batch, because a real driver does not know a burst is coming.

`arm_used_event` writes `used_event = last_used`, meaning "interrupt me when one more completion
arrives". A polling driver sets it far ahead instead and receives no interrupts at all - which is
how a busy-polling driver and an interrupt-driven one use the same mechanism with no special case.

---

## Part 3 - `device.rs`: consuming, without trusting

### `available` and `pop_chain_head`

```rust
let avail_idx = Wrapping(mem.load_idx_acquire(self.layout.avail_idx())?);
Ok((avail_idx - self.next_avail).0)
```

Wrapping subtraction, never comparison. See README §2.4.

### `ChainIter::next`, the security-critical function

In order:

1. `if self.ttl == 0` -> `TooLong`. Bounds the **length** of the walk; catches cycles.
2. `self.layout.desc(self.next_index)` returns `None` if the index is out of range -> `IndexOutOfRange`.
   Bounds **where** the walk may go.
3. Read the 16-byte descriptor.
4. `desc.is_indirect()` -> `Indirect`. Not implemented here; upstream recurses into an indirect
   table, guarding against nesting with an `is_indirect` flag.
5. `self.yielded_bytes.checked_add(desc.len)` -> `TooManyBytes`. Bounds the **work**. `checked_add`
   rather than a comparison against a limit, because it is the sum that overflows, and a wrapped sum
   compares fine against any limit.
6. Direction ordering: once a writable descriptor is seen, a later readable one is
   `ReadableAfterWritable`.
7. `if desc.has_next()` -> follow and decrement `ttl`; else terminate.

Each of the three bounds has its own test with its own malformed chain, because a single "rejects
bad input" test would pass with any one of them removed.

**Where this differs from upstream.** `virtio-queue`'s `DescriptorChain` is
`Iterator<Item = Descriptor>` and ends iteration on any error via `.ok()?`. Safe, and what callers
want. It also makes a malformed chain indistinguishable from a short one, so a device model cannot
report or count illegal input. `ChainIter` yields `Result` instead.

### `add_used`

```rust
mem.write_u32(id_addr, head as u32)?;
mem.write_u32(len_addr, len)?;
self.next_used += Wrapping(1);
self.num_added += Wrapping(1);
mem.store_idx_release(self.layout.used_idx(), self.next_used.0)
```

`len` is **bytes actually written**, not the size of the writable buffers the driver provided. The
driver takes this as the length of valid data, so an over-report hands the guest whatever was in the
buffer before - which, in a VMM that reuses buffers, is another guest's data. The demo deliberately
provides a 64-byte reply buffer for a 12-byte reply so this is visible in the trace as `len=12`.

`num_added` exists because a device may add several completions before deciding whether to
interrupt, and the decision must consider all of them.

### `needs_notification`

```rust
let used_event = mem.load_relaxed(self.layout.used_event())?;
let old = self.next_used - self.num_added;
self.num_added = Wrapping(0);
Ok(need_event(used_event, self.next_used.0, old.0))
```

`old` is reconstructed by subtracting the number added since the last call, which is what makes the
batch case work. It resets `num_added`, so it must be called exactly once per batch - the answer is
about the whole batch, not the last completion.

### `enable_notification`, and the race it closes

```rust
mem.write_u16(self.layout.avail_event(), self.next_avail.0)?;
fence(SeqCst);
Ok(self.available(mem)? != 0)
```

The **return value** is the point. Between the device deciding it has drained the queue and actually
re-enabling notifications, the driver may add a chain and skip the kick because notifications were
still disabled. The device would then sleep with work pending and nothing would wake it.

The fix is to re-check *after* enabling. `SeqCst` rather than release/acquire because the store must
not be reordered after the load - a store-load pair is the one case release/acquire does not cover,
and it is exactly the case here.

`disable_notification` has an empty branch when `EVENT_IDX` is on, which reads like an omission
until you see why: the mechanism is already one-shot. Once the driver has been told about index N it
will not kick again until N is passed. Upstream's `set_notification` has the same empty branch with
the same comment.

### `process`

Per chain: **collect the whole chain first, then act.** A device that acted as it walked would
already have written to buffers by the time it discovered the chain was malformed.

Then gather the readable descriptors into one request, call the work closure, and scatter the
response across the writable descriptors with `min(d.len, remaining)`. Truncating is correct;
overrunning because the response was longer than the driver's buffer is a host-controlled write into
guest memory at an offset the guest never agreed to.

Rejected chains are **still completed**, with length 0. See README §2.8.

Rejections are *returned* in `ProcessStats.rejected`, not logged. A device model is a library: it
reports what happened and the VMM decides whether that is a debug line, a metric, or grounds for
stopping the guest.

---

## Part 4 - `main.rs`

`demo()` publishes three requests, one of them split across two readable descriptors, and traces
every ring transition. It asserts three things, each catching a different class of bug: the output
bytes, that the free list returns to full size, and that no well-formed chain was rejected.

`hostile_chains()` writes descriptors **directly, bypassing the driver**, because the driver would
refuse to build them - which is the point. A hostile guest is not running the driver.

`bench_walk()` times `ChainIter` traversal for chains of 1, 2, 4, 8 and 16 descriptors, 200,000
samples each. `black_box` around the accumulator and the total, because the loop has no other effect
and without it LLVM is entitled to delete the whole thing and time nothing.

`bench_notification()` is a *count*, not a timing, and the code says so. It runs the same workload
twice, once with `EVENT_IDX` off and once on, and counts kicks and interrupts. The saving is then
**modelled** at rung 1's 1,610 ns - only on the kick path, because an interrupt into the guest is a
different mechanism whose cost has not been measured here.

`NOTIFY_QUEUE_SIZE` is `2 * MAX_BATCH`. Two descriptors per request, and a whole burst must fit
before the device drains it, because the driver cannot reclaim a descriptor until its completion
comes back. A queue too small would force a kick per request by running out of descriptors, quietly
turning the experiment into a measurement of queue depth.

---

## Part 5 - `toy-virtq-crates/src/main.rs`

Same device, on `virtio-queue` and `vm-memory`, with `MockSplitQueue` standing in for the driver.

```rust
for chain in queue.iter(&mem)?.collect::<Vec<_>>() {
    let head = chain.head_index();
    for d in chain.clone().readable()  { /* gather */ }
    for d in chain.clone().writable()  { /* scatter */ }
    queue.add_used(&mem, head, written)?;
}
```

`queue.iter()` replaces `pop_chain_head` plus `ChainIter`, and carries the same three bounds inside
it. `.collect()` first because the iterator borrows the queue and `add_used` needs it mutably.
`.clone()` on each pass because `readable()` and `writable()` each consume the chain - a small
friction that exists because a chain is an iterator over guest memory, not a collection.

**What the crate provides:** the layout, the walk with its bounds, wrapping arithmetic throughout,
`needs_notification`/`enable_notification`, and `MockSplitQueue`.

**What it does not:** everything about *this device* - the request format, the gather, the response,
and `used.len`. That is the whole body above, and it is where the bugs that matter live. Note that
the crate version is barely shorter than the raw `process()`; what disappeared was `layout.rs` and
`ChainIter`, which is the hard-to-get-right, boring-once-done part.

Two checks the raw crate performs that the crate version does **not** get for free: buffer bounds
(only if the device actually goes through `vm-memory`'s accessors rather than doing its own pointer
arithmetic) and readable-before-writable ordering, which `DescriptorChain` does not enforce.

---

## Part 6 - `toy-virtq-crates/tests/mock_layout.rs`

The upstream bug, with a reproducer. `used_ring_overlaps_avail_ring` asserts the rings are disjoint
for five queue sizes; `used_ring_writes_corrupt_available_entries` fills the avail ring with a
pattern, writes one used element, and shows entries 6 and 7 zeroed.

Both are `#[ignore]`d so `cargo test --workspace` stays green, with the run command in the doc
comment. If either starts passing, upstream has fixed it and the file should be deleted.
