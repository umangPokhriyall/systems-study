# Rung 4 - reading production code, with a written map each

**Not started.** Placeholder, so the repository structure is visible before the content exists.

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
