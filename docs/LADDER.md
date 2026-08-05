# The ladder

Five rungs, in a fixed order. This document explains the ordering, because the ordering is the only
part of a self-study plan that is hard to get right - the reading list is easy.

## The principle

Each rung closes one gap that is **load-bearing for the rungs above it**. A rung is skippable only
if nothing above it depends on it, and in this ladder nothing is skippable.

The failure mode this is built to avoid is the common one: reading about virtqueues before having
seen a VM exit, which produces someone who can recite the descriptor-table layout and cannot say
what happens on the host when the guest writes to the notification register. The ordering below is
chosen so that every abstraction is met *after* the mechanism underneath it.

## Rung 1 - KVM

**Closes:** no KVM experience of any kind. No `/dev/kvm` ioctl code, no vCPU run loop, no exit
handling.

**Why first:** every device model, every memory manager and every snapshot mechanism in every VMM is
ultimately a reaction to a VM exit. Until the exit loop is concrete, the rest is vocabulary. It is
also the shortest path to a running guest, which matters for motivation.

**What the rung above needs from it:** rung 2 cannot explain what a virtqueue notification *costs*
without rung 1's exit-cost measurement, and cannot explain what the host is doing when it services
one without rung 1's run loop.

## Rung 2 - virtio

**Closes:** no virtqueue experience. Lock-free ring buffers, yes; virtqueues, no - no
descriptor-chain walk, no available/used ring, no feature negotiation.

**Why second:** virtio is the interface across which essentially all microVM I/O flows, and it sits
directly on top of rung 1's exit mechanism plus shared guest memory. It also introduces the first
genuinely adversarial surface: descriptor chains are guest-controlled data structures that the host
must walk without trusting them.

**Why not first:** the notification suppression mechanism (`EVENT_IDX`) exists entirely to avoid VM
exits. Meeting it before understanding what an exit costs teaches the mechanism and hides the
motive.

## Rung 3 - `userfaultfd`

**Closes:** no `userfaultfd`, no VM-scale memory management.

**Why third:** demand-paged restore is where microVM snapshot latency actually lives, and it depends
on understanding guest memory as a host mapping - which rung 1 establishes concretely by way of
`KVM_SET_USER_MEMORY_REGION`, and rung 2 reinforces by having the host and guest share it.

**Why it matters beyond the ladder:** this is the mechanism behind Cloud Hypervisor's demand-paged
restore and Firecracker's UFFD snapshot support. It is the rung with the shortest distance to an
actual contribution.

## Rung 4 - reading production code

**Closes:** never having read a production VMM.

**Why fourth and not first:** reading a 1,500-line virtqueue implementation before rung 2 produces
notes; reading it after rung 2 produces questions. The difference is the entire value of the
exercise. Each target subsystem is read with a specific contribution in view, and each produces a
written map - data flow, hot path, where a lock or a copy lives, and the one question the code did
not answer.

## Rung 5 - review culture

**Closes:** no upstream contribution history, and no feel for what a maintainer in these projects
actually objects to.

**Why it runs continuously rather than in sequence:** it costs fifteen minutes a day and it is the
only rung whose subject matter is other people. It starts as soon as rung 1 is done and never stops.

## What "finished" means

A rung is finished when all four of these are true:

1. The artifact is committed and runs.
2. The comprehension gate in `GATE.md` is passed **from memory**, without re-reading the code, and
   the date is recorded.
3. Every question the rung raised is either answered in the README or written into
   `docs/OPEN-QUESTIONS.md`.
4. At least the first three exercises are done, and the ones not done are marked as not done.

Running code is the weakest of the four conditions and on its own means nothing.
