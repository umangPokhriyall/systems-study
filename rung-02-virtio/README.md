# Rung 2 - virtio from first principles

**Not started.** Placeholder, so the repository structure is visible before the content exists.

## What will land here

A descriptor-chain consumer built against `vm-virtio/virtio-queue`'s own `mock.rs` framework, which
exists for exactly this purpose. It will walk a chain, handle a chained read/write pair, update the
used ring correctly, and exercise `EVENT_IDX` notification suppression. Then `virtio-queue`'s
`queue.rs` and `chain.rs` get read with the toy in hand rather than cold.

## Why it comes after rung 1

`EVENT_IDX` exists entirely to avoid VM exits. Meeting it before knowing what an exit costs teaches
the mechanism and hides the motive. Rung 1 measured that cost (~1.6 µs on this machine), so the
arithmetic behind the design is available before the design is.

## The gate question already chosen

Explain why a malicious guest cannot make the host walk a descriptor chain forever, citing the
specific check in `virtio-queue` that prevents it. Descriptor chains are guest-controlled data
structures that the host must traverse without trusting them, which makes this the first genuinely
adversarial surface in the ladder.
