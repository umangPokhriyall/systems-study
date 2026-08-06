# Exercises

Ordered easy to hard. Each states what it teaches, because an exercise that only produces working
code has been wasted.

Status is recorded honestly. "not done" is a legitimate final state.

| # | Exercise | Status |
|---|---|---|
| 1 | Change the image and watch it arrive | not done |
| 2 | Use `UFFDIO_ZEROPAGE` for holes | not done |
| 3 | Kill the handler mid-restore | not done |
| 4 | Forget the `copy` out-parameter | not done |
| 5 | Batch the wakeups with `DONTWAKE` | not done |
| 6 | Measure a realistic access pattern | not done |
| 7 | Several handler threads | not done |
| 8 | Move the handler to another process | not done |
| 9 | Back the image with a real file | not done |
| 10 | Warm the handler core deliberately | not done |
| 11 | Write protection and dirty tracking | not done |
| 12 | Fault a real guest | not done |

---

### 1. Change the image and watch it arrive

Fill the source with something other than the page number - a text file, say - and read it back
through the region. Confirm the region and the source are byte-identical afterwards.

*Teaches:* that "restoring memory" is exactly this and nothing more. The mystique of snapshot
restore is entirely in the *bookkeeping* around this copy, not in the copy.

### 2. Use `UFFDIO_ZEROPAGE` for holes

A real memory image is sparse: most of a guest's address space was never written. Extend the handler
so pages outside a "populated" set are installed with `UFFDIO_ZEROPAGE` instead of `UFFDIO_COPY`, and
measure the difference.

*Teaches:* the second install ioctl, and why it exists - there is no source to read, so the kernel
can map a shared zero page instead of copying 4 KiB. Predict the ratio before measuring it.

### 3. Kill the handler mid-restore

Start a restore over 4,096 pages and have the handler `panic!` after 100 faults. Observe what the
faulting thread does.

Then work out what a VMM could do to notice.

*Teaches:* README §5, from the inside. The expectation is a hang; the reality is fast, silent,
zero-filled corruption. Nothing else in this ladder has a failure mode this dangerous.

### 4. Forget the `copy` out-parameter

Change `Uffd::copy` to check only the ioctl return value and ignore `c.copy`. Then make a copy fail -
an unaligned `dst` is easiest - and watch the faulting thread.

*Teaches:* why the out-parameter convention is worth a paragraph in the walkthrough. The failure is
a hang with no error message anywhere.

### 5. Batch the wakeups with `DONTWAKE`

The handler currently wakes on every `UFFDIO_COPY`. Install several runs with
`UFFDIO_COPY_MODE_DONTWAKE` and issue one `UFFDIO_WAKE` over the whole range, then measure.

*Teaches:* that the wakeup is a separable cost from the install - which §3.1 already suggests is the
dominant one. This is the experiment that would turn that suggestion into a number.

### 6. Measure a realistic access pattern

README §3.3 admits the batch column is an upper bound: the faulting thread walks pages in ascending
order, so every prefaulted page is used. Replace the walk with something guest-like - a shuffled
order, or a Zipf distribution over a working set - and re-measure the batch sweep.

Handle the `EEXIST` that now appears when batches overlap.

*Teaches:* the difference between a benchmark and a model. I expect prefaulting to look considerably
worse, and finding out by how much is the single most useful thing in this list for the OSS roadmap:
it is the number Cloud Hypervisor's prefault threads are implicitly betting on.

### 7. Several handler threads

Two or four handlers reading the same uffd. They will race, and `UFFDIO_COPY` will start returning
`-EEXIST`, which the code already tolerates.

Measure whether it helps at all, and at which batch size it stops helping.

*Teaches:* why `EEXIST` is not an error, and what Cloud Hypervisor's prefault *thread count* knob is
actually buying. Note that the faulting side here is a single thread; a real guest has one per vCPU,
so this experiment is missing half the parallelism.

### 8. Move the handler to another process

Pass the uffd over a unix socket with `SCM_RIGHTS`. The region must be `MAP_SHARED` for the handler
process to have anything to copy into - and that choice is made at `mmap` time, so it has to be
changed first.

*Teaches:* the deployment shape both Cloud Hypervisor and Firecracker actually use, and why they use
it: the handler can be sandboxed separately from the VMM. Also the first thing in this ladder that
is genuinely fiddly for reasons that are not conceptual.

### 9. Back the image with a real file

Replace the in-memory source with `pread` from a file. Measure with the file in page cache and with
it evicted (`posix_fadvise(POSIX_FADV_DONTNEED)`).

*Teaches:* that §3's numbers assume the image is already in memory, which a real restore-from-disk
does not. This is where the fault tail actually comes from in production, and where io_uring enters
the picture - connecting this rung to the block-path work in `OSS-ROADMAP.md`.

### 10. Warm the handler core deliberately

§3.1 concluded that cross-core fault cost is dominated by waking an idle CPU. Test it directly:
pin a thread doing trivial work to the handler's core so it never idles, and re-run the `poll`
configurations.

If the conclusion is right, the `poll` numbers should converge on the `spin` numbers without the
handler itself spinning.

*Teaches:* how to confirm a mechanism rather than infer it. The `spin` control changed two things at
once - no sleeping *and* no scheduler involvement; this changes only one.

### 11. Write protection and dirty tracking

Register a range with `UFFDIO_REGISTER_MODE_WP` instead of `MISSING`, and catch writes to pages that
are already present. Count dirtied pages over a workload.

*Teaches:* the other half of the userfaultfd API, and the mechanism behind live migration's dirty
tracking - which is the same problem as snapshotting, run continuously.

### 12. Fault a real guest

Combine with rung 1: register the toy VMM's guest memory to a userfaultfd, boot the guest, and
service its faults. Count how many pages a real-mode program actually touches.

*Teaches:* that everything in this ladder is one system. It is also the smallest possible version of
the flagship's central experiment, and the point where rungs 1 and 3 stop being separate exercises.
