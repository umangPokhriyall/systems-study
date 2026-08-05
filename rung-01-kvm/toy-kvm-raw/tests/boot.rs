//! End-to-end test: boot the demo guest and check that every exit was handled correctly.
//!
//! This is the test the measurement standard's rule 4 refers to - correctness before speed. The
//! interesting assertion is not that the program ran, it is that the byte the guest printed came
//! back out of the VMM's MMIO read handler, which is only true if the whole exit round trip works.
//!
//! The test **skips** rather than fails when `/dev/kvm` is unavailable, so `cargo test --workspace`
//! is meaningful on a machine without hardware virtualization (a container, a CI runner without the
//! device passed through, a non-x86 host).

use toy_kvm_raw::device::Devices;
use toy_kvm_raw::guest;
use toy_kvm_raw::vmm::{RunOptions, Vm};

const GUEST_RAM: usize = 32 * 1024;

fn kvm_available() -> bool {
    std::fs::OpenOptions::new().read(true).write(true).open("/dev/kvm").is_ok()
}

#[test]
fn demo_guest_boots_and_round_trips_mmio() {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm not available");
        return;
    }

    let mut vm = Vm::new(GUEST_RAM).expect("create vm");
    vm.load(guest::LOAD_ADDR, guest::DEMO_PROGRAM).expect("load guest");
    vm.set_real_mode_regs(guest::LOAD_ADDR).expect("set regs");

    let mut devices = Devices::default();
    let summary = vm
        .run(&mut devices, RunOptions { trace: false, timings: None })
        .expect("run guest");

    assert_eq!(devices.serial_out, guest::DEMO_EXPECTED_OUTPUT);
    assert_eq!(summary.io_exits, 6, "four characters, then the echo and its newline");
    assert_eq!(summary.mmio_exits, 2, "one store and one load to the toy device");
    assert_eq!(devices.unhandled, 0, "guest touched something the device model does not describe");

    // The guest is halted but not destroyed: its registers are still readable, and `rip` sits one
    // byte past the `hlt`. This is the property that makes snapshotting possible at all.
    let regs = vm.regs().expect("read regs");
    let hlt_addr = guest::LOAD_ADDR + guest::DEMO_PROGRAM.len() as u64 - 1;
    assert_eq!(regs.rip, hlt_addr + 1);
}

#[test]
fn loading_past_the_end_of_guest_ram_is_refused() {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm not available");
        return;
    }
    let mut vm = Vm::new(GUEST_RAM).expect("create vm");
    // Silently truncating here would corrupt the host heap in a VMM that used a raw memcpy, which
    // is why the bounds check in `Vm::load` is not decoration.
    assert!(vm.load(GUEST_RAM as u64 - 2, &[0u8; 16]).is_err());
}

#[test]
fn bench_program_produces_the_requested_number_of_exits() {
    if !kvm_available() {
        eprintln!("skipping: /dev/kvm not available");
        return;
    }
    const N: u32 = 1000;
    let mut vm = Vm::new(GUEST_RAM).expect("create vm");
    vm.load(guest::LOAD_ADDR, &guest::bench_program(N)).expect("load guest");
    vm.set_real_mode_regs(guest::LOAD_ADDR).expect("set regs");

    let mut devices = Devices::default();
    let mut timings = Vec::new();
    let summary = vm
        .run(&mut devices, RunOptions { trace: false, timings: Some(&mut timings) })
        .expect("run guest");

    // N I/O exits plus the final halt. If the guest's `loop` counter were patched big-endian, or
    // if `cx` started at zero and wrapped, this is the assertion that would catch it - and it would
    // catch it before a wrong number reached a results file.
    assert_eq!(summary.io_exits, u64::from(N));
    assert_eq!(timings.len(), N as usize + 1);
    assert!(timings.iter().all(|&ns| ns > 0), "a zero-nanosecond exit means the clock is not working");
}
