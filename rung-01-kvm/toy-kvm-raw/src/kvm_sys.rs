//! The raw KVM ABI: ioctl request numbers, and the C structures they carry.
//!
//! Nothing in this file is imported from a crate. The point of the exercise is that the KVM ABI is
//! a small, stable, *readable* interface, and that the `kvm-ioctls` crate is a thin safe wrapper
//! over exactly what is written here. Everything below corresponds to `include/uapi/linux/kvm.h`
//! in the kernel tree.
//!
//! The one invariant that matters: **these structure layouts are ABI**. The kernel reads and writes
//! them by offset. A reordered field is not a compile error, it is a silently wrong VM. That is why
//! every struct is `#[repr(C)]` and why the sizes are asserted at compile time at the bottom of the
//! file - a size mismatch means a field is missing or padded differently, and it would otherwise
//! show up as a mysterious `EINVAL` or, worse, as garbage in a register.

#![allow(non_camel_case_types)]

// ---------------------------------------------------------------------------------------------
// ioctl request-number encoding
// ---------------------------------------------------------------------------------------------
//
// Linux packs four things into the 32-bit ioctl request number:
//
//   bits 31..30  direction of the data transfer, from userspace's point of view
//   bits 29..16  size in bytes of the structure being passed
//   bits 15..8   a per-subsystem "type" byte, to keep numbers from colliding across drivers
//   bits  7..0   the command number within that subsystem
//
//    31 30 29                16 15             8 7              0
//   +-----+--------------------+----------------+---------------+
//   | dir |        size        |      type      |      nr       |
//   +-----+--------------------+----------------+---------------+
//
// Encoding these by hand rather than pasting hex constants is deliberate: it makes the size field
// visible, and the size field is where a mismatched struct definition gets caught. If `kvm_sregs`
// were defined with a wrong layout here, `KVM_GET_SREGS` would encode a different size, and the
// kernel would reject it with ENOTTY rather than corrupting memory.

const IOC_NRBITS: u32 = 8;
const IOC_TYPEBITS: u32 = 8;
const IOC_SIZEBITS: u32 = 14;

const IOC_NRSHIFT: u32 = 0;
const IOC_TYPESHIFT: u32 = IOC_NRSHIFT + IOC_NRBITS; // 8
const IOC_SIZESHIFT: u32 = IOC_TYPESHIFT + IOC_TYPEBITS; // 16
const IOC_DIRSHIFT: u32 = IOC_SIZESHIFT + IOC_SIZEBITS; // 30

const IOC_NONE: u32 = 0;
const IOC_WRITE: u32 = 1; // userspace writes, i.e. the kernel reads the struct
const IOC_READ: u32 = 2; // userspace reads, i.e. the kernel fills the struct

const KVMIO: u32 = 0xAE;

const fn ioc(dir: u32, ty: u32, nr: u32, size: u32) -> u64 {
    ((dir << IOC_DIRSHIFT) | (size << IOC_SIZESHIFT) | (ty << IOC_TYPESHIFT) | (nr << IOC_NRSHIFT))
        as u64
}

/// No payload; the third `ioctl` argument is a plain integer, not a pointer.
const fn io(nr: u32) -> u64 {
    ioc(IOC_NONE, KVMIO, nr, 0)
}
/// Userspace passes a struct in; the kernel reads it.
const fn iow(nr: u32, size: usize) -> u64 {
    ioc(IOC_WRITE, KVMIO, nr, size as u32)
}
/// Userspace passes a buffer in; the kernel fills it.
const fn ior(nr: u32, size: usize) -> u64 {
    ioc(IOC_READ, KVMIO, nr, size as u32)
}

// ---------------------------------------------------------------------------------------------
// The ioctls, in the order this VMM issues them
// ---------------------------------------------------------------------------------------------

/// Issued on the `/dev/kvm` fd. Must return exactly 12; KVM froze this number in 2007 and uses
/// per-capability queries (`KVM_CHECK_EXTENSION`) for everything since. Checking it is the cheapest
/// possible sanity check that the fd really is KVM.
pub const KVM_GET_API_VERSION: u64 = io(0x00);

