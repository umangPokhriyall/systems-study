# Repository structure

This document fixes the structure of `systems-study` for all rungs, so that later rungs extend a
shape that already exists rather than inventing one each time. It is written before rung 1 and is
expected to survive unchanged; if a later rung forces a change, the change and its reason are
recorded in the changelog at the bottom.

## Design constraints

Four constraints shaped this, in priority order.

1. **A virtualization engineer must be able to audit any single rung without reading the others.**
   That forces prose, code, results and the comprehension gate to live *together* in the rung
   directory, rather than being split into a global `docs/`, a global `src/` and a global
   `results/`. The cost is some duplication of context across rungs; the benefit is that a reviewer
   who lands on `rung-03-uffd/` from a search engine has everything they need.

2. **Committed artifacts must be reproducible or clearly labelled as machine-specific.** Anything
   derived from the machine it ran on (a results CSV, a flame graph) is committed *only* alongside
   the environment manifest that describes the machine. A number with no manifest is not evidence,
   so it does not get committed.

3. **Nothing may be committed that I cannot explain.** This rules out vendored dependency trees,
   generated bindings checked in as source, and large binary blobs.

4. **Work must be liftable upstream.** Anything that could plausibly become a patch to Cloud
   Hypervisor, Firecracker or a rust-vmm crate is written in that project's idiom from the start:
   Apache-2.0, `#[derive]`-light `#[repr(C)]` structs, `unsafe` blocks with a `SAFETY:` comment,
   no `unwrap()` outside tests and examples.

## The tree

```
systems-study/
├── README.md                     Front door: the ladder, status, how to read.
├── LICENSE-APACHE                Apache-2.0.
├── .gitignore
├── Cargo.toml                    Workspace root; every rung crate is a member.
├── rust-toolchain.toml           Pinned toolchain, so a results CSV is attributable to a compiler.
│
├── docs/
│   ├── LADDER.md                 Why the rungs are in this order and what each one proves.
│   ├── REPO-STRUCTURE.md         This file.
│   ├── METHODOLOGY.md            The measurement standard every committed number is held to.
│   ├── GLOSSARY.md               Terms, defined once, linked from the rung READMEs.
│   └── OPEN-QUESTIONS.md         Running list of things the code did not explain to me.
│
├── rung-NN-<subsystem>/
│   ├── README.md                 Objectives, background from first principles, architecture,
│   │                             execution flow, memory layout, kernel concepts, relation to
│   │                             CH/FC/rust-vmm, references, and the results section.
│   ├── CODE_WALKTHROUGH.md       The code in execution order; every ioctl and syscall explained.
│   ├── EXERCISES.md              Modifications to implement myself, easy to hard.
│   ├── GATE.md                   The comprehension gate: reasoning questions, and the date passed.
│   ├── COMMON-MISTAKES.md        Misconceptions, each with why it is wrong and how it shows up.
│   ├── <crate-name>/             One or more Cargo crates. Raw-syscall version first, then the
│   │                             ecosystem-crate version where one exists.
│   └── results/
│       ├── env-<host>-<date>.txt Environment manifest (see METHODOLOGY.md).
│       └── *.csv                 Raw measurements, one row per sample. Never summarised in place.
│
├── tools/
│   ├── capture-env.sh            Emits the environment manifest.
│   └── summarise.py              Percentiles over a results CSV. Reads raw, prints summary;
│                                 never mutates the raw file.
│
└── target/                       gitignored.
```

## Per-artifact policy

For each artifact: where it lives, whether it is committed, how later rungs consume it, and what it
contributes toward upstream work.

### Prose

| Artifact | Location | Committed? | Used by later rungs | Contribution to upstream work |
|---|---|---|---|---|
| Rung `README.md` | `rung-NN-*/README.md` | **Yes** | Later rungs link back to it instead of re-explaining a subsystem. Rung 4's subsystem maps cite the rung that taught the concept. | This is the register upstream review happens in: claim, mechanism, evidence, limitation. Writing five of these is practice for writing a PR description that a maintainer accepts without a round trip. |
| `CODE_WALKTHROUGH.md` | `rung-NN-*/` | **Yes** | Rung 4 reuses the walkthrough *format* for reading production code: same headings, execution order first. | Directly transferable. The hardest part of reviewing a VMM patch is knowing the surrounding execution order; a habit of writing it down is what makes a first review comment useful rather than cosmetic. |
| `GATE.md` | `rung-NN-*/` | **Yes**, including the date passed and any question I initially failed | A failed gate question becomes an entry in `docs/OPEN-QUESTIONS.md` and, if still unresolved, a study item in a later rung. | Honest failure records are the credibility mechanism. A repository that only shows successes is indistinguishable from one where the author copied the answers. |
| `COMMON-MISTAKES.md` | `rung-NN-*/` | **Yes** | Later rungs append to it when the same misconception reappears at a higher level (e.g. "an exit is free" reappears as "a notification is free" in rung 2). | These are the review comments I will one day leave on other people's patches. Writing them down first makes them precise. |
| `EXERCISES.md` | `rung-NN-*/` | **Yes** | Some exercises are *promoted* to the next rung when they turn out to be load-bearing. Each exercise records whether I completed it. | Several exercises are deliberately shaped like real upstream tasks (add a backend, add a metric, handle an error path). One of them becomes the first PR. |
| `docs/OPEN-QUESTIONS.md` | `docs/` | **Yes** | Every rung appends. Rung 4 reads production code specifically to close entries. | This is the pipeline for good mailing-list and issue-tracker questions. A well-formed question with the code path already traced is a contribution in itself, and it is the lowest-risk way to become a known name in a project. |

