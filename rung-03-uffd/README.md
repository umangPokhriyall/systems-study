# Rung 3 - userfaultfd

**Not started.** Placeholder, so the repository structure is visible before the content exists.

## What will land here

A fault-cost microbenchmark: register a `userfaultfd` over an anonymous mapping, service
`UFFD_EVENT_PAGEFAULT` from a handler thread with `UFFDIO_COPY`, and measure the per-fault cost as a
distribution. Then measure it again with the handler thread on a different physical core from the
faulting thread, and report the delta.

## Why it comes after rungs 1 and 2

It depends on understanding guest memory as an ordinary host mapping, which rung 1 establishes
concretely through `KVM_SET_USER_MEMORY_REGION`, and which rung 2 reinforces by having host and
guest share it.

## Why it matters beyond the ladder

This is the mechanism behind Cloud Hypervisor's demand-paged restore and Firecracker's UFFD snapshot
support. It is the rung with the shortest distance to an actual upstream contribution: both projects
have the feature, and the fault tail it trades away is not reported by either project's performance
suite.

Note the constraint discovered in rung 1: guest memory shared with a `userfaultfd` handler in a
separate process must be `MAP_SHARED`, and that choice is made at allocation time.