/// `/dev/kvm` fd -> a new **VM fd**. The third argument is the "machine type", 0 for the default.
/// The returned fd owns an address space: memory slots and, later, vCPUs hang off it.
pub const KVM_CREATE_VM: u64 = io(0x01);

/// Query whether an optional feature exists. Used here only for `KVM_CAP_USER_MEMORY`, without
/// which the whole memory model below is unavailable.
pub const KVM_CHECK_EXTENSION: u64 = io(0x03);

/// Size of the shared `kvm_run` communication page, in bytes. Issued on the `/dev/kvm` fd, *not*
/// the vCPU fd, because it is a property of the kernel build rather than of any particular vCPU.
pub const KVM_GET_VCPU_MMAP_SIZE: u64 = io(0x04);

/// VM fd. Installs one region of host memory as guest physical memory. See
/// [`kvm_userspace_memory_region`].
pub const KVM_SET_USER_MEMORY_REGION: u64 = iow(0x46, size_of::<kvm_userspace_memory_region>());

/// VM fd. Tells KVM where in *guest physical* space it may place three pages of scratch used to
/// emulate a task-state segment. This exists for older Intel parts that cannot run a guest in real
/// mode natively: VMX originally required the guest to be in protected mode with paging, so KVM
/// emulated real mode by running the guest inside a hidden protected-mode task, which needs a TSS
/// somewhere the guest is not using. Modern parts have "unrestricted guest" and do not need it, but
/// setting it costs nothing and makes the example work on a 2010 laptop.
pub const KVM_SET_TSS_ADDR: u64 = io(0x47);

/// VM fd -> a new **vCPU fd**. The third argument is the vCPU id. A vCPU fd is thread-affine in
/// practice: `KVM_RUN` must be called from the thread that owns it, because KVM stashes per-thread
/// FPU and register state around the entry.
pub const KVM_CREATE_VCPU: u64 = io(0x41);

/// vCPU fd. Enter guest mode. Returns when the guest exits for a reason userspace must handle.
/// This is the single most important call in any VMM; everything else is setup for it.
pub const KVM_RUN: u64 = io(0x80);

pub const KVM_GET_REGS: u64 = ior(0x81, size_of::<kvm_regs>());
pub const KVM_SET_REGS: u64 = iow(0x82, size_of::<kvm_regs>());
pub const KVM_GET_SREGS: u64 = ior(0x83, size_of::<kvm_sregs>());
pub const KVM_SET_SREGS: u64 = iow(0x84, size_of::<kvm_sregs>());

/// Capability id for user-allocated memory slots. Every VMM written this century requires it.
pub const KVM_CAP_USER_MEMORY: u64 = 3;

// ---------------------------------------------------------------------------------------------
// Exit reasons
// ---------------------------------------------------------------------------------------------
//
// Only the ones this VMM can actually encounter are named. The rest are reported numerically,
// which is the honest thing to do: an unexpected exit reason is a bug in the VMM, not something to
// swallow.

pub const KVM_EXIT_UNKNOWN: u32 = 0;
pub const KVM_EXIT_IO: u32 = 2;
pub const KVM_EXIT_HLT: u32 = 5;
pub const KVM_EXIT_MMIO: u32 = 6;
pub const KVM_EXIT_SHUTDOWN: u32 = 8;
pub const KVM_EXIT_FAIL_ENTRY: u32 = 9;
pub const KVM_EXIT_INTR: u32 = 10;
pub const KVM_EXIT_INTERNAL_ERROR: u32 = 17;

pub const KVM_EXIT_IO_IN: u8 = 0;
pub const KVM_EXIT_IO_OUT: u8 = 1;

pub fn exit_reason_name(r: u32) -> &'static str {
    match r {
        KVM_EXIT_UNKNOWN => "KVM_EXIT_UNKNOWN",
        KVM_EXIT_IO => "KVM_EXIT_IO",
        KVM_EXIT_HLT => "KVM_EXIT_HLT",
        KVM_EXIT_MMIO => "KVM_EXIT_MMIO",
        KVM_EXIT_SHUTDOWN => "KVM_EXIT_SHUTDOWN",
        KVM_EXIT_FAIL_ENTRY => "KVM_EXIT_FAIL_ENTRY",
        KVM_EXIT_INTR => "KVM_EXIT_INTR",
        KVM_EXIT_INTERNAL_ERROR => "KVM_EXIT_INTERNAL_ERROR",
        _ => "unhandled exit reason",
    }
}

