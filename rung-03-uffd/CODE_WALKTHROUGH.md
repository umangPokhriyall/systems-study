# Code walkthrough

The code in execution order, with every syscall and ioctl explained.

```
toy-uffd-raw/src/
  uffd_sys.rs    the ABI: the syscall, computed ioctl numbers, the structures
  uffd.rs        an owned userfaultfd, and the region it watches
  topology.rs    CPU topology and pinning, for the placement experiment
  main.rs        the demo, and the two measurement sweeps
  ../tests/restore.rs   6 tests
toy-uffd-crates/src/
  main.rs        the same pager on the userfaultfd crate
```

---

## Part 0 - `uffd_sys.rs`

### The syscall

```rust
pub unsafe fn userfaultfd(flags: i32) -> i32 {
    unsafe { libc::syscall(libc::SYS_userfaultfd, flags) as i32 }
}
```

Called through `syscall(2)` because glibc has no wrapper for it. `flags` takes `O_CLOEXEC`,
`O_NONBLOCK` and `UFFD_USER_MODE_ONLY`.

`O_CLOEXEC` matters here for the same reason it did in rung 1, and more: a leaked userfaultfd in a
child process is a leaked ability to stall that process's threads inside the kernel.

### The ioctl numbers

Computed from the `_IOC` encoding exactly as in rung 1, with the size field derived from
`size_of::<T>()`. If a structure below were laid out wrongly, the *request number* would be wrong
and the kernel would reject the call with `ENOTTY` rather than interpreting the wrong bytes.

One inconsistency copied faithfully rather than corrected: `UFFDIO_WAKE` and `UFFDIO_UNREGISTER` are
spelled with `_IOR` in the kernel header even though the kernel *reads* the structure. The direction
bits are named from userspace's point of view and the header is not consistent about it. The number
has to match, so the code matches it and says why.

### The structures

`uffdio_api`, `uffdio_range`, `uffdio_register`, `uffdio_copy`, `uffdio_zeropage`, and the pagefault
variant of `uffd_msg`. All `#[repr(C)]`, all with sizes asserted at compile time.

The `uffd_msg` assertion is the load-bearing one:

```rust
assert!(size_of::<uffd_msg>() == 32);
```

The kernel writes exactly 32 bytes per message and `read()` returns a multiple of that. A wrong size
here would silently mis-frame every message after the first - so the first fault would work and the
second would be serviced at a garbage address.

---

## Part 1 - `Uffd::new`

### `userfaultfd(O_CLOEXEC [| O_NONBLOCK])`

Tried **without** `UFFD_USER_MODE_ONLY` first, and retried with it on `EPERM`. Deliberately that
way round, so the code reports which path it took rather than hiding the difference: on a machine
with `vm.unprivileged_userfaultfd = 1` the plain call works, and on one with it set to 0 only the
restricted form does. The restricted fd genuinely cannot do everything the unrestricted one can, so
which you got is worth knowing.

`non_blocking` is a **parameter**, not a constant, and that is a scar. See `COMMON-MISTAKES.md` #2:
a handler that assumes a blocking fd, gets a non-blocking one, sees `EAGAIN`, mistakes it for
end-of-file and exits - after which the uffd is closed and every subsequent fault is silently
resolved with a zero page.

### `ioctl(fd, UFFDIO_API, &uffdio_api)`

The mandatory handshake. Must be the first ioctl on the fd; anything else before it fails with
`EINVAL`.

```rust
let mut api = uffdio_api { api: UFFD_API, features: 0, ioctls: 0 };
```

`features: 0` asks for nothing, which makes this a pure query - the kernel writes back what it
supports in `features` and `ioctls`.

That matters because **the handshake is a negotiation, not a query.** Asking for a feature the
kernel lacks fails the *whole ioctl*, taking the fd's setup with it. So "ask for nothing, read what
you get, and re-do the handshake on a fresh fd if you want something specific" is the only safe
discovery procedure. The `userfaultfd` crate exposes this as `require_features`, which makes the
requirement explicit and checkable - a genuine improvement over doing it by hand.

---

## Part 2 - `Region::new` and `register_missing`

### `mmap(NULL, len, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS|MAP_NORESERVE, -1, 0)`

Ordinary anonymous memory. Nothing about it is special until it is registered.

