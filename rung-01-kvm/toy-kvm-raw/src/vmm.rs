//! The VMM itself: create a VM, give it memory, give it a vCPU, run it, handle its exits.
//!
//! Read this file top to bottom and you have read a virtual machine monitor. Everything a
//! production VMM adds - many vCPUs, a real device model, a bootloader, snapshots, an API server -
//! is built on the ten ioctls below and does not change their meaning.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Instant;

use crate::device::Devices;
use crate::kvm_sys::*;

/// Guest physical address of the scratch pages KVM may use to emulate real mode on older Intel
/// parts. It must be somewhere the guest will not use; the conventional choice, inherited from
/// qemu, is just below the 4 GiB BIOS area.
const TSS_ADDR: u64 = 0xfffb_d000;

/// The API version KVM has reported since 2007. A different value means the fd is not KVM.
const EXPECTED_API_VERSION: i32 = 12;

// -------------------------------------------------------------------------------------------
// ioctl plumbing
// -------------------------------------------------------------------------------------------

/// `ioctl` with an integer third argument.
///
/// # Safety
/// `req` must be an ioctl this `fd` accepts with a by-value argument. Passing a request that
/// expects a pointer would have the kernel dereference `arg` as an address.
unsafe fn ioctl_val(fd: RawFd, req: u64, arg: u64) -> io::Result<i32> {
    // SAFETY: delegated to the caller by the contract above.
    let r = unsafe { libc::ioctl(fd, req as _, arg) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// `ioctl` with a pointer third argument.
///
/// # Safety
/// `req` must be an ioctl this `fd` accepts, and `arg` must point to a valid, correctly sized `T`.
/// The size is encoded in `req`, so a `T` that does not match the encoded size is a bug the kernel
/// will usually catch with `ENOTTY` - usually, not always, which is why the layouts in `kvm_sys`
/// are asserted at compile time.
unsafe fn ioctl_ptr<T>(fd: RawFd, req: u64, arg: *mut T) -> io::Result<i32> {
    // SAFETY: delegated to the caller by the contract above.
    let r = unsafe { libc::ioctl(fd, req as _, arg) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// An owned `mmap` region, unmapped on drop.
///
/// Both mappings in this VMM outlive every use of them by construction, because they are fields of
/// [`Vm`] and Rust drops fields after the struct body. That ordering matters more than it looks:
/// unmapping guest memory while the vCPU fd is still open would leave KVM's second-level page
/// tables pointing at freed pages, and the fault would surface as hardware behaviour rather than a
/// segfault in this process.
struct Mapping {
    ptr: *mut u8,
    len: usize,
}

impl Mapping {
    /// Anonymous private mapping, for guest RAM.
    fn anonymous(len: usize) -> io::Result<Self> {
        // SAFETY: a fresh anonymous mapping; no existing address is being disturbed.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                // MAP_PRIVATE is sufficient here. A VMM that wants to share guest memory with
                // another process - a vhost-user backend, or a `userfaultfd` handler in a separate
                // process, which is rung 3 - needs MAP_SHARED instead, and that choice is made at
                // allocation time and cannot be changed later.
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping { ptr: ptr.cast(), len })
    }

    /// Shared mapping of a file descriptor, for the `kvm_run` page.
    fn shared_fd(fd: RawFd, len: usize) -> io::Result<Self> {
        // SAFETY: as above; `fd` is a live vCPU fd and KVM defines offset 0 of it as `kvm_run`.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                // MAP_SHARED is mandatory: this page is written by the kernel and by this process,
                // and both must see each other's stores. With MAP_PRIVATE the VMM would get a
                // copy-on-write snapshot and would read a stale exit reason forever.
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapping { ptr: ptr.cast(), len })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are exactly what `mmap` returned and have not been changed.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

// -------------------------------------------------------------------------------------------
// The virtual machine
// -------------------------------------------------------------------------------------------

/// A single-vCPU virtual machine with one region of RAM.
///
/// Field order is drop order, and drop order is the teardown contract: the `kvm_run` mapping and
/// guest RAM must be released *before* the fds that reference them, and the vCPU fd before the VM
/// fd. Rust drops struct fields in declaration order, so the declaration below is load-bearing.
pub struct Vm {
    run_map: Mapping,
    mem: Mapping,
    vcpu: OwnedFd,
    /// Held, not used: the vCPU fd is only valid while the VM fd it came from is open.
    _vm: OwnedFd,
    /// Held, not used: kept so the `/dev/kvm` fd outlives the VM fd derived from it.
    _kvm: OwnedFd,
    mem_size: usize,
}

/// What one call to [`Vm::run`] observed.
#[derive(Debug, Default, Clone, Copy)]
pub struct RunSummary {
    pub exits: u64,
    pub io_exits: u64,
    pub mmio_exits: u64,
    /// `KVM_RUN` returned `EINTR`, or KVM reported `KVM_EXIT_INTR`: the host interrupted the vCPU
    /// (a signal, a scheduler tick that KVM chose to surface). Not an error, but it is a sample the
    /// benchmark must discard, because it measures the host's interruption and not an exit.
    pub interrupted: u64,
}

/// Knobs for a single run.
pub struct RunOptions<'a> {
    /// Print one line per exit to stderr. Guest output goes to stdout, so `2>/dev/null` leaves
    /// exactly what the guest printed.
    pub trace: bool,
    /// If present, one nanosecond sample is appended per `KVM_RUN` round trip.
    pub timings: Option<&'a mut Vec<u64>>,
}

impl Vm {
    /// Build a VM with `mem_size` bytes of RAM at guest physical address 0.
    ///
    /// The sequence below is the canonical KVM bring-up, and the order is forced by the fd
    /// hierarchy: `/dev/kvm` -> VM fd -> vCPU fd, with memory installed on the VM fd and the
    /// communication page mapped from the vCPU fd.
    pub fn new(mem_size: usize) -> io::Result<Self> {
        assert!(
            mem_size.is_multiple_of(4096) && mem_size > 0,
            "guest memory must be a non-zero multiple of the page size"
        );

        // 1. Open the KVM control device. This fd is not a VM; it is the handle on the subsystem,
        //    used for global queries and for creating VMs.
        //
        //    O_CLOEXEC matters in a VMM more than most places: a VMM that later spawns a helper
        //    process would otherwise leak a KVM handle into it, which is a privilege-escalation
        //    surface. Firecracker's jailer exists in large part to reason about exactly this class
        //    of leak.
        // SAFETY: a plain open of a fixed path; the returned fd is immediately taken ownership of.
        let kvm_raw = unsafe { libc::open(c"/dev/kvm".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if kvm_raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `kvm_raw` is a fresh fd owned by nobody else.
        let kvm = unsafe { OwnedFd::from_raw_fd(kvm_raw) };

        // 2. Confirm this really is KVM and that the ABI is the one this code was written against.
        // SAFETY: KVM_GET_API_VERSION takes no argument.
        let version = unsafe { ioctl_val(kvm.as_raw_fd(), KVM_GET_API_VERSION, 0)? };
        if version != EXPECTED_API_VERSION {
            return Err(io::Error::other(format!(
                "unexpected KVM API version {version}, expected {EXPECTED_API_VERSION}"
            )));
        }

        // 3. Confirm userspace may supply its own memory. Without this capability, guest memory
        //    would have to be allocated by the kernel, which no modern VMM does.
        // SAFETY: KVM_CHECK_EXTENSION takes the capability id by value.
        let has_user_mem =
            unsafe { ioctl_val(kvm.as_raw_fd(), KVM_CHECK_EXTENSION, KVM_CAP_USER_MEMORY)? };
        if has_user_mem == 0 {
            return Err(io::Error::other("KVM_CAP_USER_MEMORY unsupported"));
        }

        // 4. Create the VM. The returned fd owns a guest physical address space, initially empty.
        // SAFETY: machine type 0 is the default and is passed by value.
        let vm_raw = unsafe { ioctl_val(kvm.as_raw_fd(), KVM_CREATE_VM, 0)? };
        // SAFETY: `vm_raw` is a fresh fd returned by KVM.
        let vm = unsafe { OwnedFd::from_raw_fd(vm_raw) };

        // 5. Real-mode scratch. Harmless on hardware with unrestricted guest, required without it.
        //    A failure here is not fatal - report it and continue, because on a machine that does
        //    not need it the guest will run fine, and a hard failure would make this example
        //    mysteriously unusable on architectures where the ioctl does not exist.
        // SAFETY: KVM_SET_TSS_ADDR takes a guest physical address by value.
        if let Err(e) = unsafe { ioctl_val(vm.as_raw_fd(), KVM_SET_TSS_ADDR, TSS_ADDR) } {
            eprintln!("note: KVM_SET_TSS_ADDR failed ({e}); continuing, harmless with unrestricted guest");
        }

        // 6. Allocate guest RAM in this process, then hand the range to KVM.
        let mem = Mapping::anonymous(mem_size)?;
        let mut region = kvm_userspace_memory_region {
            slot: 0,
            flags: 0,
            guest_phys_addr: 0,
            memory_size: mem_size as u64,
            userspace_addr: mem.ptr as u64,
        };
        // SAFETY: `region` is a correctly laid out, fully initialised structure of the size encoded
        // in the request number.
        unsafe { ioctl_ptr(vm.as_raw_fd(), KVM_SET_USER_MEMORY_REGION, &mut region)? };

        // 7. Create vCPU 0. From here on, `KVM_RUN` must be issued from this thread.
        // SAFETY: the vCPU id is passed by value.
        let vcpu_raw = unsafe { ioctl_val(vm.as_raw_fd(), KVM_CREATE_VCPU, 0)? };
        // SAFETY: `vcpu_raw` is a fresh fd returned by KVM.
        let vcpu = unsafe { OwnedFd::from_raw_fd(vcpu_raw) };

        // 8. Map the shared communication page. Its size is a kernel property, queried on the
        //    /dev/kvm fd, and it is at least one page but may be larger - the I/O payload area
        //    lives past the header, so mapping only 4096 bytes would work today and corrupt memory
        //    on a kernel that grew the structure.
        // SAFETY: no argument.
        let run_size = unsafe { ioctl_val(kvm.as_raw_fd(), KVM_GET_VCPU_MMAP_SIZE, 0)? } as usize;
        if run_size < size_of::<kvm_run_header>() {
            return Err(io::Error::other(format!("implausible kvm_run size {run_size}")));
        }
        let run_map = Mapping::shared_fd(vcpu.as_raw_fd(), run_size)?;

        Ok(Vm { run_map, mem, vcpu, _vm: vm, _kvm: kvm, mem_size })
    }

    /// Read the guest's general-purpose registers.
    ///
    /// Not needed to run the guest - it is here because reading `rip` after the guest halts is the
    /// cheapest possible demonstration that the vCPU is a real, inspectable object whose state
    /// survives the exit. Snapshotting a VM is this call plus `KVM_GET_SREGS`, the vCPU's MSRs and
    /// FPU state, and a copy of guest RAM.
    pub fn regs(&self) -> io::Result<kvm_regs> {
        let mut regs = kvm_regs::default();
        // SAFETY: `regs` is correctly sized and the kernel only writes into it.
        unsafe { ioctl_ptr(self.vcpu.as_raw_fd(), KVM_GET_REGS, &mut regs)? };
        Ok(regs)
    }

    /// Copy bytes into guest physical memory.
    ///
    /// This is the whole of "loading a kernel". A real VMM parses an ELF or a bzImage and writes it
    /// to the addresses the header asks for, plus a boot parameter block; the mechanism is this
    /// function.
    pub fn load(&mut self, gpa: u64, bytes: &[u8]) -> io::Result<()> {
        let end = gpa as usize + bytes.len();
        if end > self.mem_size {
            return Err(io::Error::other(format!(
                "load of {} bytes at {gpa:#x} exceeds {} bytes of guest RAM",
                bytes.len(),
                self.mem_size
            )));
        }
        // SAFETY: the bounds check above proves the destination range is inside the mapping, and
        // the mapping is writable and not aliased by any live reference.
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.mem.ptr.add(gpa as usize), bytes.len());
        }
        Ok(())
    }

    /// Put the vCPU into flat real mode with `rip` pointing at `entry`.
    ///
    /// After `KVM_CREATE_VCPU` the vCPU is in the architectural reset state, which on x86 means
    /// `cs.selector = 0xf000`, `cs.base = 0xffff0000` and `rip = 0xfff0` - the address a physical
    /// CPU fetches its first instruction from, four gigabytes up, where the firmware ROM is
    /// decoded. Since this VMM has no firmware, the segments are flattened to base 0 so that a
    /// guest offset is a guest physical address, and `rip` is aimed straight at the loaded bytes.
    ///
    /// Writing `base = 0` with `selector = 0` is a state real hardware cannot be *placed* in
    /// directly, only arrive at. KVM exposes the hidden descriptor cache, so a VMM can construct
    /// it. That is also how a VMM restores a snapshot into the middle of a running guest.
    pub fn set_real_mode_regs(&self, entry: u64) -> io::Result<()> {
        let mut sregs = kvm_sregs::default();
        // SAFETY: `sregs` is correctly sized and the kernel only writes into it.
        unsafe { ioctl_ptr(self.vcpu.as_raw_fd(), KVM_GET_SREGS, &mut sregs)? };

        // Flatten every segment. The type/present/limit fields from the reset state are kept: they
        // already describe a usable 64 KiB real-mode segment, and rebuilding them by hand would be
        // a way to get it subtly wrong.
        for seg in [
            &mut sregs.cs,
            &mut sregs.ds,
            &mut sregs.es,
            &mut sregs.fs,
            &mut sregs.gs,
            &mut sregs.ss,
        ] {
            seg.base = 0;
            seg.selector = 0;
        }
        // SAFETY: as above; the kernel reads a fully initialised structure.
        unsafe { ioctl_ptr(self.vcpu.as_raw_fd(), KVM_SET_SREGS, &mut sregs)? };

        let mut regs = kvm_regs {
            rip: entry,
            // Bit 1 of RFLAGS is reserved and architecturally always 1. Entering with it clear is
            // an invalid guest state, and KVM rejects the entry with KVM_EXIT_FAIL_ENTRY rather
            // than running a single instruction - a confusing failure precisely because nothing in
            // the VMM looks wrong.
            rflags: 0x2,
            ..Default::default()
        };
        // SAFETY: as above.
        unsafe { ioctl_ptr(self.vcpu.as_raw_fd(), KVM_SET_REGS, &mut regs)? };
        Ok(())
    }

    /// Read the fixed header of the shared communication page.
    fn header(&self) -> kvm_run_header {
        // SAFETY: the mapping is at least `size_of::<kvm_run_header>()` bytes (checked in `new`),
        // it is `MAP_SHARED`, and the struct is `Copy` with no padding requirements beyond `repr(C)`.
        unsafe { std::ptr::read_volatile(self.run_map.ptr.cast::<kvm_run_header>()) }
    }

    /// Pointer to the exit-information union inside the communication page.
    ///
    /// `T` must be one of the union variants declared in `kvm_sys`, and it is only valid to read
    /// the variant matching the current `exit_reason`.
    fn union_ptr<T>(&self) -> *mut T {
        // SAFETY: `KVM_RUN_UNION_OFFSET` is asserted to equal the header size at compile time, and
        // the mapping is a full `kvm_run` as sized by the kernel, so the union is in bounds.
        unsafe { self.run_map.ptr.add(KVM_RUN_UNION_OFFSET).cast::<T>() }
    }

    /// Enter the guest repeatedly until it halts.
    ///
    /// This is the VMM's main loop, and every VMM has one that looks like this: enter, dispatch on
    /// the reason, resume. The loop is the reason a VM exit is the unit of cost in virtualization -
    /// each iteration is a full round trip out of guest mode, through the kernel, into userspace
    /// and back.
    pub fn run(&mut self, devices: &mut Devices, mut opts: RunOptions<'_>) -> io::Result<RunSummary> {
        let mut summary = RunSummary::default();

        loop {
            let started = opts.timings.as_ref().map(|_| Instant::now());

            // SAFETY: KVM_RUN takes no argument. The vCPU fd is live and this is its owning thread.
            let rc = unsafe { ioctl_val(self.vcpu.as_raw_fd(), KVM_RUN, 0) };

            if let (Some(t0), Some(v)) = (started, opts.timings.as_mut()) {
                v.push(t0.elapsed().as_nanos() as u64);
            }

            if let Err(e) = rc {
                // EINTR is normal, not exceptional. A signal delivered while the vCPU was in guest
                // mode kicks it out; the guest's state is intact and re-entering resumes it. A VMM
                // that treated this as fatal would die whenever the host looked at it funny.
                if e.raw_os_error() == Some(libc::EINTR) {
                    summary.interrupted += 1;
                    continue;
                }
                return Err(e);
            }

            summary.exits += 1;
            let hdr = self.header();

            match hdr.exit_reason {
                KVM_EXIT_IO => {
                    summary.io_exits += 1;
                    // SAFETY: the exit reason says this variant of the union is the live one.
                    let io = unsafe { std::ptr::read_volatile(self.union_ptr::<kvm_run_io>()) };
                    let len = io.size as usize * io.count as usize;
                    let off = io.data_offset as usize;
                    if off.saturating_add(len) > self.run_map.len {
                        return Err(io::Error::other("kvm_run io payload out of bounds"));
                    }
                    // The payload lives at `data_offset` bytes from the start of the *mapping*, not
                    // from the union. This is the detail every hand-written VMM gets wrong once.
                    // SAFETY: the bounds check above proves the range is inside the mapping.
                    let data =
                        unsafe { std::slice::from_raw_parts_mut(self.run_map.ptr.add(off), len) };
                    if opts.trace {
                        eprintln!(
                            "  exit #{:<3} KVM_EXIT_IO   port={:#06x} {} size={} count={} data={:02x?}  rip={:#x}",
                            summary.exits,
                            io.port,
                            if io.direction == KVM_EXIT_IO_OUT { "out" } else { "in " },
                            io.size,
                            io.count,
                            data,
                            self.regs().map(|r| r.rip).unwrap_or(u64::MAX),
                        );
                    }
                    match io.direction {
                        KVM_EXIT_IO_OUT => devices.pio_write(io.port, data),
                        KVM_EXIT_IO_IN => devices.pio_read(io.port, data),
                        d => return Err(io::Error::other(format!("bad io direction {d}"))),
                    }
                }

                KVM_EXIT_MMIO => {
                    summary.mmio_exits += 1;
                    let p = self.union_ptr::<kvm_run_mmio>();
                    // SAFETY: the exit reason says this variant of the union is the live one.
                    let m = unsafe { std::ptr::read_volatile(p) };
                    let len = (m.len as usize).min(8);
                    // An MMIO exit only says "nothing is mapped here". Deciding *which* device, if
                    // any, owns the address is entirely the VMM's job. A real VMM looks it up in a
                    // range map; this one has a single device, so the check is a range test - but
                    // the check must exist, or a stray guest access is silently attributed to the
                    // only device that happens to be implemented.
                    if !Devices::owns_mmio(m.phys_addr) {
                        return Err(io::Error::other(format!(
                            "guest touched unmapped, undeviced guest physical address {:#x}",
                            m.phys_addr
                        )));
                    }
                    if m.is_write != 0 {
                        if opts.trace {
                            eprintln!(
                                "  exit #{:<3} KVM_EXIT_MMIO addr={:#06x} write len={}   guest stored {:02x?}",
                                summary.exits,
                                m.phys_addr,
                                len,
                                &m.data[..len],
                            );
                        }
                        devices.mmio_write(m.phys_addr, &m.data[..len]);
                    } else {
                        // Fill the union's inline buffer. KVM copies it into the guest's
                        // destination register during the next entry, so the store below is not a
                        // message to the kernel - it *is* the value the guest's load returns.
                        let mut buf = [0u8; 8];
                        devices.mmio_read(m.phys_addr, &mut buf[..len]);
                        if opts.trace {
                            eprintln!(
                                "  exit #{:<3} KVM_EXIT_MMIO addr={:#06x} read  len={}   VMM returns {:02x?}",
                                summary.exits,
                                m.phys_addr,
                                len,
                                &buf[..len],
                            );
                        }
                        // SAFETY: `p` points at the live union variant, which is `Copy` and whose
                        // `data` field is exactly 8 bytes.
                        unsafe { std::ptr::addr_of_mut!((*p).data).write_volatile(buf) };
                    }
                }

                KVM_EXIT_HLT => {
                    if opts.trace {
                        eprintln!("  exit #{:<3} KVM_EXIT_HLT   guest halted", summary.exits);
                    }
                    return Ok(summary);
                }

                KVM_EXIT_INTR => {
                    summary.interrupted += 1;
                }

                // Everything else is a bug in this VMM, not an event to absorb.
                // `KVM_EXIT_FAIL_ENTRY` in particular means the hardware refused the guest state,
                // which is almost always wrong register setup rather than anything the guest did.
                _ => {
                    return Err(io::Error::other(format!(
                        "unhandled {} ({})",
                        exit_reason_name(hdr.exit_reason),
                        hdr.exit_reason
                    )));
                }
            }
        }
    }
}