// ---------------------------------------------------------------------------------------------
// Structures
// ---------------------------------------------------------------------------------------------

/// One guest-physical memory region, backed by a host userspace mapping.
///
/// The mental model that matters: KVM does **not** allocate guest memory. Userspace allocates it
/// (here, with `mmap`), and this call tells KVM "the host virtual range starting at
/// `userspace_addr` *is* the guest physical range starting at `guest_phys_addr`". KVM then programs
/// the hardware second-level page tables (EPT on Intel, NPT on AMD) so the guest's own page tables
/// translate into that host mapping without a further exit.
///
/// The consequence, which is the whole reason virtio works: **the host can read and write guest
/// memory by dereferencing its own pointer, with no ioctl and no copy**, because it is the same
/// physical memory. Every virtqueue implementation in every VMM is built on that fact.
///
/// Guest-physical space is sparse. Anything the guest touches that is *not* covered by a region
/// exits to userspace as an MMIO access, which is exactly how device emulation is triggered - a
/// device is a hole in the memory map plus a handler in the VMM.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_userspace_memory_region {
    /// Slot index. Slots are a fixed-size array in the kernel; reusing an index replaces the
    /// region, and setting `memory_size` to 0 deletes it.
    pub slot: u32,
    /// `KVM_MEM_LOG_DIRTY_PAGES` and `KVM_MEM_READONLY` live here. Dirty logging is what live
    /// migration and snapshotting are built on; unused in this toy.
    pub flags: u32,
    /// Where this appears in guest physical address space. Must be page-aligned.
    pub guest_phys_addr: u64,
    /// Size in bytes, a multiple of the page size.
    pub memory_size: u64,
    /// Host virtual address of the backing mapping. Must be page-aligned, and must stay mapped for
    /// as long as the region exists - unmapping it under a running guest is a use-after-free that
    /// the hardware, not the compiler, gets to discover.
    pub userspace_addr: u64,
}

/// One x86 segment register, *including its hidden descriptor cache*.
///
/// This is the structure that makes real mode comprehensible. On real hardware, loading a segment
/// selector causes the CPU to fetch a descriptor and cache base/limit/permissions in registers
/// software cannot address. In real mode there is no descriptor table, so the CPU synthesises
/// `base = selector << 4`. KVM exposes the cache directly, which means a VMM can put a vCPU into a
/// state real hardware could only reach by a specific sequence of loads - for example, base 0 with
/// selector 0, which is what this VMM does so that guest offsets equal guest physical addresses.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_segment {
    pub base: u64,
    pub limit: u32,
    pub selector: u16,
    pub type_: u8,
    pub present: u8,
    pub dpl: u8,
    pub db: u8,
    pub s: u8,
    pub l: u8,
    pub g: u8,
    pub avl: u8,
    /// Set to mark a segment as unusable; the hardware then faults on any access through it.
    pub unusable: u8,
    pub padding: u8,
}

/// A descriptor-table register (GDTR / IDTR): base and limit, no selector.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_dtable {
    pub base: u64,
    pub limit: u16,
    pub padding: [u16; 3],
}

/// The "special" registers: segmentation, control registers, and the pending-interrupt bitmap.
/// Field order is ABI. Do not sort it.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_sregs {
    pub cs: kvm_segment,
    pub ds: kvm_segment,
    pub es: kvm_segment,
    pub fs: kvm_segment,
    pub gs: kvm_segment,
    pub ss: kvm_segment,
    pub tr: kvm_segment,
    pub ldt: kvm_segment,
    pub gdt: kvm_dtable,
    pub idt: kvm_dtable,
    pub cr0: u64,
    pub cr2: u64,
    pub cr3: u64,
    pub cr4: u64,
    pub cr8: u64,
    pub efer: u64,
    pub apic_base: u64,
    /// One bit per interrupt vector, 256 vectors. Used to inject interrupts that were pending when
    /// the vCPU state was captured; central to snapshot/restore, unused here.
    pub interrupt_bitmap: [u64; 4],
}