`MAP_PRIVATE` is what a single-process pager uses. A VMM whose handler lives in a **separate
process** - which is how both Cloud Hypervisor and Firecracker deploy this, so the handler can be
sandboxed away from the VMM - needs `MAP_SHARED`, and must pass the uffd over a unix socket with
`SCM_RIGHTS`. That choice is made at allocation time and cannot be revised later.

### `ioctl(fd, UFFDIO_REGISTER, &uffdio_register)` with `MODE_MISSING`

From this call onward, any thread touching an unbacked page in the range is parked and a message
appears on the fd.

Registration does not evict anything. Pages already present stay present and never fault - which is
why the benchmark must call `MADV_DONTNEED` before each round.

The out-parameter `ioctls` reports which operations are valid **for this range**, and it is worth
checking rather than assuming: a range registered in WP mode does not accept `UFFDIO_COPY`. The demo
prints it (`0x13c` on this machine).

`MODE_WP` - report faults on *writes to present pages* - is the other half of the API and is not
used here. It is the basis of dirty tracking for live migration.

---

## Part 3 - the handler loop

```rust
loop {
    poll(uffd)                  // park until the kernel reports a fault
    read(uffd) -> uffd_msg      // which address, and was it a write?
    decide what belongs there
    UFFDIO_COPY                 // install it; the kernel wakes the faulting thread
}
```

### `poll` then `read`, rather than a blocking `read`

One extra syscall per fault. Deliberate: it is the shape every real handler has, because it
multiplexes the userfaultfd with a shutdown channel and a control socket. The cost is stated in the
README rather than optimised away.

`read_event_spin` is the alternative - spin on the non-blocking fd, never sleep - and it exists as a
*measurement instrument*, not as a mode anyone should ship. It is what isolates wakeup cost from
fault cost in §3.1, and it is the reason that section reaches a defensible conclusion instead of a
surprising number.

### Aligning the fault address

```rust
let addr = msg.pagefault_address as usize & !(PAGE - 1);
```

The kernel already rounds down. Rounding again is free insurance: an unaligned `dst` makes
`UFFDIO_COPY` fail with `EINVAL`, and the faulting thread then hangs forever with nothing near it
indicating why.

### `UFFDIO_COPY`

```rust
uffdio_copy { dst, src, len, mode, copy: 0 }
```

`len` may cover many pages - that is the whole of the batching experiment. `mode` takes
`UFFDIO_COPY_MODE_DONTWAKE`, which installs without waking, so a handler can install several runs
and then issue one `UFFDIO_WAKE` over the lot.

The **`copy` field is an out-parameter carrying an errno**, and the ioctl returns success anyway:

```rust
if c.copy < 0 {
    if c.copy == -(libc::EEXIST as i64) { return Ok(0); }
    return Err(io::Error::from_raw_os_error(-c.copy as i32));
}
```

Missing this check is the classic userfaultfd bug. The handler believes it installed a page, wakes
nobody, and the faulting thread waits forever.

`-EEXIST` means another thread installed the page first. Normal with several handler threads, and
translated to `Ok(0)` rather than an error - a handler that treats it as fatal dies the first time
two faults on the same page race, which is exactly the condition a multi-threaded handler exists to
create.

### Batching

```rust
let run = batch_pages.min(region_pages - page);
uffd.copy(addr, src.add(page * PAGE), run * PAGE, true)
```

Forward-only from the faulting page. The walk in the benchmark is sequential, so every page in the
run is guaranteed absent, which is why no `EEXIST` handling is needed *on this path*. A random-access
walk would overlap previously installed runs and would need it - and that is exercise 6.

---

## Part 4 - `Region::reset`

```rust
madvise(ptr, len, MADV_DONTNEED)
```

On private anonymous memory this is **destructive**: it frees the pages and the range reverts to
unbacked. On a userfaultfd-registered range that re-arms the missing-fault notification.

The whole benchmark depends on it. Without it, each page could be measured exactly once per process
and there would be no distribution to report. `tests::reset_re_arms_the_fault` pins the behaviour
down, because if a kernel change ever made `MADV_DONTNEED` non-destructive the benchmark would
quietly measure hits instead of faults and report a wonderful number.

Worth noticing that it is also a production footgun: `MADV_DONTNEED` on guest memory silently
discards guest data. It is how a balloon driver returns memory to the host.

---

## Part 5 - `topology.rs`

`pin_to(cpu)` is `sched_setaffinity` on the calling thread.

Pinning matters more than it looks. Without it the scheduler is free to migrate the handler onto the
faulting thread's CPU or away from it *during* a run, which does not add noise so much as silently
average two different experiments together.

