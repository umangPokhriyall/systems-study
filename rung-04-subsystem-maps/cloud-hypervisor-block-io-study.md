# Cloud Hypervisor block I/O: a subsystem map

Rung 4 subsystem map. Written 2026-08-13 against Cloud Hypervisor upstream `main` at
**`1af93ac7035cda77cd87b0c18b1134ebb0928052`** (2026-08-11).

**Naming note.** The rung-4 README specifies `<subsystem>-map.md`. The prompt that commissioned this
asked for `cloud-hypervisor-block-io-study.md`. This file uses the requested name inside the
conventional directory. It is the "block I/O backends" entry from that README's reading list, item 1.

**Purpose.** To make the proposed `micro_bench_uring_drain` contribution (OSS-0) evaluable on its
engineering merits rather than on plausibility. Everything here exists to support that judgement.
It is not a tour of Cloud Hypervisor.

**Reading contract.** Every claim is tagged when it is not simply "the code says so":

| Tag | Meaning |
|---|---|
| *(no tag)* | Read directly from the source at the commit above. File and line given. |
| **[KERNEL]** | Established by Linux/io_uring/libaio documented behaviour, not by this repository's source. |
| **[INFER]** | My inference from the code. Reasoning given. Could be wrong. |
| **[VERIFY]** | Not yet established. Belongs to the experimental stage, not this one. |

**Two corrections to earlier notes in this program**, recorded here so they do not propagate:

1. The enum variant is **`RawBackend::IoUring`**, not `RawBackend::Uring`
   (`block/src/formats/raw/mod.rs:42`). Earlier roadmap text used the wrong name.
2. `OSS-CONTRIBUTION.md` §1.2 finding C says the io_uring backend has no micro benchmark. That is
   **wrong as stated**. Twenty of the forty micro benchmarks drive io_uring through the qcow2 async
   engine (Part 12). What genuinely does not exist is any micro benchmark on the *raw* io_uring
   backend, and any benchmark anywhere that isolates io_uring *completion reaping*. The narrower
   claim is the true one and it is the one the contribution rests on.

---

## Table of contents