### Code

| Artifact | Location | Committed? | Used by later rungs | Contribution to upstream work |
|---|---|---|---|---|
| Raw-syscall crate (`toy-*-raw`) | `rung-NN-*/<crate>/` | **Yes** | The reference for "what the ecosystem crate is actually doing". Rung 2's virtqueue work and rung 3's `userfaultfd` work both assume the reader has seen the raw layer once. | This is the knowledge that separates a patch that compiles from a patch that is correct. `vm-memory` and `kvm-ioctls` review turns on questions like "is this pointer still valid after the guest may have remapped" - unanswerable without having written the raw layer. |
| Ecosystem-crate crate (`toy-*-crates`) | `rung-NN-*/<crate>/` | **Yes** | Establishes the API vocabulary (`VmFd`, `GuestMemoryMmap`, `DescriptorChain`) that rung 4 will meet in production code. | The diff between the raw and the crate version *is* the value the rust-vmm crates add. Knowing that diff precisely is what lets me argue for or against an API change upstream, rather than only consuming the API. |
| `tools/*.sh`, `tools/*.py` | `tools/` | **Yes** | Shared by every rung that measures. | Cloud Hypervisor's `performance-metrics` and Firecracker's `tools/ab_test.py` are the upstream equivalents. Arriving with my own working version of the same idea is what makes a methodology argument credible instead of theoretical. |
| `target/`, `Cargo.lock` for libraries | - | **`target/` ignored. `Cargo.lock` committed**, because every crate here is a binary and reproducibility of a measurement outranks dependency freshness. | - | A committed lockfile is what makes a results CSV attributable to an exact dependency tree. |

### Measurements

| Artifact | Location | Committed? | Used by later rungs | Contribution to upstream work |
|---|---|---|---|---|
| Raw sample CSV | `rung-NN-*/results/*.csv` | **Yes**, raw, one row per sample, never pre-aggregated | Rung 3's `userfaultfd` fault cost is only interpretable against rung 1's vmexit cost from the same machine. Cross-rung comparison is the point of keeping them raw and in the same format. | Both target projects report means. Committing full sample distributions, from a repository whose stated method is "distributions not averages", is the concrete demonstration behind the measurement-capability contribution I intend to make. |
| Environment manifest | `rung-NN-*/results/env-<host>-<date>.txt` | **Yes**, one per results-producing session | Every later comparison checks the manifest first; a comparison across two different manifests is labelled as such or not made. | The single most common reason a performance claim is rejected upstream is that the environment is unstated. This makes it impossible to forget. |
| Flame graphs, `perf` output | `rung-NN-*/results/` | **SVG committed only when it is referenced from prose.** `perf.data` is **gitignored** - it is large, host-specific and unreadable without the exact binary. | The folded stack file, which is small and diffable, is committed instead when a rung needs one. | Differential flame graphs are the standard evidence format for an optimization patch. Producing them habitually is the difference between "it got faster" and a reviewable claim. |
| Anything binary and large | - | **Gitignored.** Regeneration instructions go in the rung README instead. | - | A repository a maintainer can clone in seconds is one they will actually look at. |

On size: raw sample files are large on disk (rung 1's three runs are 13 MB) and small in git (the
whole history is 2.2 MB), because columns of similar integers compress well. The policy stays "commit
the raw samples" until a clone stops being cheap. The review point at which that changes is a rung
whose results do not compress - a folded stack file or an SVG - and the answer there is to commit
one referenced artefact rather than a series.

### Deliberately absent

- **No `benches/` using `criterion`.** Criterion's model is repeated short measurements of a
  deterministic function; the things measured here are latencies with long tails, where the mean
  criterion optimises for is the statistic that hides the finding. Measurement code is written
  explicitly, per rung, and the raw samples are committed.
- **No CI badge, no coverage badge.** They would be measuring a repository of toy programs, where
  the number would be true and meaningless.
- **No vendored dependencies.** See constraint 3.

## Naming conventions

- Rung directories: `rung-NN-<shortest-accurate-subsystem-name>`, zero-padded, so they sort.
- Crates: `toy-<subsystem>-raw` and `toy-<subsystem>-crates`. The `toy-` prefix is a promise about
  scope, and it is honoured: a toy here is small enough to read in one sitting.
- Results files: `<what>-<host>-<YYYY-MM-DD>.csv`. Host and date in the filename, because a results
  file will eventually be looked at out of context.
- Environment manifests: `env-<host>-<YYYY-MM-DD>.txt`, one per session, referenced by name from
  every results file produced in that session.

## Changelog

| Date | Change | Reason |
|---|---|---|
| 2026-08-05 | Initial structure, written before rung 1. | - |
