//! The guest programs, hand-assembled.
//!
//! These are 16-bit real-mode x86 machine code, written as byte arrays rather than assembled from a
//! `.S` file. That is a deliberate choice for this rung: an assembler would hide the one thing
//! worth seeing, which is that the "guest kernel" a VMM boots is nothing more special than bytes
//! placed at an agreed guest physical address with the instruction pointer aimed at them.
//!
//! # Why real mode
//!
//! A vCPU comes out of `KVM_CREATE_VCPU` in the architectural reset state: real mode, no paging, no
//! descriptor tables. Reaching 64-bit long mode requires building a GDT, page tables, and executing
//! a mode-switch sequence - roughly 200 lines of setup that teaches nothing about KVM. Real mode is
//! the state the hardware hands us, so it is the state with the least ceremony between
//! `KVM_CREATE_VCPU` and a running instruction.
//!
//! Real hardware boots the same way; a real bootloader's first job is to escape this mode.
//!
//! # Addressing in these programs
//!
//! Real-mode addresses are normally `segment << 4 + offset`. This VMM instead sets every segment's
//! *hidden base* to 0 (see `vmm.rs`), which real hardware could not do from a cold start but KVM
//! permits. The effect is that an offset in the code is a guest physical address directly, with no
//! arithmetic in the reader's head.
//!
//! Every address a program here touches must fit inside a real-mode segment, whose limit is 64 KiB
//! and is enforced by the hardware regardless of how the address is encoded. That is why guest RAM
//! is 32 KiB and the toy device sits at 0x8000: both are reachable with a plain 16-bit offset.
//! Reaching a device above 64 KiB from real mode is possible, but it requires widening the hidden
//! segment limit first ("unreal mode") - see `EXERCISES.md`, and see `COMMON-MISTAKES.md` for what
//! it looks like when you forget.

/// Guest physical address the programs are loaded at, and the initial `rip`.
///
/// Not zero, because guest physical page 0 is where the real-mode interrupt vector table lives on
/// real hardware, and leaving it alone keeps the layout honest.
pub const LOAD_ADDR: u64 = 0x1000;

/// The demo program.
///
/// ```text
///  offset  bytes            instruction          exit produced
///  ------  ---------------  -------------------  ---------------------------------
///  0x00    BA F8 03         mov dx, 0x3f8        -
///  0x03    B0 4B            mov al, 'K'          -
///  0x05    EE               out dx, al           KVM_EXIT_IO   (out, port 0x3f8)
///  0x06    B0 56            mov al, 'V'          -
///  0x08    EE               out dx, al           KVM_EXIT_IO
///  0x09    B0 4D            mov al, 'M'          -
///  0x0B    EE               out dx, al           KVM_EXIT_IO
///  0x0C    B0 0A            mov al, 0x0a         -
///  0x0E    EE               out dx, al           KVM_EXIT_IO
///  0x0F    B0 42            mov al, 0x42         -
///  0x11    A2 00 80         mov [0x8000], al     KVM_EXIT_MMIO (write, 1 byte)
///  0x14    A0 00 80         mov al, [0x8000]     KVM_EXIT_MMIO (read,  1 byte)
///  0x17    EE               out dx, al           KVM_EXIT_IO
///  0x18    B0 0A            mov al, 0x0a         -
///  0x1A    EE               out dx, al           KVM_EXIT_IO
///  0x1B    F4               hlt                  KVM_EXIT_HLT
/// ```
///
/// The shape is chosen to exercise the three exits a VMM must handle differently:
///
/// - **`out`** is fire-and-forget. KVM reports the bytes the guest wrote and the VMM consumes them.
/// - **The MMIO write** is the same idea at a different address decode: it exits *because there is
///   no memory slot covering 0x10000*, not because anything declared a device there. A device, at
///   the KVM level, is a hole in the guest physical map plus a handler in userspace.
/// - **The MMIO read** is the interesting one, and the reason this program does a store followed by
///   a load of the same address. The VMM must *produce* a value, write it into the shared page, and
///   resume; the guest's `mov al, [...]` then completes with that value in `al` as though memory
///   had answered. Echoing it straight back out to the serial port makes the round trip visible in
///   the output: if the plumbing is wrong, the wrong character is printed.
///
/// Expected guest output: `KVM\nB\n` - `0x42` is ASCII `B`, and it only appears because the VMM's
/// MMIO read handler returned the byte the MMIO write handler latched.
#[rustfmt::skip]
pub const DEMO_PROGRAM: &[u8] = &[
    0xBA, 0xF8, 0x03,                    // mov dx, 0x3f8   (COM1 data register, by convention)
    0xB0, b'K',                          // mov al, 'K'
    0xEE,                                // out dx, al
    0xB0, b'V',                          // mov al, 'V'
    0xEE,                                // out dx, al
    0xB0, b'M',                          // mov al, 'M'
    0xEE,                                // out dx, al
    0xB0, b'\n',                         // mov al, '\n'
    0xEE,                                // out dx, al
    0xB0, 0x42,                          // mov al, 0x42    (the value the toy device will latch)
    0xA2, 0x00, 0x80,                    // mov [0x8000], al   - moffs16 form, DS-relative
    0xA0, 0x00, 0x80,                    // mov al, [0x8000]
    0xEE,                                // out dx, al      (echo whatever the device returned)
    0xB0, b'\n',                         // mov al, '\n'
    0xEE,                                // out dx, al
    0xF4,                                // hlt
];

