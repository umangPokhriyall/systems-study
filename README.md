# systems-study

An engineering notebook for learning virtualization internals from first principles, in Rust,
on Linux/KVM - built as preparation for upstream contributions to
[Cloud Hypervisor](https://github.com/cloud-hypervisor/cloud-hypervisor),
[Firecracker](https://github.com/firecracker-microvm/firecracker) and
[rust-vmm](https://github.com/rust-vmm).

This is not a tutorial series and it is not a collection of exercises copied from a book. It is a
record of a deliberate, ordered study programme, where every stage ends in something a reader can
run, check, or disagree with.

## The rule this repository is built on

> Generation must never outrun comprehension.

Every directory here contains code I can explain line by line, including what the kernel does in
response to each ioctl. Where I do not understand something, it is written down as an open question
in [`docs/OPEN-QUESTIONS.md`](docs/OPEN-QUESTIONS.md) rather than papered over. Where a measurement
is uncertain, the uncertainty is reported next to the number.

## The ladder

Each rung closes a specific, named gap in my systems knowledge. A rung is not finished when the code
runs; it is finished when the comprehension gate is passed and the artifact is committed.

| Rung | Subsystem | Gap it closes | Artifact | Status |
|---|---|---|---|---|
| [1](rung-01-kvm/) | KVM: `/dev/kvm`, vCPU run loop, VM exits | No KVM experience at all | Two toy VMMs (raw ioctls, then `kvm-ioctls`) + a vmexit cost distribution | code and measurement done; **gate not yet taken** |
| [2](rung-02-virtio/) | virtio: virtqueues, descriptor chains, `EVENT_IDX` | No virtqueue experience | A split virtqueue by hand (both halves), the same device on `virtio-queue`, walk cost + suppression counts, and a bug found upstream | code and measurement done; **gate not yet taken** |
| [3](rung-03-uffd/) | `userfaultfd`: demand paging, fault servicing | No `userfaultfd`, no VM-scale memory management | A demand pager on the raw syscall, the same on the `userfaultfd` crate, fault cost by handler placement and prefault batch size | code and measurement done; **gate not yet taken** |
| [4](rung-04-subsystem-maps/) | Reading real VMM code with a purpose | Never read a production VMM | One written subsystem map per target area | not started |
| [5](rung-05-review-log/) | Upstream review culture | No upstream contribution history | A log of merged PRs read, and what the maintainer objected to | not started |

The full rationale for this ordering - why KVM before virtio, why virtio before `userfaultfd`, and
why none of it may be skipped - is in [`docs/LADDER.md`](docs/LADDER.md).

## How to read this repository

If you have limited time and want to judge whether the work is real:

1. Read [`rung-01-kvm/README.md`](rung-01-kvm/README.md) - it explains what a VM exit actually is,
   from the hardware upward.
2. Skim [`rung-01-kvm/toy-kvm-raw/src/vmm.rs`](rung-01-kvm/toy-kvm-raw/src/vmm.rs) - roughly 300
   lines that boot a real-mode guest with nothing but `libc` and hand-encoded ioctl numbers.
3. Read the results section of [`rung-01-kvm/README.md`](rung-01-kvm/README.md#results) - the cost
   of a VM exit on this machine, reported as a distribution rather than a mean.
4. Read [`rung-02-virtio/README.md` §4](rung-02-virtio/README.md#4-what-was-found-on-the-way-a-bug-in-virtio-queues-mock-framework) -
   a layout bug in `virtio-queue`'s test framework, found by making two implementations disagree,
   with a reproducer and a fix.
5. Read [`rung-03-uffd/README.md` §3.1](rung-03-uffd/README.md#31-handler-placement---and-a-wrong-conclusion-then-the-right-one) -
   a measurement that gave a confident wrong answer, the control that refuted it, and what the
   defensible statement turned out to be.

If you are here to check the reasoning rather than the code, read
[`docs/METHODOLOGY.md`](docs/METHODOLOGY.md), which is the measurement standard every number in this
repository is held to, and [`docs/OPEN-QUESTIONS.md`](docs/OPEN-QUESTIONS.md), which is the honest
part.

## Layout

```
systems-study/
├── docs/                    Cross-cutting documents: the ladder, the measurement standard,
│                            the glossary, and the running list of unanswered questions.
├── rung-NN-<subsystem>/     One directory per rung. Self-contained: prose, code, exercises,
│                            comprehension gate, and results all live together.
├── tools/                   Small shared scripts used by more than one rung
│                            (environment capture, statistics over a results CSV).
└── Cargo.toml               Workspace root. Every rung's crates are members, so
                             `cargo test --workspace` checks the entire repository.
```

The full structure, including which files are committed and which are deliberately ignored and why,
is documented in [`docs/REPO-STRUCTURE.md`](docs/REPO-STRUCTURE.md).

## Building

```
cargo build --workspace
cargo test  --workspace
```

Rung 1 needs access to `/dev/kvm` on an x86-64 Linux host with hardware virtualization enabled. The
tests that require it skip themselves cleanly (rather than fail) when it is unavailable, so the
workspace still builds and tests on a machine without KVM.

## License

Apache-2.0, matching Cloud Hypervisor, Firecracker and rust-vmm, so that anything developed here can
be carried upstream without a licensing conversation.
