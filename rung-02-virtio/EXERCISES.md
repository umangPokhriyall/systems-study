# Exercises

Ordered easy to hard. Each states what it teaches, because an exercise that only produces working
code has been wasted.

Status is recorded honestly. "not done" is a legitimate final state.

| # | Exercise | Status |
|---|---|---|
| 1 | Make the device do something else | not done |
| 2 | Report the real chain length | not done |
| 3 | Break `used.len` on purpose | not done |
| 4 | Remove one bound at a time | not done |
| 5 | Implement `VIRTQ_AVAIL_F_NO_INTERRUPT` | not done |
| 6 | Add a virtio-blk style request header | not done |
| 7 | Implement indirect descriptors | not done |
| 8 | Model the interleaving where `EVENT_IDX` helps the interrupt path | not done |
| 9 | Fix the `mock.rs` layout bug and send it upstream | not done |
| 10 | Make the two halves run in real threads | not done |
| 11 | Fuzz the chain walker | not done |
| 12 | Implement a packed virtqueue | not done |

---

### 1. Make the device do something else

Replace uppercasing with something that produces a different-length output - run-length encoding,
say. Watch what happens to `used.len` and to the reply the driver reads back.

*Teaches:* that `used.len` is the contract for how much of the buffer is valid, and that it is the
device's job to get it right.

### 2. Report the real chain length

`ProcessStats.descriptors` counts descriptors across all chains. Add a per-chain histogram and print
it. Then make the demo submit chains of varying length and check the histogram matches.

*Teaches:* the shape of the instrumentation every real device model has, and the first step toward
the kind of metric Cloud Hypervisor's performance suite is missing.

### 3. Break `used.len` on purpose

Change `add_used` to report the *buffer size* instead of bytes written. The demo will still pass its
output check - the first 12 bytes are still correct.

Then make the driver print the whole `used.len` bytes and see what leaks out of the buffer.

*Teaches:* the most common information-disclosure bug in device models, from the inside. Note
carefully that the existing output assertion does **not** catch it, and work out what assertion
would.

### 4. Remove one bound at a time

Comment out the `ttl` check and run the self-referential chain. Then restore it, comment out the
index check, and run the out-of-table chain. Then the `yielded_bytes` check with the 4 GiB chain.

Predict each failure mode before running it.

*Teaches:* that the three bounds are genuinely independent and none subsumes the others - which is
much more convincing after watching each one fail in its own way than after reading that sentence.

### 5. Implement `VIRTQ_AVAIL_F_NO_INTERRUPT`

The coarse, pre-`EVENT_IDX` mechanism: the driver sets a flag in `avail.flags` meaning "no
interrupts at all". Implement it in the device and add it to the notification benchmark as a third
configuration.

*Teaches:* what `EVENT_IDX` improved on, and why an all-or-nothing switch is not good enough for a
driver that alternates between polling and sleeping.

### 6. Add a virtio-blk style request header

Real devices do not take raw bytes. virtio-blk's chain is: a 16-byte readable header
(`type`, `reserved`, `sector`), then readable or writable data, then a **1-byte writable status**.
Implement that shape, with `VIRTIO_BLK_S_OK` / `VIRTIO_BLK_S_IOERR`.

Note that `used.len` should then count the data *and* the status byte, and check what Cloud
Hypervisor actually does.

*Teaches:* the gap between "a queue" and "a device". This is the shape of every block request in
every VMM, and it is the direct prerequisite for reading `virtio-devices/src/block.rs` in rung 4.

### 7. Implement indirect descriptors

`VIRTQ_DESC_F_INDIRECT`: one descriptor whose buffer *is* a descriptor table. It lets a chain exceed
the queue size without consuming table entries.

Then find the two guards upstream needs and this rung avoided by not implementing it: an indirect
table may not contain another indirect descriptor, and the table length must be a multiple of the
descriptor size. Read `switch_to_indirect_table` in `virtio-queue`'s `chain.rs` afterwards, not
before.

*Teaches:* how a recursive walk stays bounded, and why the spec forbids setting `INDIRECT` and
`NEXT` together.

### 8. Model the interleaving where `EVENT_IDX` helps the interrupt path

README §3.3 reports an honest negative: the interrupt columns show no benefit, because the device
already batches its completions. Build the interleaving that would show it - a driver that is awake
and polling, arming `used_event` far ahead so it receives no interrupts at all, versus one that
sleeps.

*Teaches:* that a measurement showing no effect is usually a statement about the workload, not the
mechanism - and that identifying which is a skill, not a formality.

### 9. Fix the `mock.rs` layout bug and send it upstream

The bug in README §4, the reproducer in `toy-virtq-crates/tests/mock_layout.rs`, and the proposed
fix are all already written. What remains is the upstream process: fork `vm-virtio`, write the fix
with a regression test in their style, a DCO `Signed-off-by`, a commit message in their format, and
a PR description that leads with the failure rather than the diff.

*Teaches:* the actual mechanics of a first contribution, on a change small enough that the process
is the only hard part. Check whether any existing `virtio-queue` test changes behaviour once the
rings stop overlapping - a test that was passing for the wrong reason is the likeliest complication.

### 10. Make the two halves run in real threads

Put `SharedMem` behind something that permits concurrent access, run the driver and the device on
separate threads, and see whether the fences in `mem.rs` are in the right places.

Then remove one and run it under `loom` or on an aarch64 machine.

*Teaches:* whether the ordering discipline was understood or merely transcribed. This is the
exercise that would turn the memory-ordering section of the gate from theory into evidence.

### 11. Fuzz the chain walker

Feed `ChainIter` random bytes as a descriptor table and assert only that it terminates and never
panics. `cargo-fuzz`, or a simple loop with a seeded RNG.

`vm-virtio` has a `fuzz/` directory - compare targets afterwards.

*Teaches:* the standard defence for exactly this class of code, and the fastest way to find the
bound that was forgotten.

### 12. Implement a packed virtqueue

The VIRTIO 1.1 alternative layout: one array instead of three rings, with a wrap counter and
per-descriptor availability flags, designed for better cache behaviour.

Then measure it against the split ring with `bench_walk` and find out whether the cache argument
survives contact with a number.

*Teaches:* that a design justified by cache behaviour needs a measurement, and gives one of the few
places in this study where a genuine A/B comparison is possible on a laptop.