/// What [`DEMO_PROGRAM`] must print if every exit was handled correctly.
pub const DEMO_EXPECTED_OUTPUT: &[u8] = b"KVM\nB\n";

/// Offset within [`BENCH_PROGRAM`] of the 16-bit iteration count, so it can be patched per round.
const BENCH_COUNT_OFFSET: usize = 4;

/// Port the benchmark exits through. 0x80 is the BIOS POST diagnostic port: writes to it are
/// meaningless to any real device, which is exactly what is wanted - the VMM's handler does
/// nothing, so the measurement is of the exit round trip and not of the handler.
pub const BENCH_PORT: u16 = 0x0080;

/// The exit-cost program: `count` iterations of a single `out`, then halt.
///
/// ```text
///  offset  bytes             instruction        note
///  ------  ----------------  -----------------  ---------------------------------------------
///  0x00    BA 80 00          mov dx, 0x80       diagnostic port; the VMM discards writes
///  0x03    B9 <lo> <hi>      mov cx, count      patched by `bench_program`
///  0x06    EE                out dx, al         one VM exit per iteration
///  0x07    E2 FD             loop 0x06          dec cx; jump if cx != 0  (displacement -3)
///  0x09    F4                hlt                KVM_EXIT_HLT, once, at the end
/// ```
///
/// The loop body is one byte long on purpose. What is being measured is
/// `KVM_RUN` entry + one trivial guest instruction + exit + KVM's in-kernel handling + return to
/// userspace, so every guest-side instruction that is not the `out` is a contaminant. The `loop`
/// instruction is unavoidable and costs a few guest cycles - single-digit nanoseconds against a
/// round trip measured in microseconds, but it is a floor on the accuracy, not a rounding error to
/// be ignored silently.
///
/// `count` is 16 bits because `loop` decrements `cx`, so a single guest run tops out at 65,535
/// samples. More than that is collected by resetting `rip` and `cx` and re-entering, which also
/// demonstrates that a vCPU is a resumable object rather than a one-shot.
#[rustfmt::skip]
const BENCH_TEMPLATE: &[u8] = &[
    0xBA, 0x80, 0x00,        // mov dx, 0x80
    0xB9, 0x00, 0x00,        // mov cx, <patched>
    0xEE,                    // out dx, al
    0xE2, 0xFD,              // loop -3
    0xF4,                    // hlt
];

/// Maximum exits obtainable from one entry into [`BENCH_TEMPLATE`].
pub const BENCH_MAX_PER_ROUND: u32 = u16::MAX as u32;

/// Build the benchmark program with the iteration count patched in.
///
/// # Panics
/// If `count` is 0 or exceeds [`BENCH_MAX_PER_ROUND`]. Zero would be a trap rather than a no-op:
/// `loop` decrements first, so `cx == 0` wraps to 65,535 and runs the maximum number of iterations
/// - the opposite of what the caller asked for. Rejecting it is better than surprising them.
pub fn bench_program(count: u32) -> Vec<u8> {
    assert!(
        count > 0 && count <= BENCH_MAX_PER_ROUND,
        "bench count must be in 1..={BENCH_MAX_PER_ROUND}, got {count}"
    );
    let mut prog = BENCH_TEMPLATE.to_vec();
    let c = count as u16;
    prog[BENCH_COUNT_OFFSET] = (c & 0xff) as u8;
    prog[BENCH_COUNT_OFFSET + 1] = (c >> 8) as u8;
    prog
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bench_count_is_patched_little_endian() {
        // x86 immediates are little-endian; a big-endian patch would silently run a different
        // number of iterations and quietly change every number in the results file.
        let p = bench_program(0x1234);
        assert_eq!(&p[BENCH_COUNT_OFFSET..BENCH_COUNT_OFFSET + 2], &[0x34, 0x12]);
    }

    #[test]
    fn demo_program_ends_in_hlt() {
        // If it does not, the run loop never terminates and the test suite hangs rather than fails.
        assert_eq!(*DEMO_PROGRAM.last().unwrap(), 0xF4);
    }

    #[test]
    #[should_panic]
    fn zero_bench_count_is_rejected() {
        let _ = bench_program(0);
    }
}
