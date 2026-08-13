# Rung 4 - reading production code, with a written map each

**In progress.** Item 1 of the reading list below is written.

## What is here

- [`cloud-hypervisor-block-io-study.md`](cloud-hypervisor-block-io-study.md) - Cloud Hypervisor's
  block I/O path from the virtio-blk device model down to the AIO and io_uring backends, written
  against upstream `1af93ac70` (2026-08-11). Covers the production request path, the `AsyncIo`
  abstraction, both backends from first principles, why the existing AIO drain benchmark cannot be
  copied line-for-line for io_uring, and the `performance-metrics` harness. Ends with the
  comprehension gates for the proposed OSS-0 contribution.

## What will land here

One `<subsystem>-map.md` per target area, each recording: the data flow, the hot path, where a lock
or a copy lives, and the one question the code did not answer. Reading order is chosen by what a
specific contribution needs, not by curiosity:

1. `cloud-hypervisor/block/src/io/async_io/` - the io_uring and AIO backends.
2. `cloud-hypervisor` memory-manager restore path, `memory_restore_mode`, and the prefault threads.
3. `firecracker/src/vmm/src/devices/virtio/vsock/` - the muxer and its drain loop.
4. `firecracker` jailer and seccomp filters, read-only.

## Why it comes fourth

Reading a 1,500-line virtqueue implementation before rung 2 produces notes; reading it after rung 2
produces questions. The difference is the entire value of the exercise.

Unanswered questions from each map go to [`../docs/OPEN-QUESTIONS.md`](../docs/OPEN-QUESTIONS.md),
which is the pipeline for well-formed upstream questions.
