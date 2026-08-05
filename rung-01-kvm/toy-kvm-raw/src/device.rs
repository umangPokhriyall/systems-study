//! The device model: two devices, both of them one line of logic.
//!
//! The point of this file is how little there is to it. In KVM, "emulating a device" means
//! answering the question *what should have happened when the guest touched this address*. The
//! guest does not know it is being answered by a program; the hardware resumes it with a value in a
//! register exactly as if a chip had responded.
//!
//! Real VMMs differ from this in scale, not in kind. Cloud Hypervisor's `Bus` and Firecracker's
//! `MMIO device manager` are address-range maps that dispatch to a `read`/`write` pair with this
//! signature. The interesting engineering starts when the device is a virtio device whose real work
//! happens in *shared memory* and whose exits are only notifications - which is rung 2.

/// Base of the toy MMIO device in guest physical space.
///
/// Chosen to sit immediately above the single RAM slot. It is a device because nothing maps it:
/// there is no registration step anywhere in this VMM that declares a device here. The guest's
/// access exits *because the address is not backed*, and the VMM decides after the fact what it
/// means. That inversion - devices are defined by the absence of memory - is the single most
/// useful thing to take from this rung.
///
/// It is at 0x8000 rather than somewhere more device-looking because a real-mode guest cannot
/// reach past its 64 KiB segment limit without first widening the limit. See `COMMON-MISTAKES.md`.
pub const MMIO_BASE: u64 = 0x0000_8000;
/// Length of the device's window. One page, of which one byte is implemented.
pub const MMIO_LEN: u64 = 0x1000;

/// COM1 data register. There is no UART here; a byte written is a byte printed. Real VMMs implement
/// the full 8250 register set because guest kernels probe it, but a guest that only ever writes the
/// data register cannot tell the difference.
pub const SERIAL_PORT: u16 = 0x03f8;

/// A one-register device plus a write-only serial port.
#[derive(Debug, Default)]
pub struct Devices {
    /// Everything the guest has written to [`SERIAL_PORT`], in order. Kept rather than printed
    /// immediately so the demo can assert on it: correctness before speed.
    pub serial_out: Vec<u8>,
    /// The single implemented MMIO register: written by the guest, read back by the guest.
    latch: u8,
    /// Counters, so the run loop can report what actually happened rather than what was expected.
    pub pio_writes: u64,
    pub pio_reads: u64,
    pub mmio_writes: u64,
    pub mmio_reads: u64,
    /// Accesses this model does not implement. A non-zero value at the end of a run is a finding,
    /// not a warning to be ignored - it means the guest saw a value the VMM invented.
    pub unhandled: u64,
}

impl Devices {
    /// Guest executed `out`. `data` is what it wrote, `size` bytes per item, `count` items.
    pub fn pio_write(&mut self, port: u16, data: &[u8]) {
        self.pio_writes += 1;
        match port {
            SERIAL_PORT => self.serial_out.extend_from_slice(data),
            // The benchmark port. Deliberately empty: the measurement in `main.rs` is of the exit
            // round trip, so the handler must not contribute to it.
            crate::guest::BENCH_PORT => {}
            _ => self.unhandled += 1,
        }
    }

    /// Guest executed `in`. The VMM must fill `data`; whatever is left there is what the guest
    /// reads.
    ///
    /// The default of `0xff` is not arbitrary. An unterminated bus on real hardware floats high, so
    /// probing an absent device reads all-ones, and guest drivers are written to treat all-ones as
    /// "not present". Returning zero instead would make an absent device look like a present one
    /// answering with zeros, which is how a guest ends up hanging on a device that is not there.
    pub fn pio_read(&mut self, _port: u16, data: &mut [u8]) {
        // No port in this toy is readable: neither guest program executes `in`. Reaching here means
        // the guest did something the model does not describe, so it is counted, not swallowed.
        self.pio_reads += 1;
        self.unhandled += 1;
        data.fill(0xff);
    }

    /// Guest stored to an unbacked guest physical address.
    pub fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        self.mmio_writes += 1;
        match addr - MMIO_BASE {
            0 if !data.is_empty() => self.latch = data[0],
            _ => self.unhandled += 1,
        }
    }

    /// Guest loaded from an unbacked guest physical address.
    ///
    /// Whatever this writes into `data` is placed into the guest's destination register when the
    /// vCPU resumes. The guest cannot distinguish it from a value that came out of a chip.
    pub fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        self.mmio_reads += 1;
        match addr - MMIO_BASE {
            0 if !data.is_empty() => {
                data.fill(0);
                data[0] = self.latch;
            }
            _ => {
                self.unhandled += 1;
                data.fill(0xff);
            }
        }
    }

    /// Is `addr` inside the toy device's window?
    pub fn owns_mmio(addr: u64) -> bool {
        (MMIO_BASE..MMIO_BASE + MMIO_LEN).contains(&addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latch_round_trips() {
        let mut d = Devices::default();
        d.mmio_write(MMIO_BASE, &[0x42]);
        let mut buf = [0u8; 1];
        d.mmio_read(MMIO_BASE, &mut buf);
        assert_eq!(buf[0], 0x42);
        assert_eq!(d.unhandled, 0);
    }

    #[test]
    fn unimplemented_mmio_offset_reads_all_ones_and_is_counted() {
        let mut d = Devices::default();
        let mut buf = [0u8; 1];
        d.mmio_read(MMIO_BASE + 8, &mut buf);
        assert_eq!(buf[0], 0xff);
        assert_eq!(d.unhandled, 1);
    }

    #[test]
    fn serial_accumulates_in_order() {
        let mut d = Devices::default();
        d.pio_write(SERIAL_PORT, b"KV");
        d.pio_write(SERIAL_PORT, b"M");
        assert_eq!(d.serial_out, b"KVM");
    }
}