/// The general-purpose registers plus `rip` and `rflags`. Field order is ABI.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_regs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rsp: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

/// The header of the shared `kvm_run` page.
///
/// `kvm_run` is not passed to an ioctl. It is a page of memory `mmap`'d from the **vCPU fd**, and
/// it is the only zero-syscall communication channel between KVM and the VMM. After `KVM_RUN`
/// returns, the VMM reads the reason and the operands out of this page directly; when the exit
/// requires a value to be handed back to the guest (an I/O or MMIO *read*), the VMM writes it into
/// this page and the next `KVM_RUN` picks it up.
///
/// Only the fixed header is declared here. At byte offset 32 a large `union` begins, one variant
/// per exit reason. Rust unions of non-`Copy` types are awkward and hide the layout, so instead the
/// variants are declared as separate `#[repr(C)]` structs and reached by pointer arithmetic from
/// the end of this header - which is precisely what the C union is doing anyway, with the offset
/// made explicit rather than implicit. [`KVM_RUN_UNION_OFFSET`] is asserted below.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct kvm_run_header {
    /// In: ask KVM to exit as soon as the guest can accept an interrupt.
    pub request_interrupt_window: u8,
    /// In: set from a signal handler or another thread to force an immediate exit.
    pub immediate_exit: u8,
    pub padding1: [u8; 6],
    /// Out: why `KVM_RUN` returned. The dispatch key for the whole VMM.
    pub exit_reason: u32,
    pub ready_for_interrupt_injection: u8,
    pub if_flag: u8,
    pub flags: u16,
    pub cr8: u64,
    pub apic_base: u64,
    // The union starts here, at offset 32.
}

/// Byte offset of the exit-information union inside the `kvm_run` page.
pub const KVM_RUN_UNION_OFFSET: usize = 32;

/// `KVM_EXIT_IO` variant of the union.
///
/// Note what is *not* here: the data. Port I/O can be a string operation (`ins`/`outs`) moving
/// `count` items at once, so KVM places the payload elsewhere in the shared page and reports
/// `data_offset`, a byte offset **from the start of the `kvm_run` mapping**, not from this struct.
/// Getting that base wrong is the classic first bug in a hand-written VMM.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct kvm_run_io {
    /// [`KVM_EXIT_IO_IN`] or [`KVM_EXIT_IO_OUT`], from the *guest's* point of view.
    pub direction: u8,
    /// Bytes per item: 1, 2 or 4.
    pub size: u8,
    pub port: u16,
    /// Number of items. Greater than 1 only for string I/O instructions.
    pub count: u32,
    /// Offset of the payload from the start of the `kvm_run` mapping.
    pub data_offset: u64,
}

/// `KVM_EXIT_MMIO` variant of the union.
///
/// Unlike port I/O, the payload is inline: MMIO accesses are at most 8 bytes, so KVM can afford to
/// carry them in the union itself. On a write, `data[..len]` is what the guest stored. On a read,
/// the VMM **fills** `data[..len]` and the value is placed into the guest's destination register
/// when the vCPU is resumed - the guest never learns that its load took a detour through userspace.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct kvm_run_mmio {
    pub phys_addr: u64,
    pub data: [u8; 8],
    pub len: u32,
    pub is_write: u8,
}

// ---------------------------------------------------------------------------------------------
// Compile-time ABI checks
// ---------------------------------------------------------------------------------------------
//
// A wrong size here is the difference between a working VMM and one that fails with EINVAL or
// silently reads the wrong field. These are cheap; a mismatch is caught at build time on the
// machine whose kernel headers actually matter.

const _: () = {
    assert!(size_of::<kvm_segment>() == 24);
    assert!(size_of::<kvm_dtable>() == 16);
    assert!(size_of::<kvm_sregs>() == 312);
    assert!(size_of::<kvm_regs>() == 144);
    assert!(size_of::<kvm_userspace_memory_region>() == 32);
    // The header must be exactly the offset of the union, or every exit-info read is skewed.
    assert!(size_of::<kvm_run_header>() == KVM_RUN_UNION_OFFSET);
};