1. [Where this contribution fits](#part-1--where-this-contribution-fits)
2. [Trace of a 4 KiB buffered write](#part-2--trace-of-a-4-kib-buffered-virtio-blk-write)
3. [The `AsyncIo` abstraction](#part-3--the-asyncio-abstraction)
4. [AIO from first principles](#part-4--aio-from-first-principles)
5. [io_uring from first principles](#part-5--io_uring-from-first-principles)
6. [AIO versus io_uring](#part-6--aio-versus-io_uring)
7. [The existing AIO benchmark](#part-7--the-existing-aio-benchmark)
8. [Why the io_uring benchmark is non-trivial](#part-8--why-the-io_uring-benchmark-is-non-trivial)
9. [`in_flight` and memory lifetime](#part-9--in_flight-and-memory-lifetime)
10. [The production hot path](#part-10--the-production-hot-path)
11. [performance-metrics architecture](#part-11--performance-metrics-architecture)
12. [The existing qcow io_uring benchmarks](#part-12--the-existing-qcow-io_uring-benchmarks)
13. [How the architecture lets us answer the 10 comprehension gates](#part-13--how-the-architecture-lets-us-answer-the-10-comprehension-gates)
14. [Source map](#part-14--source-map)
15. [Rung 4 comprehension checklist](#part-15--rung-4-comprehension-checklist)

---

## Part 1 - Where this contribution fits

### 1.1 The stack, with responsibilities

```
┌───────────────────────────────────────────────────────────────────────────┐
│ GUEST KERNEL                                                              │
│   drivers/block/virtio_blk.c                                              │
│   Responsibility: turn a bio into a virtio-blk request; place descriptors  │
│   in the descriptor table; publish the head index in the AVAILABLE ring;   │
│   kick (write the notify register) unless suppressed by EVENT_IDX.         │
└───────────────────────────────┬───────────────────────────────────────────┘
                                │  guest-physical memory + one MMIO/PIO write
                                ▼
┌───────────────────────────────────────────────────────────────────────────┐
│ KVM                                                                       │
│   The notify write is trapped. Because CH registers an ioeventfd, KVM      │
│   does not exit to userspace with a full KVM_EXIT_MMIO: it writes an       │
│   eventfd and resumes the vCPU. This is the rung-1 exit you measured,      │
│   converted into a cheap fd signal.                                        │
└───────────────────────────────┬───────────────────────────────────────────┘
                                │  eventfd becomes readable
                                ▼
┌───────────────────────────────────────────────────────────────────────────┐
│ CLOUD HYPERVISOR - virtio device model      virtio-devices/src/block.rs    │
│   A dedicated per-queue worker thread blocked in epoll_wait.               │
│   Responsibility: virtqueue mechanics. Walk the descriptor chain, enforce  │
│   the spec's read-only/write-only rules, parse the virtio-blk header, drive│
│   submission, and on completion write the status byte, add_used, and       │
│   decide whether to interrupt the guest.                                   │
│   THIS IS RUNG 2, WITH A REAL DEVICE ON THE OTHER SIDE.                    │
└───────────────────────────────┬───────────────────────────────────────────┘
                                │  Request::execute_async(..., disk_image, ...)
                                ▼
┌───────────────────────────────────────────────────────────────────────────┐
│ block CRATE - request layer                 block/src/io/request.rs        │
│   Responsibility: translate "virtio-blk request" into "host I/O            │
│   operation". Sector -> byte offset. Guest descriptor list -> iovec list.  │
│   Alignment decision: pass guest memory straight to the kernel, or bounce  │
│   through an aligned host buffer. Knows virtio; does not know io_uring.    │
└───────────────────────────────┬───────────────────────────────────────────┘
                                │  AsyncIoOperation
                                ▼
┌───────────────────────────────────────────────────────────────────────────┐
│ block CRATE - AsyncIo abstraction           block/src/io/async_io.rs       │
│   THE SEAM. A trait with two verbs: submit an operation, retrieve the next │
│   completion. Plus one eventfd that says "something completed".            │
│   Everything above is backend-agnostic. Everything below is a backend.     │
└──────────────┬──────────────────────────┬─────────────────────────────────┘
               │                          │
               ▼                          ▼
┌──────────────────────────┐  ┌──────────────────────────┐  ┌──────────────┐
│ RawAio                   │  │ RawAsync                 │  │ RawSync      │
│ engine_aio.rs            │  │ engine_uring.rs          │  │ engine_sync  │
│   AioDataIo              │  │   UringDataIo            │  │  blocking    │
│   io_setup/io_submit/    │  │   io_uring_setup/        │  │  pread/pwrite│
│   io_getevents           │  │   io_uring_enter + rings │  │  + synthetic │
└──────────┬───────────────┘  └──────────┬───────────────┘  └──────┬───────┘
           └──────────────────┬──────────┴─────────────────────────┘
                              ▼
┌───────────────────────────────────────────────────────────────────────────┐
│ LINUX KERNEL                                                              │
│   Page cache (buffered) or direct-to-device (O_DIRECT). Block layer,       │
│   scheduler, driver. io-wq worker threads for io_uring operations that     │
│   would block. Posts completions and signals the registered eventfd.       │
└───────────────────────────────┬───────────────────────────────────────────┘
                                ▼
                          host storage
```

### 1.2 What each layer is allowed to know

The value of the design is in what each layer is forbidden from knowing:

- `virtio-devices/src/block.rs` knows virtqueues and `dyn AsyncIo`. It does not know that io_uring
  exists. Search it: there is no `io_uring` identifier anywhere in the file.
- `block/src/io/request.rs` knows the virtio-blk request format and iovecs. It asks
  `disk_image.batch_requests_enabled()` (line 257) and `disk_image.alignment()` (line 244) rather
  than testing which backend it has.
- `block/src/formats/raw/mod.rs` knows the three backends exist and picks one at construction.
- `block/src/io/async_io/uring_data_io.rs` knows io_uring and nothing about virtio.

The one place that decides which backend to use is `block/src/factory.rs:144` (`open_raw`), and it
decides by *probing the running kernel*, not by configuration alone.

### 1.3 Connecting to rungs 1 to 3

| You built in rung | It reappears here as |
|---|---|
| `KVM_CREATE_VM`, `KVM_SET_USER_MEMORY_REGION`, GPA/HVA | `GuestMemoryMmap`. The `iovec.iov_base` values handed to the kernel in Part 2 are HVAs derived from guest GPAs (`guest_memory_target.rs:93-99`) |
| `KVM_RUN` and `KVM_EXIT_MMIO` | The guest's notify write. In production it is an ioeventfd rather than a userspace exit, which is why the cost model in rung 1 matters |
| Descriptor table, available ring, used ring | `queue.iter(...).next()` at `block.rs:271`, `queue.add_used(...)` at `block.rs:617`. Same three rings you laid out by hand |
| `Device::needs_notification` | `queue.needs_notification(...)` at `block.rs:445`, guarding the interrupt |
| `virtio-queue`'s `DescriptorChain` | `Request::parse` consuming `desc_chain.next_checked(...)` at `request.rs:89` |
| The `mock.rs` layout bug you fixed in PR #400 | The same crate, one layer down from this file |

The genuinely new territory below rung 3 is everything from `AsyncIo` downward: what a VMM does
*after* it has understood the guest's request. Rung 2 gave you the queue. This rung gives you the
device.

---

## Part 2 - Trace of a 4 KiB buffered virtio-blk write

Scenario, fixed so every number is concrete:

- Raw disk image, buffered (`direct = false`), io_uring backend available.
- Guest writes 4096 bytes at LBA 8 (byte offset 4096).
- Descriptor chain: header descriptor (16 B, read-only), one data descriptor (4096 B, read-only for
  a write), status descriptor (1 B, write-only).
- Queue size 128, one queue.

### Step 0 - Setup, once at device activation

| Where | What |
|---|---|
| `block/src/factory.rs:144` `open_raw` | Probes io_uring at `factory.rs:154` via `io_uring_supported()` -> `block_io_uring_is_supported()` (`block/src/lib.rs:255`), which creates a one-entry ring and `register_probe`s for `Fsync`, `Readv`, `Writev`. If all present, returns `RawDisk::new(file, RawBackend::IoUring, direct)`. Else falls to AIO (`factory.rs:165`), else `Sync` (`factory.rs:177`). The result is memoised in a `OnceLock` (`factory.rs:63`) so the probe runs once per process |
| `block/src/formats/raw/mod.rs:152` `create_async_io(ring_depth)` | Wraps the fd in `AlignedFile::new(file, direct)`. With `direct = false`, `alignment = 0` (`aligned_file.rs:51-57`). Then builds `RawAsync::new(raw_file, ring_depth)` (`mod.rs:161`) |
| `block/src/formats/raw/engine_uring.rs:27` `RawAsync::new` | `UringDataIo::new(ring_depth)` -> `IoUring::new(ring_depth)` then `register_eventfd(completions.notifier())` (`uring_data_io.rs:36-40`) |
| `virtio-devices/src/block.rs:686-688` | Worker thread builds an `EpollHelper` and registers **two** fds: the queue's kick eventfd as `QUEUE_AVAIL_EVENT`, and `self.disk_image.notifier()` (the `CompletionCommon` eventfd) as `COMPLETION_EVENT` |

One structural point worth pausing on: **the same worker thread waits for both guest kicks and I/O
completions.** There is no separate completion thread. That is why draining efficiently matters, and
it is the reason a per-completion cost is a per-I/O cost.

### Step 1 to 3 - Guest submits, CH discovers, descriptors interpreted

```
guest virtio_blk driver
  → writes descriptors, publishes head index in avail ring, writes notify register
  → KVM traps, signals the ioeventfd, resumes the vCPU (no userspace exit)
  → worker thread's epoll_wait returns

virtio-devices/src/block.rs:707   handle_event, QUEUE_AVAIL_EVENT arm
  → self.queue_evt.read()                          drain the eventfd counter
  → process_queue_submit_and_signal()              block.rs:458
    → process_queue_submit()                       block.rs:244
```

Inside `process_queue_submit` (`block.rs:261-410`), per iteration:

| Line | Call | Purpose |
|---|---|---|
| 264 | `if processed >= queue_size break` | Bound the drain at the virtqueue size. A malicious driver can keep appending while the VMM reads; this caps the work per wake-up |
| 271-278 | `queue.iter(mem)?.next()` | Pull one descriptor chain. This is `virtio-queue` from rung 2 |
| 281 | `is_head_in_flight(...)` | Reject a guest that reuses a head index still in flight. Returns `QueueDuplicatedHeadIndex`, which is treated as fatal at `block.rs:461` and marks the device `NEEDS_RESET` |
| 286 | `Request::parse(&mut desc_chain, access_platform)` | `block/src/io/request.rs:84` |
| 305 | `check_request(...)` | Read-only device, sector-0 protection |
| 325-354 | rate limiter | Ops and bytes token buckets; `go_to_previous_position()` pushes the chain back if throttled |
| 358 | `request.execute_async(mem, nsectors, disk_image, serial, ..., head_index as u64)` | The head index becomes `user_data`. Remember this: **`user_data` is the virtqueue head index** |

`Request::parse` (`request.rs:84-173`) walks the chain:

1. Head descriptor must be readable (`:97`). Read `request_type` and `sector` from it (`:104-105`).
2. Walk the middle descriptors into `data_descriptors: SmallVec<[(GuestAddress, u32); 32]>`
   (`:140`), enforcing direction per request type (`:124-138`): for `Out` (guest write to disk) a
   write-only data descriptor is illegal; for `In` a read-only one is.
3. The final descriptor is the status byte; it must be write-only and non-empty (`:162-168`).
4. `start: Instant::now()` (`:109`) is stamped here, which is where the per-request latency counter
   at `block.rs:516` measures from.

### Step 4 to 6 - Request becomes an I/O operation

```
block/src/io/request.rs:232  execute_async
  → offset = sector << SECTOR_SHIFT                       :243   (LBA 8 → 4096)
  → alignment = disk_image.alignment()                    :244   (0, buffered)
  → check_data_bounds(disk_nsectors)                      :246
  → match RequestType::Out                                :279
    → build_data_operation(mem, offset, alignment, ud)    :280 → :466
    → if disk_image.batch_requests_enabled()              :281
        ret.batch_request = Some(op)                      :282   ← io_uring takes THIS branch
      else
        disk_image.write_from_memory(offset, target, ud)  :290   ← AIO takes THIS branch
```

`build_data_operation` (`request.rs:466-496`) makes the one decision that matters for memory layout:

- `guest_memory_is_aligned(&mem, alignment)` (`:473` -> `:499`). With `alignment <= 1` it returns
  `true` immediately (`:504`). Buffered I/O therefore **always** takes the zero-copy path.
- Zero-copy path: `GuestMemoryTarget::new(mem, &self.data_descriptors)` (`:474`). This validates
  each range with `mem.get_slice` and converts it into a `libc::iovec` whose `iov_base` is the
  **host virtual address** of the guest page (`guest_memory_target.rs:80-99`). It retains an `Arc`
  to the guest memory so the mapping cannot go away while the kernel holds the pointer
  (`guest_memory_target.rs:52-61`).
- Bounce path (only under O_DIRECT with misaligned guest buffers): allocate an `OwnedIoBuffer`
  (`:484`) and, for a write, copy guest bytes into it (`:488`).

For our scenario: one iovec, `iov_len = 4096`, `iov_base` = HVA of the guest's page. The result is
`AsyncIoOperation::WriteFromMemory { offset: 4096, target, user_data: head_index }`.

Because io_uring reports `batch_requests_enabled() == true` (`engine_uring.rs:90`), the operation is
**not submitted here**. It is returned to `block.rs:372`, pushed onto `batch_requests`, and the
whole batch is submitted once after the loop:

```
virtio-devices/src/block.rs:412-413
  if !batch_requests.is_empty()
    → self.disk_image.submit_batch_requests(batch_requests)
```

AIO reports `false` (the default at `async_io.rs:167`), so AIO submits one operation at a time,
inline at `request.rs:290`. **This asymmetry is load-bearing and returns in Parts 6 and 8.**

### Step 7 to 8 - Backend submission

**io_uring** (`engine_uring.rs:94` -> `uring_data_io.rs:98`):

```
submit_batch(fd, batch)
  :103  validate_batch(...)                 reject duplicate/in-flight user_data
  :105  let (submitter, mut sq, _) = io_uring.split()
  :106  available = sq.capacity() - sq.len()
  :107  if batch.len() > available          → complete every op locally with -EAGAIN, return
  :120  for op in batch:
          entry = build_entry(fd, &op)      :179  Writev{fd, iovecs.as_ptr(), len}
                                                   .offset(op.offset()).user_data(op.user_data())
          in_flight.insert(user_data, Some(op))    ← ownership parked BEFORE the tail moves
          unsafe { sq.push(&entry) }
  :141  sq.sync()                           publish the SQ tail to the kernel
  :142  submitter.submit()                  ONE io_uring_enter for the whole batch
```

Note the ordering comment at `:125-127`: the operation is inserted into `in_flight` *before* the SQ
tail is advanced, so the iovec memory is owned for as long as the kernel can see the SQE.

**AIO** (`engine_aio.rs:54` -> `aio_data_io.rs:52`):

```
submit_operation(fd, op)
  :53   validate_batch(...)  (single-element slice)
  :65   build iocb {
          aio_lio_opcode = IOCB_CMD_PWRITEV
          aio_buf        = iovecs.as_ptr()      ← pointer to the iovec ARRAY
          aio_nbytes     = iovecs.len()         ← COUNT of iovecs, not bytes
          aio_offset     = 4096
          aio_data       = user_data
          aio_flags      = IOCB_FLAG_RESFD
          aio_resfd      = completions.notifier()
        }
  :76   in_flight.insert(user_data, Some(op))
  :78   ctx.submit(&[&mut iocb])              ONE io_submit syscall for ONE operation
```

The `aio_buf`/`aio_nbytes` overloading is a libaio convention for the vectored opcodes: for
`PREADV`/`PWRITEV` they carry the iovec array pointer and the iovec count.

### Step 9 - The kernel performs it

**[KERNEL]** For a buffered write, the kernel copies from the iovecs into page cache pages, marks
them dirty, and returns. Nothing reaches storage synchronously. The `writeback` flag on the request
(`block.rs:356`, consumed at `block.rs:555`) decides whether CH issues an `fsync` afterwards.

**[KERNEL]** The two interfaces differ in *when they return relative to that copy*:

- libaio with buffered I/O has no asynchronous path in the general case. `io_submit` performs the
  work in the submitting thread's context and the completion is queued essentially immediately. This
  is the long-standing libaio limitation that motivated io_uring.
- io_uring first attempts the operation non-blocking in the submitting context. If it would block,
  it is punted to an **io-wq** kernel worker thread and completes later.

Both of these are documented kernel behaviour, not statements this repository's source makes. They
are the hinge of Part 8, and both are marked **[VERIFY]** for the experimental stage.

### Step 10 - Completion generated

**AIO:** the kernel writes an `IoEvent { data = user_data, obj, res, res2 }` into the completion
ring of the `io_context_t`, and because `IOCB_FLAG_RESFD` was set with `aio_resfd`, it signals the
eventfd.

**io_uring:** the kernel writes a CQE `{ user_data, res, flags }` into the shared CQ ring, advances
the ring's `tail`, and signals the registered eventfd.

Either way, the worker thread's `epoll_wait` returns with `COMPLETION_EVENT`.

### Step 11 to 12 - Retrieval and guest completion

```
virtio-devices/src/block.rs:719  handle_event, COMPLETION_EVENT arm
  :720  self.disk_image.notifier().read()    drain the eventfd counter
  :724  process_queue_complete()             block.rs:498
  :728  try_signal_used_queue()              block.rs:442
  :734  process_queue_submit_and_signal()    opportunistically pick up new work
```

`process_queue_complete` (`block.rs:498-640`) is the hot loop:

```rust
while let Some(mut completion) = self.disk_image.next_completed_request() {   // :505
    let result     = completion.result;                                      // :506
    let desc_index = completion.user_data as u16;                            // :507
    let mut request = self.find_inflight_request(desc_index)?;               // :509
    request.complete_async(&mem, &mut completion)?;                          // :512
    let latency = request.start().elapsed().as_micros() as u64;              // :516
    // ... counters ...
    mem.write_obj(status, request.status_addr())?;                           // :612
    queue.add_used(mem.deref(), desc_index, len)?;                           // :617
    queue.enable_notification(mem.deref())?;                                 // :620
}
```

- `find_inflight_request` (`:478`) recovers the `Request` that `process_queue_submit` parked. It
  scans a `VecDeque` and `swap_remove_front`s the match. The comment at `:479-488` records that
  completions are in order about 99% of the time during boot, so this is normally a `pop_front`.
- `complete_async` (`request.rs:589`) is a no-op for writes. For reads that used a bounce buffer it
  copies the host buffer back into guest memory.
- `add_used` publishes the head index and length into the used ring. **This is rung 2's used ring,
  in production.**
- `enable_notification` and then `needs_notification` at `block.rs:445` decide whether to raise the
  interrupt at `signal_used_queue` (`block.rs:642`). One interrupt can cover many completions, which
  is exactly the `EVENT_IDX` suppression you measured in rung 2.

### 2.1 The whole path on one line

```
guest kick → ioeventfd → epoll → queue.iter().next() → Request::parse →
execute_async → build_data_operation → AsyncIoOperation →
[batch or single] → SQE/iocb → kernel → CQE/IoEvent → eventfd → epoll →
next_completed_request() → find_inflight_request → status byte → add_used →
needs_notification → interrupt
```

---

## Part 3 - The `AsyncIo` abstraction

### 3.1 The trait

`block/src/io/async_io.rs:106`:

```rust
pub trait AsyncIo: Send {
    fn notifier(&self) -> &EventFd;                                    // :107
    fn submit_data_operation(&mut self, op: AsyncIoOperation) -> AsyncIoResult<()>;  // :114
    fn read_to_memory(...)   { self.submit_data_operation(...) }       // :117  default
    fn write_from_memory(...) { self.submit_data_operation(...) }      // :127  default
    fn read_to_vec(...)      { ... }                                   // :139  default
    fn write_from_vec(...)   { ... }                                   // :149  default
    fn fsync(&mut self, user_data: Option<u64>) -> ...;                // :158
    fn punch_hole(...);  fn write_zeroes(...);                         // :159-160
    fn next_completed_request(&mut self) -> Option<AsyncIoCompletion>;  // :165
    fn batch_requests_enabled(&self) -> bool { false }                 // :167  default
    fn submit_batch_requests(&mut self, ...) -> ... { Err(...) }       // :175  default
    fn alignment(&self) -> u64 { SECTOR_SIZE }                         // :185  default
}
```

Four required methods carry the whole design: a wake-up fd, a submit verb, a retrieve verb, and the
metadata verbs. The four data-shaped methods are defaults that funnel into
`submit_data_operation`, so a backend implements one submission function, not four.

The two capability-query methods (`batch_requests_enabled`, `alignment`) are how the layer above
adapts without naming a backend. `request.rs:257` reads the first; `request.rs:244` reads the second.

### 3.2 `AsyncIoOperation` - the unit of work

`block/src/io/async_io/operation.rs:16`. A four-variant enum:

| Variant | Data source/sink | When |
|---|---|---|
| `ReadToMemory { offset, target: GuestMemoryTarget, user_data }` | guest memory, zero-copy | aligned reads |
| `WriteFromMemory { offset, target, user_data }` | guest memory, zero-copy | aligned writes |
| `ReadToVec { offset, buffer: OwnedIoBuffer, user_data }` | host bounce buffer | O_DIRECT misaligned reads |
| `WriteFromVec { offset, buffer, user_data }` | host bounce buffer | O_DIRECT misaligned writes |

Accessors: `user_data()` `:97`, `offset()` `:107`, `is_read()` `:127`, `iovecs()` `:168`. The last is
the one the backends use: it returns `&[libc::iovec]` regardless of variant, so
`aio_data_io.rs:59` and `uring_data_io.rs:180` are identical in shape.

**The operation owns its memory.** For the guest-memory variants that ownership is an `Arc` to the
`GuestMemoryMmap` (`guest_memory_target.rs:58`); for the vec variants it is the buffer itself. The
doc comment at `async_io.rs:109-113` states the contract: *"Implementations that complete
asynchronously must retain it until its completion is returned."* This is the whole reason Part 9
exists.

### 3.3 `AsyncIoCompletion` - the unit of result

`block/src/io/async_io/completion.rs:16`:

```rust
pub struct AsyncIoCompletion {
    pub user_data: u64,               // the virtqueue head index, in production
    pub result: i32,                  // >= 0 byte count, < 0 negative errno
    pub buffer: Option<OwnedIoBuffer>, // returned so a read can be copied back / freed
}
```

The `buffer` field is how ownership travels *back*. `from_operation` (`:42`) consumes the operation
and extracts its buffer; `request.rs:596` takes it out with `completion.buffer.take()` and copies it
into guest memory.

### 3.4 `CompletionCommon` - the shared plumbing

`block/src/io/async_io/completion.rs:51`:

```rust
pub(crate) struct CompletionCommon {
    queue: VecDeque<AsyncIoCompletion>,
    eventfd: EventFd,                        // EFD_NONBLOCK, :60
}
    fn complete(&mut self, c)  { self.queue.push_back(c); self.eventfd.write(1).unwrap(); }  // :70
    fn next_completed(&mut self) -> Option<...> { self.queue.pop_front() }                    // :75
```

Both real backends own one (`aio_data_io.rs:30`, `uring_data_io.rs:28`). It serves three jobs:

1. **The wake-up fd.** Registered in epoll by the device (`block.rs:688`). For AIO the *kernel*
   signals it via `aio_resfd`; for io_uring the *kernel* signals it via `register_eventfd`. In both
   cases `complete()` can also signal it from userspace.
2. **A staging buffer.** AIO reaps 32 kernel events at a time into this `VecDeque` and hands them out
   one at a time (`aio_data_io.rs:144-155`).
3. **A channel for synthetic completions.** Operations that never reach the kernel still have to
   surface through the same path: an unaligned O_DIRECT operation run inline
   (`engine_uring.rs:65-67`), a `punch_hole` done with `fallocate` (`engine_aio.rs:99`,
   `engine_uring.rs:131`), a batch that did not fit in the SQ (`uring_data_io.rs:111-115`). Each
   calls `inject_completion`, and the caller cannot tell the difference. **This uniformity is the
   abstraction's real payoff.**

`EFD_NONBLOCK` matters: `wait_for_eventfd` (`util.rs:388-398`) relies on `read()` returning
`WouldBlock` rather than blocking, and spins with 50 µs sleeps.

### 3.5 Conceptual diagram

```
            ┌──────────────────────────────────────────────────────┐
            │            virtio-devices/src/block.rs               │
            │      submit side              complete side          │
            └───────┬──────────────────────────────▲───────────────┘
                    │ AsyncIoOperation             │ AsyncIoCompletion
                    ▼                              │
            ┌───────────────────────────────────────────────────────┐
            │      trait AsyncIo   (block/src/io/async_io.rs)       │
            │  submit_data_operation      next_completed_request    │
            │  submit_batch_requests      notifier() -> &EventFd    │
            └───┬───────────────────────────────────▲───────────────┘
   ┌────────────┴──────────┐            ┌───────────┴──────────────┐
   ▼                       ▼            │                          │
┌─────────────┐   ┌──────────────────┐  │   ┌───────────────────┐  │
│ AioDataIo   │   │ UringDataIo      │  │   │ CompletionCommon  │  │
│             │   │                  │  │   │  VecDeque<...>    │──┘
│ io_submit   │   │ SQ ring + SQEs   │  │   │  EventFd          │
│     ↓       │   │      ↓           │  │   └─────────▲─────────┘
│  KERNEL     │   │   KERNEL         │  │             │ inject_completion
│     ↓       │   │      ↓           │  │             │ (synthetic)
│ io_getevents│   │ CQ ring + CQEs   │  │             │
│  (SYSCALL)  │   │ (NO SYSCALL)     │──┴─────────────┘
└─────────────┘   └──────────────────┘
                              ▲
             in_flight: HashMap<u64, Option<AsyncIoOperation>>
             (both backends; keeps iovec memory alive - Part 9)
```

### 3.6 Why the abstraction exists

Not for elegance. Five concrete reasons, each visible in the tree:

1. **The backend is a runtime property of the host.** `factory.rs:152-182` picks io_uring, then AIO,
   then sync, by probing. A kernel without io_uring, a seccomp filter blocking `io_uring_setup`, or
   `--disable-io-uring` all change the answer on the same binary. The device model cannot be
   compiled against one backend.
2. **`io_uring` is an optional cargo feature.** `block/Cargo.toml` `io_uring = ["dep:io-uring"]`, and
   `RawBackend::IoUring` is `#[cfg(feature = "io_uring")]` (`mod.rs:41-42`). Without the seam, every
   caller would need `cfg` arms.
3. **There are more formats than backends.** raw, qcow2, vhd, vhdx, vmdk each implement `AsyncIo`,
   and qcow2 has both a sync and an io_uring engine. The device model must not care.
4. **Synthetic completions need a uniform path** (§3.4).
5. **Testability.** `virtio-devices/src/block.rs:1399` defines a fake `AsyncIo` returning `None`
   from `next_completed_request`, so the virtqueue logic is testable without any kernel I/O at all.

---

## Part 4 - AIO from first principles

### 4.1 The minimum Linux AIO you need

Linux AIO ("libaio", not POSIX AIO) is a four-syscall interface. The Rust binding is
`vmm_sys_util::aio`, vendored at `vmm-sys-util-0.15.0/src/linux/aio.rs`:

| Concept | Syscall | Binding |
|---|---|---|
| Create a context with room for N in-flight ops | `io_setup(nr_events, &ctx)` | `IoContext::new(nr_events)` `:86` |
| Submit an array of control blocks | `io_submit(ctx, n, iocbs)` | `IoContext::submit(&[&mut iocb])` `:128` |
| Reap completions | `io_getevents(ctx, min_nr, nr, events, timeout)` | `IoContext::get_events(min_nr, events, timeout)` `:218` |
| Destroy | `io_destroy(ctx)` | `Drop` `:247` |

**The `iocb`** (`IoControlBlock`) is the request descriptor. The fields CH sets
(`aio_data_io.rs:65-75`):

```
aio_fildes     the fd
aio_lio_opcode IOCB_CMD_PREADV or IOCB_CMD_PWRITEV
aio_buf        pointer to the iovec array      (vectored-opcode convention)
aio_nbytes     number of iovecs                (vectored-opcode convention)
aio_offset     file offset in bytes
aio_data       opaque u64 returned verbatim in the completion  ← the correlation token
aio_flags      IOCB_FLAG_RESFD
aio_resfd      an eventfd the kernel signals on completion
```

**The `IoEvent`** (`vmm-sys-util .../aio.rs:66`) is 4 x `u64` = **32 bytes**:

```
data   echo of aio_data
obj    pointer to the original iocb
res    result (bytes transferred, or negative errno)
res2   secondary result, unused here
```

**`aio_resfd` is what makes AIO composable with epoll.** Without it you would have to block in
`io_getevents`. With it, the kernel bumps an eventfd, the device's epoll loop wakes, and reaping is a
non-blocking `io_getevents(min_nr = 0, ...)`.

**The interface's central limitation.** **[KERNEL]** libaio is genuinely asynchronous only for
`O_DIRECT` on a file system that supports it. For buffered I/O, `io_submit` does the work inline and
can block. This is the well-known limitation that motivated io_uring. It is not stated anywhere in
this repository, and it is the hinge of Part 8, so treat it as **[VERIFY]**.

### 4.2 Mapping onto `aio_data_io.rs`

```rust
pub struct AioDataIo {                          // :21
    ctx: aio::IoContext,                        // :24  declared FIRST, on purpose
    in_flight: HashMap<u64, Option<AsyncIoOperation>>,  // :29
    completions: CompletionCommon,              // :30
}
```

The comment at `:22-23` is worth reading twice: *"Keep this before `in_flight`: Rust drops fields in
declaration order, so dropping the context destroys kernel AIO state before retained operations
release the buffers referenced by their iovecs."* Field order is a memory-safety decision, and there
is no test that would catch reordering it.

**Submission** (`:52-91`):

```
validate_batch(...)                          reject duplicate user_data (common.rs:26)
build iocb with aio_resfd = notifier
in_flight.insert(user_data, Some(op))        ← ownership parked BEFORE the syscall
match ctx.submit(&[&mut iocb]):
   Ok(1)  => return Ok(())                   accepted; completion will come from the kernel
   Ok(_)  => result = -EAGAIN                accepted zero; synthesise a failure
   Err(e) => result = errno_result(&e)
remove from in_flight, inject_completion(...)  submission failures still surface as completions
```

The `Ok(_) => -EAGAIN` arm is subtle: `io_submit` returns the number of iocbs accepted, and a short
accept is not an error. Treating it as `-EAGAIN` lets the caller see every request exactly once.

**Reaping** (`:131-156`) - the function the benchmark measures:

```rust
pub fn next_completion(&mut self) -> Option<AsyncIoCompletion> {
    if let Some(c) = self.completions.next_completed() { return Some(c); }   // :132  local first
    let mut events = [aio::IoEvent::default(); 32];                          // :136  1 KiB stack
    let rc = match self.ctx.get_events(0, &mut events, None) { ... };        // :137  SYSCALL, min_nr=0
    for event in &events[..rc] {                                             // :144
        self.completions.complete(AsyncIoCompletion::new(
            event.data, event.res as i32,
            self.in_flight.remove(&event.data).flatten()
                .and_then(AsyncIoOperation::into_completion_buffer),         // :148-151
        ));
    }
    self.completions.next_completed()                                        // :155
}
```

Three properties fall out:

- **Local-first.** If the `VecDeque` is non-empty, no syscall happens at all.
- **`min_nr = 0`** makes `io_getevents` non-blocking. It returns whatever is ready, possibly zero.
  So `next_completion()` returning `None` means "nothing ready right now", not "all done".
- **Batch size 32.** PR #7864's body: *"The batch size of 32 matches QEMU `DEFAULT_MAX_BATCH`. The
  stack cost is 1 KB per call (32 x 32 byte `IoEvent`)."*

Note `self.completions.complete(...)` at `:145` also does `eventfd.write(1)` per event
(`completion.rs:72`). **[INFER]** So reaping 32 kernel events performs 32 eventfd writes that nobody
needed, because the kernel already signalled the eventfd. That is a real per-completion cost on the
AIO drain path that is invisible from the syscall count. Whether it is measurable is **[VERIFY]**,
but it is a genuine observation about the code and is exactly the kind of thing a drain benchmark is
for.

### 4.3 Deriving N = 128 and N = 256

Do not memorise the answer. Derive it from two facts: submission is one `io_submit` per operation
(`:78`, one-element slice), and reaping is local-first with a 32-event batch (`:132-137`).

**Submission, N = 128.** The AIO path never batches (`batch_requests_enabled()` is the default
`false`), so `request.rs:290` calls `write_from_memory` once per request, each reaching
`ctx.submit(&[&mut iocb])`. Count: **128 `io_submit` calls**. For N = 256: **256**.

**Reaping, N = 128.** Trace the `VecDeque` state across calls, assuming all 128 events are ready:

```
call   1: deque empty → io_getevents → returns 32 → deque=32 → pop → deque=31   [1 syscall]
call   2: deque=31 → pop → deque=30                                            [0 syscalls]
   ...
call  32: deque=1  → pop → deque=0                                             [0 syscalls]
call  33: deque empty → io_getevents → returns 32 → ...                        [1 syscall]
```

The pattern repeats. 128 / 32 = 4 refills, so **4 `io_getevents` calls** for 128 completions. This is
exactly PR #7864's claim: *"At the default queue depth of 128, this reduces syscalls from 128 to 4
per drain cycle."* For N = 256: 256 / 32 = **8 calls**.

**The caveat that makes this an upper bound, not a certainty.** `io_getevents` with `min_nr = 0`
returns *what is ready*, not 32. If only 5 events have completed, `rc = 5`. So 4 is the count in the
best case where all completions are already queued when draining starts. In the worst case, where
completions trickle in one at a time, you get one syscall per completion and the batching buys
nothing. **Whether the benchmark realises the best case is precisely the question in Part 8.**

**One more `+1`.** After the 128th completion, `process_queue_complete`'s `while let` loop calls
`next_completed_request()` once more, gets `None`, and exits. That call costs a fifth
`io_getevents`. The benchmark's loop (`micro_bench_block.rs:52`) counts to `num_ops` instead of
looping until `None`, so it does *not* pay that call. A small but real difference between the
benchmark and production.

---

## Part 5 - io_uring from first principles

### 5.1 The minimum io_uring you need

io_uring replaces "syscall per operation" with **two ring buffers in memory shared between kernel and
userspace**.

```
        USERSPACE                                 KERNEL
   ┌──────────────────────┐                ┌──────────────────────┐
   │  SQ ring             │                │                      │
   │   khead  (kernel rd) │◄───── reads ───┤  consumes SQEs       │
   │   ktail  (user  wr)  ├────── writes ──►                      │
   │   [ SQE ][ SQE ] ... │                │                      │
   └──────────────────────┘                │                      │
            ▲                              │   performs I/O       │
            │ push + sync                  │   (inline or io-wq)  │
   ┌────────┴─────────────┐                │                      │
   │  application         │                │                      │
   └────────┬─────────────┘                │                      │
            │ read + advance head          │                      │
   ┌────────▼─────────────┐                │                      │
   │  CQ ring             │                │                      │
   │   khead  (user  wr)  ├────── writes ──►  reclaims CQE slots   │
   │   ktail  (kernel wr) │◄───── reads ───┤  posts CQEs           │
   │   [ CQE ][ CQE ] ... │                │                      │
   └──────────────────────┘                └──────────────────────┘

   io_uring_setup(entries, params) → ring fd + params filled in by the KERNEL
   mmap(ring fd, ...)              → both rings mapped into the process
   io_uring_enter(fd, to_submit, min_complete, flags)
                                   → tells the kernel to consume SQEs
                                     and optionally wait for completions
```

Terms:

- **SQE** - submission queue entry. Opcode (`Readv`, `Writev`, `Fsync`, `Nop`), fd, buffer pointer,
  offset, and a 64-bit `user_data` echoed back verbatim.
- **CQE** - completion queue entry. `user_data`, `res` (bytes or negative errno), `flags`.
- **The critical asymmetry.** Submission needs a syscall (`io_uring_enter`) unless SQPOLL is
  configured, and one call can carry many SQEs. **Retrieving completions needs no syscall at all**:
  the CQEs are already in memory the process has mapped. You read them and advance the head.
- **io-wq** - **[KERNEL]** the kernel-side worker thread pool io_uring punts operations to when they
  cannot be completed without blocking in the submitting context.

### 5.2 Sizing: what `IoUring::new(entries)` actually does

`io-uring-0.7.12/src/lib.rs:125` -> `builder().build(entries)` -> `:160 with_params` ->
`sys::io_uring_setup(entries, &mut p)`.

**The capacities are decided by the kernel, not the crate.** `io_uring_setup` fills `p.sq_entries`
and `p.cq_entries`, and the mmap sizes are computed from those returned values (`lib.rs:178-180`).
`SubmissionQueue::capacity()` and `CompletionQueue::capacity()` read `ring_entries` back out of the
mapped ring, so at runtime the truth is observable.

**[KERNEL]** The documented default (`io_uring_setup(2)`): `sq_entries` is `entries` rounded up to a
power of two, and `cq_entries` defaults to **twice** `sq_entries` unless `IORING_SETUP_CQSIZE` is
used. CH never sets `IORING_SETUP_CQSIZE` (`setup_cqsize` at `io-uring/src/lib.rs:372` has no caller
in this tree).

**Flag for accuracy:** the crate's own doc comment on `build` (`lib.rs:475`) says "the specified
number of entries in the submission queue and completion queue", which reads as CQ = SQ. That
contradicts the kernel man page. **The code proves neither** - it proves only that the values come
back from the kernel. Reading `params.cq_entries` at runtime settles it and is a one-line experiment.
**[VERIFY]**

So for `create_async_io(128)`: SQ = 128, CQ = 256 expected. For 256: SQ = 256, CQ = 512 expected.
Both leave headroom for N in-flight operations, which is why the current sizes are safe.

### 5.3 Mapping onto `uring_data_io.rs`

```rust
pub struct UringDataIo {                          // :21
    io_uring: IoUring,
    in_flight: HashMap<u64, Option<AsyncIoOperation>>,   // :26
    completions: CompletionCommon,                       // :27
    needs_submit_retry: bool,                            // :30
}
```

**Construction** (`:35-48`): `IoUring::new(ring_depth)?`, then
`submitter().register_eventfd(completions.notifier().as_raw_fd())?`. That single call is what makes
io_uring composable with the device's epoll loop, mirroring AIO's `aio_resfd`.

**Submission** (`:98-155`), covered in Part 2. Two design points worth naming:

- **SQ-full is not an error.** `:107-116` completes every operation in the batch locally with
  `-EAGAIN` rather than returning `Err`. The caller sees each request exactly once through the normal
  path. Before doing so it `drop(sq)` (`:110`) so the unmodified tail is republished.
- **`needs_submit_retry`** (`:143-148`). If `submit()` fails *after* SQEs were published, the kernel
  may or may not have seen them. The flag forces a retry on the next `next_completion()` (`:223`)
  and again in `Drop` (`:250`). This exists because publishing to a shared ring is not atomic with
  telling the kernel about it.

**Reaping** (`:222-243`) - the function a uring drain benchmark would measure:

```rust
pub fn next_completion(&mut self) -> Option<AsyncIoCompletion> {
    if self.needs_submit_retry { ... }                            // :223  rare
    if let Some(entry) = self.io_uring.completion().next() {      // :230  ← the hot line
        let user_data = entry.user_data();
        return Some(AsyncIoCompletion::new(
            user_data, entry.result(),
            self.in_flight.remove(&user_data).flatten()
                .and_then(AsyncIoOperation::into_completion_buffer),   // :235-238
        ));
    }
    self.completions.next_completed()                             // :242  injected only
}
```

Compare with AIO: **the order is inverted.** AIO checks its local `VecDeque` first and the kernel
second; io_uring checks the kernel ring first and the local `VecDeque` second. **[INFER]** That makes
sense - for io_uring the "kernel" check is a memory read, so there is nothing to save by deferring
it, and the `VecDeque` only ever holds synthetic completions.

### 5.4 What line `:230` costs, precisely

This is the single most important mechanical detail in this document. Unfold
`self.io_uring.completion().next()` against `io-uring-0.7.12`:

```
io_uring.completion()                     lib.rs:299   → self.cq.borrow()
  cqueue::Inner::borrow(&mut self)        cqueue.rs:88 → unsafe { self.borrow_shared() }
    borrow_shared(&self)                  cqueue.rs:78
      CompletionQueue {
          head: unsync_load(self.head),                    non-atomic read of our own head
          tail: (*self.tail).load(Ordering::Acquire),      ← ATOMIC ACQUIRE LOAD of the
          queue: self,                                       ktail the KERNEL writes
      }

  .next()                                 cqueue.rs:173
      if self.head != self.tail { read the CQE at head & mask; head += 1; Some(cqe) }

  <temporary CompletionQueue dropped at the end of the statement>
  Drop for CompletionQueue                cqueue.rs:162-167
      unsafe { &*self.queue.head }.store(self.head, Ordering::Release);
                                                        ← ATOMIC RELEASE STORE of the
                                                          khead the KERNEL reads
```

So **one completion costs, per call**:

| Cost | Which memory |
|---|---|
| 1 Acquire load of `ktail` | shared with the kernel, **written by the kernel** |
| 1 read of the CQE | shared with the kernel, written by the kernel |
| 1 Release store to `khead` | shared with the kernel, **read by the kernel** |
| 1 `HashMap::remove` | process-private |
| 0 syscalls | - |

The Release store is not bookkeeping - it is **how the process tells the kernel the CQE slot may be
reused**. The Acquire/Release pair is the memory ordering that makes the ring protocol correct: the
Acquire load of `tail` guarantees the CQE contents written by the kernel before it advanced the tail
are visible; the Release store of `head` guarantees our reads of the CQE happen before the kernel
observes the slot as free.

**[INFER] The optimization this exposes.** Because the borrow/drop pair happens *per call*, draining
N completions performs N Acquire loads and N Release stores on kernel-shared cache lines. Draining
them in one borrow would perform 1 and 1. That is the structural analogue of what PR #7864 did to
AIO: same shape, different underlying cost, and dramatically smaller expected win because there is no
syscall to eliminate. **Whether it is measurable at all is [VERIFY], and that is the honest reason to
build a benchmark before proposing a patch.**

### 5.5 Deriving N = 128 and N = 256

**Submission.** io_uring reports `batch_requests_enabled() == true` (`engine_uring.rs:90`), so
production takes the batch path: `request.rs:282` collects, `block.rs:413` submits once.
`submit_batch` pushes all N SQEs then makes **one** `submitter.submit()` call (`:142`). Count for
N = 128: **1 `io_uring_enter`**, assuming all 128 fit (SQ capacity is 128, so exactly at the limit -
see below). For N = 256 with SQ = 256: **1**.

Watch the capacity check at `:106-107`: `available = sq.capacity() - sq.len()`. If a previous batch
left SQEs unconsumed, `available < capacity` and the whole batch is rejected with `-EAGAIN`. So "1
enter" is the clean-ring case.

If instead you submit one at a time - which is what a naive mirror of the AIO benchmark would do -
each `submit_operation` (`:56`) calls `submit_batch` with a one-element vector, hence **128 or 256
`io_uring_enter` calls**. Same count as AIO's `io_submit`, and *not the shape production uses*. This
is the trap Part 8 is about.

**Reaping.** Every completion goes through `:230`. Count for N = 128: **0 syscalls**, 128 CQ
borrows, 128 Acquire loads, 128 Release stores, 128 HashMap removals. For N = 256: **0 syscalls**,
and everything else doubles.

**The comparison table this produces:**

| | AIO, N = 128 | io_uring, N = 128 (batched) |
|---|---|---|
| submission syscalls | 128 x `io_submit` | 1 x `io_uring_enter` |
| completion syscalls | 4 x `io_getevents` | **0** |
| per-completion shared-memory atomics | 0 (results copied by the syscall) | 2 (Acquire tail, Release head) |
| per-completion eventfd writes | 1 (`completion.rs:72`, **[INFER]** redundant) | 0 |
| per-completion HashMap removes | 1 | 1 |

---

## Part 6 - AIO versus io_uring

Not "old versus new". The interfaces make different trade-offs about **where the boundary between
kernel and userspace is drawn**, and that determines where the cost lands.

### 6.1 Side-by-side

| Dimension | Linux AIO | io_uring |
|---|---|---|
| Request handoff | Copy an `iocb` array into the kernel by syscall | Write an SQE into memory the kernel already maps |
| Submission syscall | `io_submit`, 1 per call, can carry many iocbs (CH uses 1) | `io_uring_enter`, 1 per call, carries all queued SQEs (CH batches) |
| Shared memory | **None.** All data crosses by syscall argument | **Both rings.** SQ, CQ, and SQE array are mmap'd |
| Completion storage | Kernel-internal ring, not visible to userspace | CQ ring, directly readable by userspace |
| Completion retrieval | `io_getevents` **syscall**, copies events out | Read the CQ ring. **No syscall** |
| Batching completions | Essential. 1 syscall per N events instead of per event | Irrelevant to syscall count; only affects atomics |
| Event notification | `aio_resfd` on the iocb (`aio_data_io.rs:72-73`) | `register_eventfd` on the ring (`uring_data_io.rs:38-40`) |
| Asynchrony for buffered I/O | **[KERNEL]** effectively none; `io_submit` does the work | **[KERNEL]** inline if possible, else punted to io-wq |
| Ordering requirements in userspace | None; the syscall is the barrier | Acquire/Release on the ring head and tail |
| Userspace bookkeeping | `in_flight` HashMap | `in_flight` HashMap, **identical** |
| Where the cost lives | **Syscall entry/exit, once per ~32 completions** | **Cache-line traffic on shared ring indices, once per completion** |

### 6.2 The two drain loops, drawn

```
AIO: drain 128 completions
────────────────────────────────────────────────────────────────────
 call 1     [ USER ]──syscall──►[ KERNEL io_getevents ]──copies 32──►[ USER deque ]
 calls 2-32 [ USER ] pop_front  (no kernel involvement)
 call 33    [ USER ]──syscall──►[ KERNEL io_getevents ]──copies 32──►[ USER deque ]
 ...
 TOTAL: 4 syscalls, 128 deque pops, 128 eventfd writes, 128 hashmap removes
 COST CONCENTRATED IN: 4 kernel transitions


io_uring: drain 128 completions
────────────────────────────────────────────────────────────────────
 call 1     [ USER ] acquire-load ktail │ read CQE │ release-store khead
 call 2     [ USER ] acquire-load ktail │ read CQE │ release-store khead
 ...
 call 128   [ USER ] acquire-load ktail │ read CQE │ release-store khead
 TOTAL: 0 syscalls, 128 hashmap removes, 256 atomic ops on kernel-shared lines
 COST SPREAD EVENLY ACROSS: 128 iterations, none of them a kernel transition
```

### 6.3 Why the AIO benchmark cannot be copied line-for-line

Four independent reasons, each sufficient on its own:

1. **The measured quantity is not the same physical thing.** `micro_block_raw_aio_drain_128_us`
   measures syscall entry/exit amortised over 32 events. An identically shaped io_uring benchmark
   measures atomic operations on shared cache lines. Putting the two numbers in one table implies a
   comparison that is not being made. The honest statement, "io_uring's completion path does not
   enter the kernel", is a property of the interface, not a measurement result.

2. **The timing boundary does not transfer.** The AIO benchmark's clock is valid only if all
   completions are already queued when it starts. Part 8 shows why that is plausible for buffered
   libaio and not obviously true for io_uring.

3. **The submission shape does not transfer.** AIO's benchmark submits one at a time, and AIO
   production submits one at a time (`batch_requests_enabled() == false`), so the benchmark is
   faithful. io_uring production **batches** (`block.rs:413`). A one-at-a-time io_uring benchmark
   models a path the VMM does not use, and changes when CQEs arrive relative to the clock.

4. **The optimization story does not transfer.** The AIO benchmark existed to justify eliminating
   syscalls (#7864, 8x). For io_uring there are no syscalls to eliminate, only atomics to coalesce.
   The expected win is far smaller and might be zero. That does not make the benchmark worthless -
   it makes it the instrument that decides whether a patch is worth proposing at all - but it does
   mean the framing "add the missing io_uring counterpart" oversells it.

**What a legitimate comparison would look like:** not AIO-drain versus uring-drain, but
**uring-drain before versus after** coalescing the CQ borrow. That is a within-backend A/B on one
mechanism, and it is exactly the shape #7864 used.

---

## Part 7 - The existing AIO benchmark

`performance-metrics/src/micro_bench_block.rs:28-58`. Reproduced with the boundary marked:

```rust
pub fn micro_bench_aio_drain(control: &PerformanceTestControl) -> f64 {
    let num_ops = control.num_ops.expect("num_ops required") as usize;      // :29
    let tmp = util::sized_tempfile(num_ops);                                // :30
    let disk = RawDisk::new(tmp.as_file().try_clone().unwrap(),
                            RawBackend::Aio, false);                        // :31  direct = false
    let mut aio = disk.create_async_io(num_ops as u32)
                      .expect("failed to create AIO context");              // :32-34

    let mem = util::guest_memory_buffer(BLOCK_SIZE as usize);               // :36  ONE 4 KiB page
    util::fill_guest_memory(&mem, BLOCK_SIZE as usize, 0xA5);               // :37

    for i in 0..num_ops {                                                   // :40  ── NOT TIMED ──
        let target = util::guest_memory_target(&mem, BLOCK_SIZE as usize);
        aio.write_from_memory((i as u64 * BLOCK_SIZE) as libc::off_t,
                              target, i as u64).expect(...);                // :42-43
    }

    util::wait_for_eventfd(aio.notifier());                                 // :47  ── NOT TIMED ──

    let start = Instant::now();                                             // :50  ══ CLOCK START ══
    let mut drained = 0usize;
    while drained < num_ops {                                               // :52
        if aio.next_completed_request().is_some() { drained += 1; }         // :53-54
    }
    start.elapsed().as_secs_f64()                                           // :57  ══ CLOCK STOP ══
}
```

### 7.1 to 7.5 - What it does, in the requested order

1. **Workload.** A `TempFile` pre-sized to `num_ops * 4096` bytes (`util.rs:27-33`). One prefaulted
   4 KiB guest-memory region (`util.rs:106-119`), filled with `0xA5`. Every write reuses **the same
   4 KiB source buffer**, targeting a different file offset. The comment at `util.rs:104-105` says
   this is deliberate: *"use this as a hot buffer to keep cache behavior close to the borrowed-iovec
   benchmarks they replaced."*
2. **Operations submitted.** Exactly `num_ops`: 128 or 256, from the `TEST_LIST` entries at
   `main.rs:1249` and `:1261`.
3. **Backend.** `RawBackend::Aio` explicitly, `direct = false`. Because `direct` is false,
   `AlignedFile::new` gives `alignment = 0` (`aligned_file.rs:51-57`), so `operation_is_aligned`
   short-circuits to `true` (`raw/mod.rs:170`) and every operation goes to the kernel. No bounce
   buffers, no synthetic completions.
4. **Before the clock.** Create the file, create the `IoContext` with `nr_events = num_ops`, allocate
   and prefault the buffer, submit all N writes (N x `io_submit`), and wait for the eventfd to fire
   **once**.
5. **After the clock.** Nothing but `next_completed_request()` in a loop until N have been counted.

### 7.6 What it claims to measure

The commit that introduced it, `00957fa9d`, is explicit: *"This measures per completion syscall
overhead and provides a baseline before any batching optimizations."* The module doc
(`micro_bench_block.rs:9-10`) says the micro benchmarks *"measure hot path operations (e.g. AIO
completion draining) at the syscall level."*

So the claim is: **the cost of converting kernel-produced AIO events into `AsyncIoCompletion`
values.**

### 7.7 What it does NOT measure

- Submission cost. N x `io_submit` happens before the clock.
- I/O latency, *provided* the assumption in Part 8 holds. If it does not, it measures exactly this.
- File creation, buffer allocation, prefaulting, `io_setup`.
- The trailing `next_completed_request() -> None` that production pays (`block.rs:505`), because the
  loop counts to N rather than looping until `None`.
- Anything above `AsyncIo`: no descriptor parsing, no `add_used`, no interrupt decision.
- Multi-iovec requests. Every operation has exactly one iovec.
- Any concurrency. Single-threaded, no contention on the shared cache lines.

### 7.8 Why batching `io_getevents` matters

Derived in §4.3: with the 32-event batch, 128 completions need 4 syscalls instead of 128. A syscall
is a privilege transition - register save/restore, and on post-Spectre kernels, page-table switching
and speculation barriers. Order hundreds of nanoseconds to low microseconds each, versus tens of
nanoseconds for a `VecDeque::pop_front`. Removing 124 of 128 transitions therefore removes most of
the work.

### 7.9 Why PR #7864 improved this benchmark so much

The PR's own table:

| Test | Before | After (batch = 32) | Speedup |
|---|---|---|---|
| `drain_128` | 61.8 µs | 8.2 µs | ~8x |
| `drain_256` | 160.1 µs | 12.6 µs | ~13x |

Work the arithmetic backwards, because it is a good consistency check:

- Before: 128 syscalls in 61.8 µs = **483 ns per completion**, consistent with one `io_getevents`
  per completion on a mitigated kernel.
- After: 4 syscalls in 8.2 µs = 2.05 µs per syscall *including* 32 pops, HashMap removes and eventfd
  writes = **64 ns per completion**.
- 483 / 64 = 7.5x, close to the reported ~8x.

The 256 case improving *more* (13x) is the interesting part. Before, per-completion cost was
160.1/256 = 625 ns, worse than the 128 case's 483 ns; after, 12.6/256 = 49 ns, better than the 128
case's 64 ns. **[INFER]** Larger N amortises the fixed per-drain cost better and, before the fix,
probably suffered more cache pressure from 256 separate syscalls. The direction is what matters:
**the benchmark was sensitive enough to resolve a change in the reap path, which is the property that
makes it a useful instrument.** Any io_uring benchmark has to clear the same bar, and with no
syscall in the picture the signal is far smaller.

---

## Part 8 - Why the io_uring benchmark is non-trivial

**This is the section that decides whether the contribution is sound.**

### 8.1 The unstated assumption

The AIO benchmark starts its clock after **one** `wait_for_eventfd`. Look at what that call actually
guarantees (`util.rs:388-398`):

```rust
pub fn wait_for_eventfd(notifier: &EventFd) {
    loop {
        match notifier.read() {
            Ok(_) => return,                                   // ← returns on the FIRST read
            Err(e) if e.kind() == ErrorKind::WouldBlock => { sleep(50us); }
            Err(e) => panic!(...),
        }
    }
}
```

An eventfd in counter mode (not `EFD_SEMAPHORE`) accumulates, and a `read` returns the whole counter
and resets it to zero. So `wait_for_eventfd` returns as soon as **at least one** completion has
signalled. It says nothing about the other N-1.

**The benchmark is therefore valid only if, by the time it returns, all N operations have already
completed.** That is an assumption about kernel behaviour, and the source code does not state it
anywhere.

### 8.2 Why the assumption is plausible for AIO

**[KERNEL]** libaio has no asynchronous path for buffered I/O. `io_submit` performs a buffered write
inline in the calling thread: copy into page cache, mark dirty, queue the completion event, signal
`aio_resfd`. It returns after the write is done, not before.

**[INFER]** If that is right, then by the end of the submission loop at `micro_bench_block.rs:44`,
**all N events are already sitting in the kernel's AIO completion ring.** `wait_for_eventfd` returns
immediately, the clock starts, and the drain loop reaps N already-present events with 4 syscalls.
The number is pure reap cost, as claimed.

This is consistent with the observed magnitudes: 8.2 µs to drain 128 completions is ~64 ns each. No
storage device completes a write in 64 ns. **The measured number is only explicable if the I/O was
already finished.** That is strong circumstantial evidence the assumption holds for AIO, but it is
still **[VERIFY]** - the direct test is to time the submission loop separately and confirm it, not
the drain loop, carries the I/O cost.

### 8.3 Why the assumption may fail for io_uring

**[KERNEL]** io_uring's model is different in exactly the relevant way:

1. The kernel first tries the operation **non-blocking** in the submitting context. If it can be
   satisfied without sleeping, a CQE is posted before `io_uring_enter` returns.
2. If it would block - page cache allocation under memory pressure, writeback throttling, a cold
   file needing a read-modify-write, a filesystem taking a lock - the request is **punted to io-wq**,
   a kernel worker thread pool. `io_uring_enter` returns immediately, having submitted but not
   completed the work. The CQE appears later, when the io-wq worker finishes.

The whole point of io_uring is that submission does not block. **The property the AIO benchmark
silently depends on is the property io_uring was designed to remove.**

### 8.4 The failure mode, concretely

Suppose `num_ops = 128`, and 120 complete inline while 8 are punted to io-wq:

```
t=0      submit all 128 SQEs, one io_uring_enter
         kernel completes 120 inline → 120 CQEs posted → eventfd signalled
         8 punted to io-wq workers
t=~1us   wait_for_eventfd() returns (the eventfd fired for the first of the 120)

t=~1us   ═══ CLOCK STARTS ═══
         drain 120 CQEs   ≈ 120 x ~40ns  =  ~5 us     ← reap cost, what we want
         drained = 120, but the loop needs 128
         SPIN: completion().next() returns None ... None ... None ...
               each iteration: acquire-load ktail, compare, drop (release-store khead)
               ~40 ns per useless spin, thousands of them
t=~85us  io-wq worker finishes op #121 → CQE posted → loop picks it up
         ... 7 more ...
t=~200us ═══ CLOCK STOPS ═══

REPORTED: micro_block_raw_uring_drain_128_us = 200
REALITY:  5 us of reaping + 195 us of waiting for 8 slow writes
```

The reported number is dominated by **storage and scheduler latency**, and it varies with page-cache
state, memory pressure, io-wq thread availability, and what else is running. That is a metric with a
misleading name, high variance, and no relationship to the code path it claims to measure.

Worse, the failure is **silent and directional**: it can only inflate the number. A reviewer seeing
`uring_drain_128 = 200 µs` next to `aio_drain_128 = 8.2 µs` would reasonably conclude io_uring's
completion path is 24x worse than AIO's, which is the opposite of the truth.

### 8.5 Why the AIO benchmark does not suffer this

**[INFER]** Because `io_submit`'s blocking behaviour is a *bug* from libaio's perspective and a
*guarantee* from the benchmark's perspective. The interface's weakness accidentally makes the timing
boundary sound. io_uring, having fixed the weakness, removes the accident.

### 8.6 What this implies for the contribution

The benchmark can still be built, but its design has to make the boundary **explicit** rather than
inherit an assumption that no longer holds. Options, in the order I would consider them:

1. **Barrier before the clock.** Establish that all N CQEs are posted before starting the timer, then
   measure only reaping. Preserves the AIO benchmark's intent honestly.
2. **Measure submit-plus-drain**, like the qcow async benchmarks already do (Part 12). Cannot be
   corrupted, but no longer isolates the reap path.
3. **Use `O_DIRECT` with pre-warmed extents**, to make completion timing more deterministic. Changes
   the workload relative to the AIO benchmark.

**Deciding between these requires measurement, not argument**, and that measurement is the go/no-go
experiment for the whole contribution. If option 1's barrier turns out to change the number
materially, the naive mirror was measuring I/O latency and the framing "add the missing io_uring
counterpart" was wrong from the start.

---

## Part 9 - `in_flight` and memory lifetime

`in_flight: HashMap<u64, Option<AsyncIoOperation>>` appears in both backends:
`aio_data_io.rs:29` and `uring_data_io.rs:26`, with the same comment: *"tracks every `user_data`
value accepted by the kernel. Owned data operations store `Some(op)` so their iovecs and backing
buffers remain valid until completion; metadata operations store `None`."*

### 9.1 What `user_data` identifies

A 64-bit token the kernel echoes back verbatim, so it must be unique among in-flight operations.

| Layer | What it is |
|---|---|
| Production | The **virtqueue head index**, passed at `block.rs:364` as `desc_chain.head_index() as u64` |
| Kernel, AIO | `iocb.aio_data`, returned as `IoEvent.data` |
| Kernel, io_uring | `sqe.user_data`, returned as `cqe.user_data` |
| Coming back | `AsyncIoCompletion.user_data`, cast back to `u16` at `block.rs:507` and used to find the parked `Request` |

Uniqueness is enforced twice: by `validate_batch` (`common.rs:26-39`, checking both the in-flight set
and duplicates within the batch), and one layer up by `is_head_in_flight` (`block.rs:239`), which
treats a reused head as a fatal protocol violation.

### 9.2 Why the operation is stored, not dropped

This is the crux, and it is a memory-safety requirement, not an optimisation.

When an SQE or iocb is handed to the kernel, what crosses the boundary is a **raw pointer**:
`iovecs.as_ptr()` (`uring_data_io.rs:183`, `aio_data_io.rs:68`). Those `iovec` structs live in a
`Vec` inside `GuestMemoryTarget` (`guest_memory_target.rs:60`), and each `iov_base` points into an
mmap'd guest-memory region kept alive by an `Arc` (`guest_memory_target.rs:58`).

If the `AsyncIoOperation` were dropped at submission:

1. The `Vec<iovec>` is freed. The kernel now holds a dangling pointer to the iovec array.
2. The `Arc` refcount drops. If it was the last reference, the guest memory is unmapped, and every
   `iov_base` dangles too.
3. The kernel later performs a DMA-like write into freed or unmapped memory. **Use-after-free, in
   the kernel, on behalf of a guest.**

`in_flight.insert(user_data, Some(op))` parks the operation so it outlives the kernel's use of the
pointers. The ordering is explicit in both backends: insert **before** publishing.
`uring_data_io.rs:125-127` says so: *"Every iovec's pointer is retained in `self.in_flight` before
the SQ tail is advanced by sync or drop."*

The `Option` distinguishes two kinds of entry:

- `Some(op)` - a data operation with memory to keep alive.
- `None` - a metadata operation (`submit_fsync` `:201`, `submit_nop` `:196`) that owns no buffers but
  still needs its `user_data` reserved so a later data op cannot collide with it. That is what
  `reserve_user_data` (`:60-67`) does.

### 9.3 What happens when a completion arrives

Both backends do the same three-step dance:

```rust
self.in_flight
    .remove(&user_data)                        // release ownership of the operation
    .flatten()                                 // Option<Option<Op>> → Option<Op>
    .and_then(AsyncIoOperation::into_completion_buffer)   // hand back a bounce buffer, if any
```

`uring_data_io.rs:235-238`, `aio_data_io.rs:148-151`.

- **`remove` is the ownership transfer.** The kernel is done with the pointers, so the operation may
  be dropped: the `Vec<iovec>` is freed and the `Arc` refcount falls.
- **`into_completion_buffer`** extracts an `OwnedIoBuffer` for the bounce-buffer variants so
  `request.rs:596-599` can copy it into guest memory. For the zero-copy variants it yields `None`
  because the data already landed in guest memory directly.

### 9.4 Why forgetting to remove would be a leak, and why abandoning is worse

If an entry is never removed, the `Arc` is never released and the guest memory region stays mapped
for the process's lifetime. A leak, not a crash - and the reason `Drop for UringDataIo`
(`uring_data_io.rs:246-291`) works so hard: *"Closing the ring fd does not cancel io_uring ops that
have started. Wait for CQEs before releasing retained iovecs."* It drains, and if it cannot, it
**deliberately leaks** with `mem::forget(mem::take(&mut self.in_flight))` (`:267`, `:287`) rather
than free memory the kernel might still write to. Leaking is the correct choice when the alternative
is a use-after-free. Open upstream issue #8069, "Cancel outstanding io_uring requests in Drop", is
about exactly this.

AIO's equivalent protection is the field-order comment (`aio_data_io.rs:22-23`): destroy the context
first, then release the operations.

### 9.5 Is the cost present on both backends?

**Yes, identically.** Both perform one `HashMap::insert` per submission and one `HashMap::remove` per
completion, on the same key type, in the same place in the flow. The `in_flight` bookkeeping is
therefore a **constant across the AIO-versus-io_uring comparison** and cannot explain a difference
between them.

**[INFER]** It can, however, dominate the io_uring drain measurement in absolute terms. Once the
syscall is gone, what remains per completion is two atomics and a hash lookup - and a SipHash-based
`HashMap::remove` with an allocation-free `u64` key is plausibly comparable to, or larger than, the
ring atomics. If a uring drain benchmark comes in at, say, 40 ns per completion, attributing that to
"io_uring's reap path" without noting that a chunk of it is `HashMap` would be sloppy. **[VERIFY]**
This is worth naming in any PR body.

---

## Part 10 - The production hot path

### 10.1 The proof line

`virtio-devices/src/block.rs:505`:

```rust
while let Some(mut completion) = self.disk_image.next_completed_request() {
```

`self.disk_image` is `Box<dyn AsyncIo>`, so this is a virtual call landing in
`engine_uring.rs:86 → uring_data_io.rs:222` or `engine_aio.rs:89 → aio_data_io.rs:131`.

### 10.2 Why the loop means once per I/O

Read the loop's contract. `next_completed_request()` returns `Option`, and `while let Some(...)`
exits on `None`. So:

- The body runs **once per completed request**: one `find_inflight_request`, one `complete_async`,
  one status write, one `add_used`, one `enable_notification`, plus counter updates.
- The condition is evaluated **once per completion plus once more** to observe `None`.
- Therefore, for a guest doing X IOPS, `next_completed_request()` is called **X + (wake-ups) times
  per second**, and the per-call cost multiplies by X.

At queue depth 128 and 100k IOPS, that is 100k calls per second per queue, on the same thread that
also parses descriptors and submits new work. A per-call cost of 50 ns is 5 ms of CPU per second per
queue, or 0.5% of a core - not catastrophic alone, but it is pure overhead, it scales with IOPS, and
it is on the thread that is also the submission bottleneck.

### 10.3 Why the loop shape amplifies it

The wake-up structure (`block.rs:719-735`) is: one eventfd signal, then drain everything available,
then try to signal the guest, then try to submit more. **One epoll wake-up can cover many
completions**, which is good for syscall amortisation and means the per-completion cost is *not*
amortised by anything. Each iteration pays in full.

Note also `block.rs:728` and `:734`: after draining, the handler calls `try_signal_used_queue` and
then `process_queue_submit_and_signal`. Time spent in the drain loop is time not spent submitting
new work, so drain cost feeds back into submission latency.

### 10.4 The criterion the maintainers actually use

This matters for the PR, not just for understanding. On PR #7847, `likebreath` asked whether micro
benchmarks belong in CI at all. `weltling`'s accepted answer set the criterion:

> "This first benchmark targets the AIO completion drain, which sits on the hot path for every block
> I/O operation. The absolute values are in microseconds, but this overhead is incurred on every
> completion and multiplies across queue depth and sustained IOPS. ... Not every micro benchmark
> carries the same weight though. A benchmark exercising a qcow2 cluster allocation routine, for
> instance, runs once per new cluster rather than once per I/O, so a regression there is far less
> impactful at runtime."

So the project's own test is **per-I/O versus per-rare-event**. `block.rs:505` puts the io_uring
drain squarely on the per-I/O side, by the same argument they already accepted for AIO. That is the
strongest available justification, and it is theirs, not ours.

---

## Part 11 - performance-metrics architecture

### 11.1 What the crate is

A standalone binary (`performance-metrics/`, 4 source files, ~3,800 lines) that runs a fixed list of
performance tests and emits a JSON report. Not a `cargo bench`, not `criterion` - a bespoke harness,
because most of its tests boot a VM.

`performance-metrics/Cargo.toml` declares:

```toml
block = { path = "../block", features = ["io_uring", "test-utils"] }
```

**So the `io_uring` feature is already enabled for this crate.** `RawBackend::IoUring` is
unconditionally available in benchmark code and no `cfg` gating would be needed to use it.

### 11.2 `PerformanceTest` and `TEST_LIST`

`main.rs:280`:

```rust
struct PerformanceTest {
    pub name: &'static str,                              // becomes the metric name verbatim
    pub func_ptr: fn(&PerformanceTestControl) -> f64,    // returns SECONDS (or bytes/s, etc.)
    pub control: PerformanceTestControl,                 // timeout, iterations, warmup, num_ops...
    unit_adjuster: fn(f64) -> f64,                       // seconds → the unit in the name
}
```

`main.rs:391`: `const TEST_LIST: [PerformanceTest; 100] = [ ... ];`

**A fixed-size array with the length written out.** Adding two entries requires changing `100` to
`102`; getting it wrong is a compile error, not a silent bug. `00957fa9d` changed `60` to `62` for
exactly this reason.

`PerformanceTest::run` (`main.rs:287-327`):

```
for _ in 0..warmup_iterations { let _ = (func_ptr)(&control); }     // :309  discarded
for _ in 0..test_iterations   { metrics.push((func_ptr)(&control)); } // :314
mean, std_dev, max, min, each passed through unit_adjuster           // :321-324
```

`adjuster::s_to_us` (`main.rs:377`) multiplies by 1e6, which is where the `_us` suffix comes from.
The reported statistics are **mean and standard deviation** - no percentiles anywhere, which is the
gap `OSS-ROADMAP.md` §2.2 C2 identified and OSS-1/OSS-2 target.

The two AIO entries (`main.rs:1242-1265`): `test_timeout: 5`, `test_iterations: 20`,
`warmup_iterations: 5`, `num_ops: Some(128)` / `Some(256)`, `unit_adjuster: s_to_us`.

On why 128 and 256, `weltling` on PR #7847:

> "I started with the value of 64, but then realized the default queue size is 128, so switched to
> that. It is unlikely someone would narrow the queue below that, so 128 represents the lower bound
> and 256 shows how drain time scales with queue depth."

### 11.3 Micro benchmarks specifically

The `micro_` name prefix is not cosmetic; it is a dispatch signal in three places:

| Location | Behaviour |
|---|---|
| `main.rs:289` | Warn if `num_ops` is set on a test whose name does not start with `micro_` |
| `main.rs:1937` | `needs_vm_tests = tests_to_run.iter().any(\|t\| !t.name.starts_with("micro_"))`. If **only** micro benchmarks are selected, `init_tests` and `cleanup_tests` are skipped entirely - no VM lifecycle, no image download |
| CI workflow | Excluded by name prefix (§11.5) |

There are currently **40** micro benchmarks: 38 qcow2 and 2 raw AIO. The raw backend has exactly one
benchmark family and it is AIO-only.

`performance-metrics` has **no unit tests for micro benchmarks**. `performance_tests.rs:576` has a
`#[cfg(test)] mod`, but `micro_bench_block.rs` and `main.rs` have none. There is no validation of
metric-name uniqueness beyond the array length check.

### 11.4 Filtering

`main.rs:1878-1893`:

```rust
tests_to_run = TEST_LIST.iter()
    .filter(|t| test_filter.is_empty() || test_filter.iter().any(|&s| t.name.contains(s)))
    .filter(|t| !test_exclude.iter().any(|&s| t.name.contains(s)))
```

Plain substring matching, both directions, comma-separated. `--list-tests` (`:1895`) prints the
selection and exits. `--test-exclude` was added by PR #7860 as the follow-up `likebreath` requested
on #7847.

### 11.5 The CI story, and the two commits

`.github/workflows/integration-metrics.yaml`:

```yaml
on:
  push:
    branches: [ main ]        # ← post-merge only, NOT per pull request
...
      - name: Run metrics tests
        run: scripts/dev_cli.sh tests --metrics -- --test-exclude micro_,block_qcow2 -- \
             --report-file /root/workloads/metrics.json
      - name: Upload metrics report
        run: curl -X PUT https://ch-metrics.azurewebsites.net/api/publishmetrics ...
```

Three facts follow directly:

1. Metrics run **after** merge to `main`, not on PRs. A benchmark cannot break anyone's PR CI.
2. `--test-exclude micro_` removes **every** micro benchmark from that run.
3. Only what survives the exclusion is uploaded to the dashboard.

**The two commits that establish this:**

- **`f4772e7f4`** (2026-03-18, Anatol Belski), *"ci: Exclude micro benchmarks from metrics CI"*.
  Body: *"Skip micro_ prefixed tests in the metrics CI workflow to avoid dashboard pollution. They
  can still be run on demand via --test-filter micro_."* Landed the day after the harness itself
  (#7847, merged 2026-03-17), i.e. the exclusion was part of the deal that got micro benchmarks
  accepted at all.
- **`b5aeabe77`** (2026-07-08, Bo Chen), *"build: Exclude the qcow2 block tests from the metrics
  runner"*, which added `block_qcow2` to the same exclusion list. Same mechanism, different motive.

**The reservation those commits answer**, from `likebreath` on #7847:

> "The main concern would be dashboard pollution if too many accumulate." / "Right, this was my main
> reservation too, particularly given the way how our metrics dashboard are being setup - it will
> display all data at once."

**Two consequences for the contribution, pulling in opposite directions:**

- *Easier:* a new `micro_` benchmark adds no dashboard row, runs in no CI job, perturbs no baseline,
  and cannot make anyone's build red. The dashboard-pollution objection is pre-answered by the
  project's own commit.
- *Harder:* `OSS-ROADMAP.md` §1's success metric is *"a measurement I contributed is running in
  someone else's CI."* A `micro_` benchmark is excluded from CI by construction, so **OSS-0 cannot
  satisfy that bar.** OSS-0 was always the calibration PR rather than the wedge, so the ladder is
  intact, but the claim has to be dropped now.

---

## Part 12 - The existing qcow io_uring benchmarks

### 12.1 Where io_uring enters the qcow path

`util.rs:70-75`:

```rust
pub fn qcow_async_tempfile(num_clusters: usize) -> (TempFile, QcowDisk) {
    let tmp = create_qcow_tempfile(num_clusters);
    let disk = QcowDisk::new(tmp.as_file().try_clone().unwrap(),
                             false, false, true, true)   // ← last arg: use_io_uring = true
        .expect("failed to open QCOW2 via QcowDisk");
    (tmp, disk)
}
```

`QcowDisk::new`'s fifth parameter is `use_io_uring: bool` (`qcow/mod.rs:92`). With it set,
`create_async_io` (`qcow/mod.rs:311-331`) returns a `QcowAsync`, which owns a `UringDataIo`
(`qcow/engine_uring.rs:36`). **So `micro_block_qcow_async_*` really does drive io_uring.** Any claim
that io_uring is unbenchmarked in this suite is false.

### 12.2 What `micro_block_qcow_async_read_128_us` measures

`micro_bench_block.rs:377-397`:

```rust
pub fn micro_bench_qcow_async_read(control: &PerformanceTestControl) -> f64 {
    let num_ops = ...;
    let (_tmp, disk) = util::qcow_async_tempfile(num_ops);          // NOT timed
    let mut async_io = disk.create_async_io(num_ops as u32)...;     // NOT timed
    let mem = util::guest_memory_buffer(QCOW_CLUSTER_SIZE as usize); // NOT timed

    let start = Instant::now();                                     // ══ CLOCK START ══
    submit_reads(async_io.as_mut(), &mem, num_ops,
                 QCOW_CLUSTER_SIZE, QCOW_CLUSTER_SIZE as usize);    // :386  submission
    drain_async_completions(async_io.as_mut(), num_ops);            // :395  wait + reap
    start.elapsed().as_secs_f64()                                   // ══ CLOCK STOP ══
}
```

The timing boundary contains **four** things:

1. Per-read qcow2 metadata resolution: `map_clusters_for_read`, L1/L2 lookup under the metadata
   lock, LRU cache hit or miss.
2. io_uring submission - one `io_uring_enter` per read, since `submit_reads` (`util.rs:149-162`)
   calls `read_to_memory` one at a time.
3. **Actual disk/page-cache read latency**, because the clock does not stop until every completion
   has arrived.
4. Completion reaping, which is the only part shared with the proposed benchmark.

And `drain_async_completions` (`util.rs:182-190`) is a different loop from the AIO benchmark's:

```rust
while drained < count {
    wait_for_eventfd(async_io.notifier());              // ← waits INSIDE the timed region
    while async_io.next_completed_request().is_some() { drained += 1; }
}
```

It re-waits on the eventfd each round, so waiting for I/O is explicitly, deliberately inside the
measurement. **This benchmark makes no claim to isolate reaping**, and by including the wait it is
immune to the Part 8 failure mode. That is a design choice worth noticing: `weltling` wrote both
loops, and used the wait-inclusive one for io_uring.

Also relevant: qcow2's io_uring engine only routes **single allocated cluster reads** through
io_uring. Multi-mapping reads, compressed clusters, backing-file reads, and **all writes** fall back
to synchronous I/O with synthetic completions (issue #8033, "Cycle 1" summary). So even in this
benchmark the io_uring fraction of the work is partial.

### 12.3 Why it is not equivalent, and why it does not make the raw benchmark redundant

| | `micro_block_qcow_async_read_128_us` | proposed raw io_uring drain |
|---|---|---|
| Image format | qcow2, with L1/L2 indirection | raw, no metadata |
| Timed region | metadata + submit + **wait** + reap | reap only (if Part 8 is handled) |
| Submission shape | one at a time | to be decided |
| Isolates the reap path | **no** | that is its entire purpose |
| Sensitive to storage latency | **yes, by design** | must not be |
| Backend under test | `QcowAsync`, partial io_uring | `RawAsync`, fully io_uring |

Three reasons its existence is not an argument against the raw benchmark:

1. **A composite measurement cannot detect a change in one component.** If the CQ reap cost doubled,
   `qcow_async_read` would move by a fraction of a percent and no one would notice. That is exactly
   the argument for `micro_bench_aio_drain` existing alongside the full block throughput tests, and
   it applies unchanged here.
2. **Different backend.** `QcowAsync` and `RawAsync` are separate `AsyncIo` implementations. Raw is
   what the factory prefers for raw images (`factory.rs:152-163`), and raw is what most VM
   configurations use. It currently has **no** io_uring coverage in the suite at all.
3. **The comparison the project would actually want is within-raw:** AIO drain versus io_uring drain
   on the same format, same file, same buffer. qcow2 cannot provide that because it has no AIO
   engine.

**The honest counter-argument, which belongs in the issue:** with 20 qcow io_uring benchmarks
already, a reviewer may reasonably ask what a 41st micro benchmark buys. The answer must be the
specific one - *nothing today isolates the raw io_uring completion path, which runs once per I/O in
the shipping VMM* - and not the general one, "io_uring is unbenchmarked", which is false.

---

## Part 13 - How the architecture lets us answer the 10 comprehension gates

For each gate: the prerequisite sections, the reasoning path, and what you should be able to derive.
**The answers are deliberately not written out.**

---

### Gate 1 - Syscalls per backend, on submission and on retrieval, for N = 128

**Prerequisites:** Part 4.1 (the four AIO syscalls), Part 4.2 (`submit_operation` uses a
one-element slice; `next_completion` is local-first with a 32-event batch), Part 5.1 (the shared-ring
model), Part 5.3 (`submit_batch` publishes all SQEs then calls `submit()` once; `next_completion`
reads the CQ directly), Part 2 step 6 (`batch_requests_enabled` splits the two submission shapes).

**Reasoning path:**
1. For each backend, find the single line that makes the submission syscall. Ask how many operations
   it carries.
2. Check `batch_requests_enabled()` for each backend, then follow `request.rs:257`/`:281` to see
   whether production submits one at a time or as a batch. They differ. State which shape you are
   counting.
3. For retrieval, walk `next_completion` and mark every line that crosses into the kernel. For AIO,
   simulate the `VecDeque` across successive calls (do the bookkeeping in Part 4.3 yourself). For
   io_uring, notice there is nothing to simulate.
4. Note the assumption your AIO count depends on: `io_getevents` with `min_nr = 0` returns what is
   ready, not necessarily 32.

**You should be able to derive:** four numbers (AIO submit, AIO reap, io_uring submit, io_uring
reap) for N = 128 and N = 256, the condition under which the AIO reap count is a floor rather than a
fact, and why the io_uring submit count depends on which submission shape you assume.

---

### Gate 2 - Why 32-event batching gave ~8x, and why the argument does not transfer

**Prerequisites:** Part 4.2 (the batch), Part 4.3 (the derivation), Part 7.8 and 7.9 (the arithmetic
and the measured table), Part 5.4 (what an io_uring reap actually costs), Part 6.2 (the two drain
diagrams).

**Reasoning path:**
1. Work out what fraction of pre-#7864 drain time was syscall entry/exit. Divide 61.8 µs by 128 and
   ask what costs ~480 ns.
2. Work out what the batch removes: not work, but *transitions*. 128 to 4.
3. Now ask the transfer question: for io_uring, what does a batched drain remove? Enumerate what a
   single `next_completion` costs from Part 5.4 and identify which parts a batch would eliminate and
   which it would not.
4. Compute the ratio of "what a batch removes" to "what remains" for each backend. That ratio is the
   speedup ceiling.

**You should be able to derive:** that the AIO win is a syscall-elimination win, that the io_uring
analogue can only coalesce atomics on shared cache lines, that the expected magnitude is therefore
much smaller, and why "much smaller" is a reason to measure first rather than a reason not to.

---

### Gate 3 - What `self.io_uring.completion()` does, what its Drop does, and what the kernel sees

**Prerequisites:** Part 5.1 (head/tail ownership: which side writes which index), Part 5.4 (the
unfolded call chain with crate line numbers).

**Reasoning path:**
1. Follow `completion()` into `borrow()` into `borrow_shared()`. Two field initialisations. Ask what
   memory each one reads and which side of the boundary writes that memory.
2. Follow `next()`. Ask what it reads and what it mutates - and note that what it mutates is a
   *local copy* of head, not the shared one.
3. Find the `Drop` impl. Ask what it stores, where, and with what ordering. Then ask the important
   question: **what does the kernel do differently as a result of that store?**
4. Classify each access as process-private or kernel-shared.
5. Then ask why `Acquire` on the load and `Release` on the store, and what would break with
   `Relaxed`.

**You should be able to derive:** the exact per-call access list, which two accesses touch memory the
kernel also touches, why the Drop is functionally necessary rather than cleanup, and what the memory
ordering is protecting.

---

### Gate 4 - What must be true for the AIO clock to measure reaping rather than waiting

**Prerequisites:** Part 7 (the exact timing boundary), Part 8.1 (what one `wait_for_eventfd`
guarantees), Part 8.2 (the libaio buffered-write behaviour and the sanity check), Part 4.1 (the
libaio limitation).

**Reasoning path:**
1. Read `wait_for_eventfd` and state precisely what it guarantees. Then state what the benchmark
   *needs* to be true. Notice the gap.
2. Ask what closes that gap. It is not in this repository's code - it is a property of `io_submit`.
   Name the property.
3. Sanity-check it against the published number: 8.2 µs for 128 completions. What per-completion
   cost is that? Is any storage device capable of that? What does the answer tell you about when the
   I/O finished?
4. Design the experiment that would settle it directly rather than by inference. (Hint: the
   submission loop is not timed. Time it.)

**You should be able to derive:** the precondition, why it is an assumption rather than a guarantee,
why the published magnitude is strong indirect evidence it holds, and the one measurement that would
turn indirect evidence into direct evidence.

---

### Gate 5 - When a CQE is not yet posted, how that corrupts the number, and io-wq's role

**Prerequisites:** Gate 4's reasoning, Part 5.1 (io-wq), Part 8.3 (why io_uring differs), Part 8.4
(the worked failure timeline).

**Reasoning path:**
1. State io_uring's submission contract: what does `io_uring_enter` guarantee about completion?
2. Enumerate the conditions under which the kernel cannot complete a buffered write inline. Where do
   those requests go?
3. Now re-run the AIO benchmark's timing boundary with io_uring underneath. Draw the timeline. Mark
   where the loop is reaping and where it is spinning.
4. Ask about the sign of the error. Can this failure make the number too small? Too large? Both?
5. Ask what a reviewer would conclude from the corrupted number placed next to the AIO number.
6. Then ask which of the three fixes in Part 8.6 preserves the benchmark's stated intent.

**You should be able to derive:** the mechanism, the direction and unbounded magnitude of the error,
why it is silent, why it makes the metric name a lie, and why this is the go/no-go question for the
whole contribution rather than a detail.

---

### Gate 6 - SQ/CQ capacities, AIO's `nr_events`, and CQ overflow

**Prerequisites:** Part 5.2 (what `IoUring::new` delegates to the kernel, and the flagged
discrepancy between the crate doc and the man page), Part 4.1 (`io_setup`), Part 2 step 0
(`create_async_io(ring_depth)` and who passes what).

**Reasoning path:**
1. Trace `create_async_io(num_ops)` to `IoUring::new(entries)` to `io_uring_setup`. Ask **who decides
   the capacities** - the crate or the kernel? Find the line that proves it.
2. State the kernel's documented default for `cq_entries` relative to `sq_entries`. Then note that
   the crate's own doc comment says something different, and decide which you would trust and how you
   would settle it in one line of code.
3. Trace AIO's `IoContext::new(queue_depth)` to `io_setup`. What does `nr_events` bound?
4. Compute headroom: with N operations in flight and the derived capacities, how much slack is there
   at N = 128 and N = 256?
5. For overflow, look in two places: `uring_data_io.rs:106-116` (what CH does when the **SQ** is
   full) and the io-uring crate's `overflow()` / `IORING_FEAT_NODROP` handling (what the **kernel**
   does when the **CQ** is full). These are different failures with different owners.

**You should be able to derive:** both capacity numbers for both N values, `nr_events` for AIO, why
the current sizes cannot overflow the CQ, what CH does on SQ-full (and why it is not an error), what
the kernel does on CQ-full, and the exact runtime check that would replace inference with fact.

---

### Gate 7 - `in_flight`: purpose, memory safety, and symmetry across backends

**Prerequisites:** Part 9 in full, Part 3.2 (the operation owns its memory), Part 2 steps 4 and 7
(where the iovec pointers come from and when they cross to the kernel).

**Reasoning path:**
1. Follow one `iov_base` value backwards: from `build_entry`'s `iovecs.as_ptr()` to the `Vec` in
   `GuestMemoryTarget` to the `Arc<GuestMemoryMmap>` to the guest's physical page. How many owners
   does that chain have and which one is holding it alive?
2. Now delete the `in_flight.insert` line in your head and follow the same chain again. Write down
   the sequence of events that ends in a fault. Be specific about *who* writes to freed memory.
3. Find the ordering constraint - insert before publish - and the comment that states it. Ask why the
   reverse order would be a race rather than merely untidy.
4. Explain `Option`: what is stored as `None`, and why reserve a key at all for an operation with no
   buffers?
5. Compare the two backends line by line: `aio_data_io.rs:148-151` versus
   `uring_data_io.rs:235-238`. Same or different?
6. Then ask the measurement question: if the cost is identical on both, can it explain any AIO versus
   io_uring difference? And can it still dominate the io_uring number in absolute terms?

**You should be able to derive:** the memory-safety argument end to end, why `Drop` prefers leaking
to freeing, why the cost is a constant across the comparison, and why it nonetheless needs
acknowledging in any io_uring drain result.

---

### Gate 8 - Why completion draining is a per-I/O hot path, and the line that proves it

**Prerequisites:** Part 10 in full, Part 2 steps 11-12, Part 3.1 (`next_completed_request` returns
`Option`).

**Reasoning path:**
1. Find the only production caller of `next_completed_request`. Read the loop condition, not just the
   body.
2. From the loop's structure, derive how many times the call happens per completed request, and the
   `+1` for the terminating `None`.
3. Ask what thread this runs on and what else that thread is responsible for (`block.rs:686-693`).
4. Multiply: pick an IOPS figure, multiply by per-call cost, express as a fraction of a core.
5. Then apply the project's own criterion from #7847 - per-I/O versus per-rare-event - and decide
   which side this falls on.

**You should be able to derive:** the exact line, the call count per I/O, why the single-threaded
submit-and-complete design amplifies it, and the argument you would give a maintainer, stated in
their own terms rather than yours.

---

### Gate 9 - Why a `micro_` test affects neither the dashboard nor CI, and the two commits

**Prerequisites:** Part 11.3 (the prefix as a dispatch signal), Part 11.4 (substring filtering),
Part 11.5 (the workflow, the two commits, the quoted reservation).

**Reasoning path:**
1. Read the workflow's `on:` trigger. Does it run on pull requests or after merge? What does that
   alone rule out?
2. Read the `--test-exclude` argument. Match it against the filter code at `main.rs:1892` and confirm
   the mechanism is substring matching on the name.
3. Ask what gets uploaded to the dashboard, and whether an excluded test can reach it.
4. Find the two commits, read their messages, and note the *dates* relative to #7847's merge. What
   does the sequencing tell you about why the exclusion exists?
5. Then take the argument seriously in both directions: what does this make easier for the
   contribution, and what roadmap claim does it invalidate?

**You should be able to derive:** the three independent reasons a new `micro_` metric is invisible to
CI and the dashboard, both commit hashes with what each one did, and the strategic consequence for
`OSS-ROADMAP.md` §1's success metric.

---

### Gate 10 - What the qcow async read benchmark measures, and why it does not make the raw one redundant

**Prerequisites:** Part 12 in full, Part 7 (the AIO benchmark's boundary, for contrast), Part 5.3
(which qcow operations reach io_uring at all).

**Reasoning path:**
1. Mark the timing boundary of `micro_bench_qcow_async_read` precisely. List everything inside it.
2. Read `drain_async_completions` and compare it with the AIO benchmark's loop. Where is the wait in
   each? What does that difference mean for what is being measured?
3. Ask what fraction of the timed region is completion reaping. Then ask: if reap cost changed by
   50%, by how much would this benchmark move?
4. Ask which backend each benchmark exercises, and which one the factory picks for a raw image.
5. Ask what fraction of qcow2 operations reach io_uring at all.
6. Finally, construct the *strongest* argument against the raw benchmark, and then answer it. If you
   cannot state the objection convincingly, you are not ready to defend the PR.

**You should be able to derive:** the four components inside the qcow benchmark's clock, why a
composite metric cannot resolve a component change, the two backend-level differences, and a defence
of the raw benchmark that concedes the true part of the objection rather than denying it.

---

## Part 14 - Source map

All paths relative to the `cloud-hypervisor` clone at `1af93ac70`. External crate paths under
`~/.cargo/registry/src/index.crates.io-*/`.

### Device layer

| Concept | File | Function / type | Why we read it |
|---|---|---|---|
| Submission path | `virtio-devices/src/block.rs:244` | `process_queue_submit` | Where descriptor chains become I/O operations |
| Descriptor iteration | `virtio-devices/src/block.rs:271` | `queue.iter(mem)?.next()` | Rung 2's available ring in production |
| Head-reuse defence | `virtio-devices/src/block.rs:239` | `is_head_in_flight` | Why `user_data` uniqueness is enforced twice |
| Batch submission | `virtio-devices/src/block.rs:412` | `submit_batch_requests` | The io_uring production submission shape |
| **Completion drain** | **`virtio-devices/src/block.rs:505`** | **`while let Some(...) = next_completed_request()`** | **The line proving per-I/O hot path (Gate 8)** |
| Completion handling | `virtio-devices/src/block.rs:498` | `process_queue_complete` | status byte, `add_used`, counters |
| Inflight lookup | `virtio-devices/src/block.rs:478` | `find_inflight_request` | How `user_data` maps back to a `Request` |
| Interrupt decision | `virtio-devices/src/block.rs:442`, `:642` | `try_signal_used_queue`, `signal_used_queue` | Rung 2's `EVENT_IDX` suppression in production |
| Event loop | `virtio-devices/src/block.rs:681`, `:699` | `run`, `handle_event` | One thread serves kicks and completions |
| Event constants | `virtio-devices/src/block.rs:62`, `:64` | `QUEUE_AVAIL_EVENT`, `COMPLETION_EVENT` | The two epoll sources |

### Request layer

| Concept | File | Function / type | Why we read it |
|---|---|---|---|
| virtio-blk parsing | `block/src/io/request.rs:84` | `Request::parse` | Descriptor direction rules, status descriptor |
| Request state | `block/src/io/request.rs:74` | `struct Request` | `start: Instant` is the latency origin |
| Submission dispatch | `block/src/io/request.rs:232` | `execute_async` | Where batch-vs-single is decided (`:257`, `:281`) |
| Operation construction | `block/src/io/request.rs:466` | `build_data_operation` | Zero-copy versus bounce buffer |
| Alignment test | `block/src/io/request.rs:499` | `guest_memory_is_aligned` | Why buffered I/O is always zero-copy |
| Read completion | `block/src/io/request.rs:589` | `complete_async` | Where a bounce buffer returns to the guest |

### The abstraction

| Concept | File | Function / type | Why we read it |
|---|---|---|---|
| The seam | `block/src/io/async_io.rs:106` | `trait AsyncIo` | Four required methods define the boundary |
| Capability queries | `block/src/io/async_io.rs:167`, `:185` | `batch_requests_enabled`, `alignment` | How callers adapt without naming a backend |
| Ownership contract | `block/src/io/async_io.rs:109-113` | doc comment on `submit_data_operation` | The rule Part 9 enforces |
| Unit of work | `block/src/io/async_io/operation.rs:16` | `enum AsyncIoOperation` | Four variants, two memory sources |
| iovec access | `block/src/io/async_io/operation.rs:168` | `iovecs()` | The pointer that crosses to the kernel |
| Unit of result | `block/src/io/async_io/completion.rs:16` | `AsyncIoCompletion` | How ownership returns |
| Shared plumbing | `block/src/io/async_io/completion.rs:51` | `CompletionCommon` | eventfd + `VecDeque` + synthetic completions |
| eventfd flags | `block/src/io/async_io/completion.rs:60` | `EFD_NONBLOCK` | Why `wait_for_eventfd` spins |
| Duplicate detection | `block/src/io/async_io/common.rs:26` | `validate_batch` | `user_data` uniqueness |

### AIO backend

| Concept | File | Function / type | Why we read it |
|---|---|---|---|
| AIO state | `block/src/io/async_io/aio_data_io.rs:21` | `struct AioDataIo` | Field order is a safety decision (`:22-23`) |
| Context creation | `block/src/io/async_io/aio_data_io.rs:35` | `new` | `io_setup(nr_events)` |
| Submission | `block/src/io/async_io/aio_data_io.rs:52` | `submit_operation` | One `io_submit` per operation; `IOCB_FLAG_RESFD` at `:72` |
| **Batched reap** | **`block/src/io/async_io/aio_data_io.rs:131`** | **`next_completion`** | **32-event batch, `min_nr = 0` (Gates 1, 2)** |
| Engine wrapper | `block/src/formats/raw/engine_aio.rs:30`, `:54`, `:89` | `RawAio` | Alignment check, synthetic completions |
| libaio binding | `vmm-sys-util-0.15.0/src/linux/aio.rs:86`, `:128`, `:218` | `IoContext::{new,submit,get_events}` | The three syscalls |
| Event layout | `vmm-sys-util-0.15.0/src/linux/aio.rs:66` | `IoEvent` | 32 bytes; explains #7864's "1 KB stack" |

### io_uring backend

| Concept | File | Function / type | Why we read it |
|---|---|---|---|
| Ring state | `block/src/io/async_io/uring_data_io.rs:21` | `struct UringDataIo` | `in_flight` + `needs_submit_retry` |
| Ring creation | `block/src/io/async_io/uring_data_io.rs:35` | `new` | `IoUring::new` + `register_eventfd` |
| Batch submission | `block/src/io/async_io/uring_data_io.rs:98` | `submit_batch` | Capacity check, SQ-full as `-EAGAIN`, one `submit()` |
| SQE construction | `block/src/io/async_io/uring_data_io.rs:179` | `build_entry` | `Readv`/`Writev` with `user_data` |
| **CQ reap** | **`block/src/io/async_io/uring_data_io.rs:222`** | **`next_completion`** | **The function a drain benchmark measures (Gates 1, 2, 3)** |
| Teardown | `block/src/io/async_io/uring_data_io.rs:246` | `Drop` | Deliberate leak over use-after-free (Gate 7) |
| Engine wrapper | `block/src/formats/raw/engine_uring.rs:27`, `:90`, `:94` | `RawAsync` | `batch_requests_enabled() == true` |
| CQ borrow | `io-uring-0.7.12/src/cqueue.rs:78` | `borrow_shared` | Acquire load of the kernel-written tail |
| CQ iteration | `io-uring-0.7.12/src/cqueue.rs:169` | `Iterator::next` | Reads the CQE, advances a local head |
| **CQ drop** | **`io-uring-0.7.12/src/cqueue.rs:162`** | **`Drop`** | **Release store of head; how the kernel learns the slot is free (Gate 3)** |
| Ring setup | `io-uring-0.7.12/src/lib.rs:125`, `:160` | `new`, `with_params` | Kernel fills `sq_entries`/`cq_entries` (Gate 6) |
| Enter | `io-uring-0.7.12/src/submit.rs:140`, `:146` | `submit`, `submit_and_wait` | The one submission syscall |

### Backend selection

| Concept | File | Function / type | Why we read it |
|---|---|---|---|
| Backend enum | `block/src/formats/raw/mod.rs:36` | `RawBackend` | `IoUring` is `#[cfg(feature = "io_uring")]` at `:42` |
| Construction | `block/src/formats/raw/mod.rs:152` | `create_async_io` | Backend to engine mapping |
| Alignment fast path | `block/src/formats/raw/mod.rs:168` | `operation_is_aligned` | `alignment == 0` short-circuit |
| Runtime selection | `block/src/factory.rs:144` | `open_raw` | io_uring, then AIO, then sync |
| Probe caching | `block/src/factory.rs:62`, `:70` | `io_uring_supported`, `aio_supported` | `OnceLock`, once per process |
| io_uring probe | `block/src/lib.rs:255` | `block_io_uring_is_supported` | Ring creation + opcode probe |
| AIO probe | `block/src/lib.rs:249` | `block_aio_is_supported` | `IoContext::new(1).is_ok()` |
| O_DIRECT alignment | `block/src/aligned_file.rs:44`, `:51` | `AlignedFile` | `direct = false` gives `alignment = 0` |

### Benchmarks and harness

| Concept | File | Function / type | Why we read it |
|---|---|---|---|
| **AIO drain bench** | **`performance-metrics/src/micro_bench_block.rs:28`** | **`micro_bench_aio_drain`** | **The template under evaluation (Part 7)** |
| qcow io_uring bench | `performance-metrics/src/micro_bench_block.rs:377` | `micro_bench_qcow_async_read` | Submit + wait + reap (Part 12) |
| qcow batch bench | `performance-metrics/src/micro_bench_block.rs:404` | `micro_bench_qcow_batch_read` | The batched submission path |
| Workload helpers | `performance-metrics/src/util.rs:27`, `:106`, `:149` | `sized_tempfile`, `guest_memory_buffer`, `submit_reads` | Shared benchmark scaffolding |
| io_uring qcow setup | `performance-metrics/src/util.rs:70` | `qcow_async_tempfile` | `use_io_uring = true` |
| Wait-inclusive drain | `performance-metrics/src/util.rs:182` | `drain_async_completions` | Contrast with the AIO benchmark's loop |
| eventfd wait | `performance-metrics/src/util.rs:388` | `wait_for_eventfd` | Returns on the FIRST signal (Part 8.1) |
| Test descriptor | `performance-metrics/src/main.rs:280` | `PerformanceTest` | name, fn ptr, control, unit adjuster |
| Registration | `performance-metrics/src/main.rs:391` | `TEST_LIST: [_; 100]` | Fixed-length array; must be bumped |
| AIO entries | `performance-metrics/src/main.rs:1242-1265` | the two `micro_block_raw_aio_drain_*` | 5s timeout, 20 iterations, 5 warmup |
| Statistics | `performance-metrics/src/main.rs:287` | `run` | mean and std_dev only, no percentiles |
| Filtering | `performance-metrics/src/main.rs:1878-1893` | filter/exclude | Substring matching both ways |
| VM skip | `performance-metrics/src/main.rs:1937` | `needs_vm_tests` | `micro_` prefix skips VM lifecycle |
| Feature wiring | `performance-metrics/Cargo.toml` | `block = { features = ["io_uring", ...] }` | No `cfg` gating needed in benchmarks |

### CI and process

| Concept | File / ref | Why we read it |
|---|---|---|
| Metrics workflow | `.github/workflows/integration-metrics.yaml` | Push-to-main only; `--test-exclude micro_,block_qcow2` |
| Micro exclusion | commit `f4772e7f4` (2026-03-18) | "avoid dashboard pollution" |
| qcow2 exclusion | commit `b5aeabe77` (2026-07-08) | Same mechanism, later |
| Harness origin | PR #7847, commit `00957fa9d` | Micro benchmark support + the AIO drain bench |
| The optimization | PR #7864 | The 8x/13x table; why the bench existed |
| qcow benchmarks | PR #8034 | 20 benchmarks, 35 commits, 2 review comments |
| Block refactor tracking | issue #8033 | Open, mostly `TBD` assignees, this subsystem |
| Contribution rules | `CONTRIBUTING.md:157-160` | **Issue required before an enhancement PR** |
| LLM disclosure | `CONTRIBUTING.md:241-275`, `AGENTS.md:72-76` | `Assisted-by:`, never `Co-authored-by` |

---

## Part 15 - Rung 4 comprehension checklist

Statements you should be able to explain **in your own words, without this file open**. Nothing is
marked complete; that is yours to do.

### A. Architecture

- [ ] Name the seven layers between a guest `write()` and the host disk, and say what each one is
      responsible for and what it is deliberately ignorant of.
- [ ] Explain why the virtio-blk device model contains no reference to io_uring, and what would break
      if it did.
- [ ] Explain how a backend is chosen at runtime and why it cannot be a compile-time decision.
- [ ] Say which parts of this stack you already built in rungs 1-3 and which parts are new.
- [ ] Explain why one worker thread handles both guest kicks and I/O completions, and what that costs.

### B. Virtio block path

- [ ] Walk a 4 KiB buffered write from the guest kick to the used ring, naming the function at each
      transition.
- [ ] Explain what `Request::parse` validates and why each check exists from a security standpoint.
- [ ] Explain what `user_data` is in production and trace it through all four layers it crosses.
- [ ] Explain why `process_queue_submit` caps its loop at the virtqueue size.
- [ ] Explain how a completion becomes a guest interrupt, and why not every completion causes one.

### C. AsyncIo

- [ ] State the four required methods of `AsyncIo` and why those four are sufficient.
- [ ] Explain the difference between the four `AsyncIoOperation` variants and when each is used.
- [ ] Explain the three jobs `CompletionCommon` performs.
- [ ] Explain what a "synthetic completion" is, give two examples from the tree, and say why callers
      cannot distinguish them from kernel completions.
- [ ] Explain how `batch_requests_enabled` lets the request layer adapt without naming a backend.

### D. AIO

- [ ] Name the four AIO syscalls and what each does.
- [ ] Explain the `iocb` fields CH sets, including the `aio_buf`/`aio_nbytes` convention for vectored
      opcodes.
- [ ] Explain what `IOCB_FLAG_RESFD` buys and why an epoll-driven VMM needs it.
- [ ] Derive the `io_getevents` count for 128 and 256 completions from the batch size, and state the
      assumption that makes your count a floor rather than a fact.
- [ ] Explain libaio's buffered-I/O limitation and why it matters to a benchmark rather than only to
      a VMM.
- [ ] Explain why `AioDataIo`'s field declaration order is load-bearing.

### E. io_uring

- [ ] Draw the SQ and CQ rings and mark which side writes each of the four indices.
- [ ] Explain why submitting needs a syscall but reaping does not.
- [ ] Unfold `self.io_uring.completion().next()` into every memory access it performs.
- [ ] Explain what the `CompletionQueue` `Drop` does and why the kernel depends on it.
- [ ] Explain the Acquire/Release pair and what would break under `Relaxed`.
- [ ] Explain what io-wq is and when the kernel uses it.
- [ ] Explain what CH does when the SQ is full, and why that is not an error.
- [ ] Explain who decides SQ and CQ capacities, and how you would confirm them at runtime rather than
      by reading documentation.

### F. Completion lifecycle

- [ ] Explain what `in_flight` holds and why an entry must outlive the kernel's use of the pointers.
- [ ] Construct the use-after-free that would occur without it, naming who writes to freed memory.
- [ ] Explain why insertion must precede publication to the ring.
- [ ] Explain why some entries hold `None` and why their keys are still reserved.
- [ ] Explain why `Drop for UringDataIo` prefers leaking to freeing.
- [ ] State whether the `in_flight` cost differs between backends, and what that means for comparing
      them.

### G. Benchmark methodology

- [ ] Mark the exact timing boundary of `micro_bench_aio_drain` and list what is inside and outside.
- [ ] Explain what the benchmark claims to measure and name three things it does not.
- [ ] Reconstruct #7864's 8x from the syscall counts and check it against the published number.
- [ ] Explain the unstated assumption behind starting the clock after one `wait_for_eventfd`.
- [ ] Explain why that assumption is plausible for AIO, using the published magnitude as evidence.
- [ ] Explain why it may fail for io_uring, and draw the timeline of the failure.
- [ ] State the direction of the error and why the failure is silent.
- [ ] Give three ways to design an io_uring drain benchmark that does not inherit the assumption, and
      say what each one gives up.
- [ ] Explain why comparing an AIO drain number with an io_uring drain number in one table is
      misleading, and what within-backend comparison would be legitimate instead.

### H. performance-metrics and CI

- [ ] Explain how a benchmark is registered and what breaks at compile time if you get it wrong.
- [ ] Explain what the `micro_` prefix changes, in all three places it is consulted.
- [ ] Explain when the metrics workflow runs and what that alone rules out.
- [ ] Name the two commits that established the `micro_` exclusion and what each one did.
- [ ] Explain the maintainer reservation those commits answer, and why the sequencing relative to
      #7847 matters.
- [ ] State which roadmap claim the CI exclusion invalidates, and why the ladder survives anyway.
- [ ] State the criterion this project uses to decide whether a micro benchmark deserves to exist,
      and apply it to the io_uring drain path.

---

## Open questions for `../docs/OPEN-QUESTIONS.md`

Per the rung-4 README, each map ends with the questions the code did not answer. These are candidates
for upstream discussion, not conclusions.

1. **Redundant eventfd writes on the AIO reap path.** `CompletionCommon::complete` writes the eventfd
   per completion (`completion.rs:72`), and `AioDataIo::next_completion` calls it once per reaped
   kernel event (`:145`) even though the kernel already signalled via `aio_resfd`. Is this a
   measurable cost at queue depth 128, and is it intentional? **[VERIFY]**
2. **Per-completion CQ borrow.** `UringDataIo::next_completion` constructs and drops a
   `CompletionQueue` per completion, producing one Acquire load and one Release store per CQE on
   kernel-shared cache lines. Would a single-borrow drain measurably reduce that, and is the
   difference above noise? **[VERIFY]** This is the question a raw io_uring drain benchmark exists to
   answer.
3. **Do the crate docs or the man page describe CQ sizing correctly?** `io-uring`'s `build` doc says
   entries apply to both queues; `io_uring_setup(2)` says CQ defaults to twice SQ. One line reading
   `params.cq_entries` settles it. **[VERIFY]**
4. **Does `wait_for_eventfd`'s single-signal semantics hold for every existing benchmark that relies
   on it?** `micro_bench_aio_drain` is the only one that starts a clock after it, but the pattern is
   worth checking before it is copied. **[VERIFY]**

---

*End of map. Next stage is experimental: reproduce the AIO baseline, then answer open question 4 by
measurement before any benchmark is designed. No code is to be written against the upstream tree
until that measurement exists.*
