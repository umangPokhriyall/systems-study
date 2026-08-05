//! The same VMM as `toy-kvm-raw`, rebuilt on `kvm-ioctls` and `vm-memory`.
//!
//! It boots the same guest bytes and must produce byte-identical output. The purpose is the *diff*:
//! having written the raw version first, it is possible to say precisely what these crates add, and
//! that is worth more than being able to use them.
//!
//! # What the crates actually buy
//!
//! - **ioctl numbers and structure layouts.** The raw version computes `_IOC` encodings and
//!   transcribes `#[repr(C)]` structs by hand. `kvm-bindings` generates them from the kernel
//!   headers. This is the part most worth outsourcing, because a transcription error is silent
//!   until it is a wrong field at a wrong offset.
//! - **Ownership and drop order.** `Kvm`, `VmFd` and `VcpuFd` encode the fd hierarchy in types. The
//!   raw version enforces the same thing with a comment about struct field order, which is correct
//!   and fragile.
//! - **A guest address space instead of a pointer.** `GuestMemoryMmap` is a list of regions with
//!   `GuestAddress` as a distinct type from a host pointer, and every access bounds-checked. The
//!   raw version has one region and one hand-written check.
//! - **A decoded exit.** `match vcpu.run()?` yields variants carrying correctly-sized slices. This
//!   removes the sharpest edge in the raw version: `data_offset` is relative to the start of the
//!   `kvm_run` mapping, and reading it relative to the union instead is a bug that compiles.
//!
//! # What they do not do
//!
//! They do not hide the model. There is still a run loop, still an exit dispatch, still a device
//! defined by the absence of memory, and `set_user_memory_region` is still `unsafe` - because the
//! obligation it carries (the host mapping must outlive the region) is not one a type can
//! discharge. Anyone who learned KVM from this file alone would know the API and not the machine.

use std::error::Error;
use std::io::Write;

use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::{Kvm, VcpuExit};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};

// The guest programs and the device model are shared with the raw crate rather than duplicated.
// Only the VMM is rewritten - that is the whole point of the comparison.
use toy_kvm_raw::device::{self, Devices};
use toy_kvm_raw::guest;

const GUEST_RAM: usize = 32 * 1024;

fn main() -> Result<(), Box<dyn Error>> {
    // 1. `Kvm::new()` is `open("/dev/kvm")` plus the API-version check the raw version does by
    //    hand. The returned value owns the fd; there is no `close` to forget.
    let kvm = Kvm::new()?;
    let vm = kvm.create_vm()?;
    vm.set_tss_address(0xfffb_d000)?;

    // 2. Guest memory as an address space rather than as a pointer.
    let mem = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), GUEST_RAM)])?;

    // 3. Hand every region to KVM. Real VMMs loop exactly like this, which is why the raw version's
    //    single hard-coded slot 0 is the one place it is *less* than a toy: slot management is real
    //    work in a VMM that supports hotplug or a memory map with holes.
    for (slot, region) in mem.iter().enumerate() {
        let mr = kvm_userspace_memory_region {
            slot: slot as u32,
            guest_phys_addr: region.start_addr().raw_value(),
            memory_size: region.len(),
            userspace_addr: mem.get_host_address(region.start_addr())? as u64,
            flags: 0,
        };
        // SAFETY: `mem` owns these mappings and outlives `vm`, so the host range stays valid for as
        // long as KVM's second-level page tables reference it. This is the obligation the type
        // system cannot discharge, and it is why this call is `unsafe` in a crate whose entire
        // purpose is to be the safe wrapper.
        unsafe { vm.set_user_memory_region(mr)? };
    }

    // 4. Load the guest. `write_slice` is bounds-checked against the address space, so the raw
    //    version's manual comparison against `mem_size` is folded into the type.
    mem.write_slice(guest::DEMO_PROGRAM, GuestAddress(guest::LOAD_ADDR))?;

    // 5. vCPU, and the same flat-real-mode setup. Note that this part is *not* shorter than the raw
    //    version: `kvm-ioctls` has nothing to say about what a sensible initial vCPU state is, and
    //    every VMM writes its own. Firecracker and Cloud Hypervisor each have a few hundred lines
    //    doing this properly for long mode, and it is entirely their own code.
    let mut vcpu = vm.create_vcpu(0)?;
    let mut sregs = vcpu.get_sregs()?;
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
    vcpu.set_sregs(&sregs)?;

    let mut regs = vcpu.get_regs()?;
    regs.rip = guest::LOAD_ADDR;
    regs.rflags = 0x2; // reserved bit 1 is architecturally always set
    vcpu.set_regs(&regs)?;

    // 6. The run loop. Compare with the raw version's pointer arithmetic into the shared page: the
    //    same control flow, but the payload arrives as a slice of the right length, and an MMIO read
    //    is answered by writing into a `&mut [u8]` the crate handed us rather than by a volatile
    //    store into a union at a hard-coded offset.
    let mut devices = Devices::default();
    let mut exits = 0u64;
    loop {
        exits += 1;
        match vcpu.run()? {
            VcpuExit::IoOut(port, data) => devices.pio_write(port, data),
            VcpuExit::IoIn(port, data) => devices.pio_read(port, data),
            VcpuExit::MmioWrite(addr, data) => {
                assert!(Devices::owns_mmio(addr), "unmapped, undeviced address {addr:#x}");
                devices.mmio_write(addr, data);
            }
            VcpuExit::MmioRead(addr, data) => {
                assert!(Devices::owns_mmio(addr), "unmapped, undeviced address {addr:#x}");
                devices.mmio_read(addr, data);
            }
            VcpuExit::Hlt => break,
            other => return Err(format!("unhandled exit: {other:?}").into()),
        }
    }

    std::io::stdout().write_all(&devices.serial_out)?;
    std::io::stdout().flush()?;
    eprintln!(
        "  {exits} exits; guest RAM [0x0, {:#x}); device [{:#x}, {:#x})",
        GUEST_RAM,
        device::MMIO_BASE,
        device::MMIO_BASE + device::MMIO_LEN
    );

    if devices.serial_out != guest::DEMO_EXPECTED_OUTPUT {
        return Err("guest output does not match the raw VMM's".into());
    }
    if devices.unhandled != 0 {
        return Err("guest performed accesses the device model does not describe".into());
    }
    eprintln!("  output identical to toy-kvm-raw");
    Ok(())
}
