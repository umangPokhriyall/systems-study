//! Demand paging with `userfaultfd`, from the raw syscall up.
//!
//! Rung 1 established that guest memory is an ordinary host mapping. Rung 2 used that fact to move
//! data without copies. This rung uses it to *not have the data yet*: a region can be handed to a
//! guest before it has been read off disk, and the pages fetched as the guest asks for them.
//!
//! That is what makes a microVM snapshot restore in single-digit milliseconds instead of the time
//! it takes to read its whole memory image. It is also the mechanism this study's flagship is built
//! on, and the one Cloud Hypervisor shipped in v52/v53 and does not measure.
//!
//! - [`uffd_sys`] - the raw ABI: the syscall, the ioctl numbers, the structures.
//! - [`uffd`] - an owned userfaultfd and the region it watches.
//! - [`topology`] - CPU topology and pinning, for the placement experiment.

pub mod topology;
pub mod uffd;
pub mod uffd_sys;