Note that pinning happens **inside** the spawned thread. Affinity is per-thread, so setting it from
the parent would move the parent.

`Topology::detect` reads `thread_siblings_list` and `core_id` from sysfs and picks one CPU for each
of the three placements. It returns `None` for a placement the machine cannot offer - a machine
without SMT has no sibling - and the benchmark prints "skipping placement: not available" rather
than substituting something else and labelling it as the thing that was asked for.

---

## Part 6 - `main.rs`

### `demo()`

A miniature snapshot restore. The "image" is 64 pages with page N filled with byte N - recognisable,
so a wrong page is *obvious* rather than merely wrong.

Pages are touched in a scattered order (`7, 0, 63, 31, 7, 32, 1, 31`) because a sequential walk
would not distinguish "the handler installed the right page" from "the handler installed pages in
order".

The assertion is on the **fault count**, not just the content: 8 touches over 6 distinct pages must
produce exactly 6 faults. The repeats at 7 and 31 must not fault, because once installed a page is
ordinary memory - which is the property that makes demand paging pay for itself.

The first fault in the trace is ~3.5 ms rather than ~6 µs. That is thread startup, not fault cost:
the handler thread has not been scheduled yet when the first fault arrives. Real restore paths have
the same shape and warm the handler before starting vCPUs.

### `bench_all` and `run_config`

Each configuration gets a fresh `Region`, a fresh `Uffd`, and a fresh handler thread pinned as
specified. Then 25 rounds of: `reset()`, then time each of 4,096 touches individually.

Timing each touch rather than the whole walk is what produces a distribution instead of a mean, and
in §3.2 it is what makes the bimodality visible - which is the finding.

`--reverse` reverses the configuration list. A control, not a convenience: if configuration order
mattered, a reversed run would disagree with a forward one *systematically* rather than randomly.
Run 2 of the committed results is a reversed run, and it agreed.

The baseline configuration (`handler_cpu: None`) uses no userfaultfd at all - the kernel's own
anonymous fault. Everything else in the table is measured against it, and without it the numbers
would have no scale.

---

## Part 7 - `toy-uffd-crates/src/main.rs`

The same pager on the `userfaultfd` crate.

**What the crate provides:** `UffdBuilder` (flags plus the handshake, with `require_features` making
feature negotiation explicit); `Event::Pagefault { rw, addr, .. }` instead of a union read at the
right offset, with `rw` a two-variant enum rather than a bit in a flags word; and `Result` semantics
over the `copy` out-parameter, including a distinct `Error::PartiallyCopied` that the raw version
here does not model.

**What it does not provide:** which page belongs at which address, where the image comes from, how
many pages to install per fault, which thread the handler runs on, and what to do when the guest
asks for a page that is not in the image. That is the whole body of the file, and it is where the
engineering is.

One asymmetry worth carrying: the crate's `copy` takes `(src, dst, len, wake)` while the kernel
struct orders the fields `dst, src`. Both are defensible and the mismatch is exactly the kind of
thing that produces a working-but-backwards pager - which is why the raw version keeps the kernel's
order.

`open_uffd()` is the workaround for §4 of the README: on `EACCES` from `/dev/userfaultfd`, create and
handshake the fd with the raw crate's code and hand it over via `Uffd::from_raw_fd`. That is the
shape a fallback inside the crate would take.

---

## Part 8 - `tests/restore.rs`

Six tests, each skipping cleanly when `userfaultfd` is unavailable.

- `every_page_restores_its_own_content` - scattered order, content and fault count and read/write
  classification.
- `a_page_faults_once_no_matter_how_often_it_is_touched` - 1,000 touches, 1 fault.
- `batching_reduces_the_fault_count_exactly` - `PAGES / batch` faults for each batch size. This is
  the assertion that would catch the handler installing the wrong run, *before* a wrong `ns/page`
  reached a results file.
- `reset_re_arms_the_fault` - five rounds, fault count growing exactly.
- `a_write_fault_is_reported_as_a_write` - the flag decoding, and that the guest's write survives.
- `closing_the_uffd_resolves_faults_with_zero_pages_instead_of_hanging` - the most important one.
  Register a region, never start a handler, drop the uffd, then touch. It does not hang. It returns
  zeros in a few hundred nanoseconds. README §5 is about why that is the worst possible failure mode
  for a VMM.
