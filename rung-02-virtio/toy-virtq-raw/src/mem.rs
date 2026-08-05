//! The shared memory both sides of a virtqueue live in.
//!
//! In a real system this is guest RAM: the `mmap` that rung 1 handed to KVM with
//! `KVM_SET_USER_MEMORY_REGION`. The driver inside the guest writes to it with ordinary stores and
//! the device outside the guest reads it by dereferencing a host pointer into the same physical
//! pages. There is no copy and no syscall on that path - that fact is what the entire virtio design
//! is built to exploit, and it is why rung 1 comes first.
//!
//! Here it is a `Vec<u8>` indexed by "guest physical address", which is the same thing with the
//! hardware removed.
//!
//! # Why every access returns a `Result`
//!
//! Addresses inside a virtqueue come from the *driver*, which in a real VMM is the guest, which is
//! the thing being isolated. A descriptor's `addr` and `len` are attacker-controlled 64- and 32-bit
//! integers. Every read and write below is bounds-checked against the region because there is no
//! other layer that will do it: the host is dereferencing its own valid pointer into a region it
//! owns, so an out-of-range access is not a segfault, it is the host reading or writing memory that
//! belongs to something else.
//!
//! `vm-memory` exists to make this checking systematic and hard to skip. `GuestAddress` is a
//! distinct type from a host pointer for exactly this reason.

use std::fmt;

/// A guest physical address. A newtype rather than a bare `u64` so that a guest-supplied value
/// cannot be passed where a host offset is expected without saying so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GuestAddr(pub u64);

impl GuestAddr {
    /// Checked addition. Overflow here is not theoretical: `addr` is guest-controlled and
    /// `addr + len` with `addr` near `u64::MAX` wraps to a small number, which would turn a bounds
    /// check into a bounds *pass*.
    pub fn checked_add(self, n: u64) -> Option<GuestAddr> {
        self.0.checked_add(n).map(GuestAddr)
    }
}

impl fmt::Display for GuestAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#x}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemError {
    /// The access ran off the end of the region, or `addr + len` overflowed.
    OutOfBounds { addr: u64, len: u64, region_len: u64 },
}

impl fmt::Display for MemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemError::OutOfBounds { addr, len, region_len } => write!(
                f,
                "access of {len} bytes at {addr:#x} is outside the {region_len:#x}-byte region"
            ),
        }
    }
}

impl std::error::Error for MemError {}

pub type Result<T> = std::result::Result<T, MemError>;

/// A flat region of memory shared by the driver and the device.
pub struct SharedMem {
    buf: Vec<u8>,
}

impl SharedMem {
    pub fn new(len: usize) -> Self {
        SharedMem { buf: vec![0; len] }
    }

    pub fn len(&self) -> u64 {
        self.buf.len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The one bounds check every accessor funnels through.
    fn range(&self, addr: GuestAddr, len: u64) -> Result<std::ops::Range<usize>> {
        let end = addr.checked_add(len).ok_or(MemError::OutOfBounds {
            addr: addr.0,
            len,
            region_len: self.len(),
        })?;
        if end.0 > self.len() {
            return Err(MemError::OutOfBounds { addr: addr.0, len, region_len: self.len() });
        }
        Ok(addr.0 as usize..end.0 as usize)
    }

    pub fn read_slice(&self, addr: GuestAddr, len: u64) -> Result<&[u8]> {
        Ok(&self.buf[self.range(addr, len)?])
    }

    pub fn write_slice(&mut self, addr: GuestAddr, data: &[u8]) -> Result<()> {
        let r = self.range(addr, data.len() as u64)?;
        self.buf[r].copy_from_slice(data);
        Ok(())
    }

    pub fn read_u16(&self, addr: GuestAddr) -> Result<u16> {
        let r = self.range(addr, 2)?;
        Ok(u16::from_le_bytes([self.buf[r.start], self.buf[r.start + 1]]))
    }

    pub fn read_u32(&self, addr: GuestAddr) -> Result<u32> {
        let r = self.range(addr, 4)?;
        Ok(u32::from_le_bytes(self.buf[r.clone()].try_into().unwrap()))
    }

    pub fn read_u64(&self, addr: GuestAddr) -> Result<u64> {
        let r = self.range(addr, 8)?;
        Ok(u64::from_le_bytes(self.buf[r.clone()].try_into().unwrap()))
    }

    pub fn write_u16(&mut self, addr: GuestAddr, v: u16) -> Result<()> {
        self.write_slice(addr, &v.to_le_bytes())
    }

    pub fn write_u32(&mut self, addr: GuestAddr, v: u32) -> Result<()> {
        self.write_slice(addr, &v.to_le_bytes())
    }

    // -------------------------------------------------------------------------------------------
    // Ordering
    // -------------------------------------------------------------------------------------------
    //
    // Everything below is a plain little-endian access, and virtio is explicitly little-endian on
    // the wire regardless of host or guest endianness - the `le16`/`le32` in the spec's structure
    // definitions is normative, not descriptive.
    //
    // What a plain access does *not* give is ordering. In this simulation the driver and the device
    // are the same thread, so no reordering is observable and the fences below are unobservable
    // too. They are written where a real implementation needs them, with the reason, because the
    // places they are needed are not obvious and getting them wrong produces a bug that appears
    // once every few million operations on one machine and never on another.
    //
    // The two that matter:
    //
    //   Driver, publishing a chain:      write descriptors  ->  RELEASE  ->  bump avail.idx
    //   Device, consuming a chain:       read avail.idx     ->  ACQUIRE  ->  read descriptors
    //
    //   Device, completing a chain:      write used elem    ->  RELEASE  ->  bump used.idx
    //   Driver, collecting a completion: read used.idx      ->  ACQUIRE  ->  read used elem
    //
    // Without the release/acquire pair on each side, the *index* can become visible before the data
    // it advertises. The reader then sees a valid-looking index pointing at a descriptor that has
    // not been written yet, and processes garbage. `vm-memory` provides `load`/`store` taking an
    // `Ordering` so this can be expressed per-access rather than as a bare fence.

    /// Read an index field with the ordering a consumer needs.
    ///
    /// The `Acquire` fence after the read is what makes every store the producer made *before*
    /// publishing the index visible to us.
    pub fn load_idx_acquire(&self, addr: GuestAddr) -> Result<u16> {
        let v = self.read_u16(addr)?;
        std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
        Ok(v)
    }

    /// Write an index field with the ordering a producer needs.
    ///
    /// The `Release` fence before the write is what guarantees the descriptors or used elements we
    /// just wrote are visible to the consumer before the index that advertises them.
    pub fn store_idx_release(&mut self, addr: GuestAddr, v: u16) -> Result<()> {
        std::sync::atomic::fence(std::sync::atomic::Ordering::Release);
        self.write_u16(addr, v)
    }

    /// Read a field with no ordering requirement - the notification-suppression hints, which are
    /// advisory by construction. Reading a stale `used_event` costs at most one unnecessary
    /// notification, never a correctness failure. That is a deliberate property of the design, not
    /// an accident: it is what lets the hint be read without synchronising.
    pub fn load_relaxed(&self, addr: GuestAddr) -> Result<u16> {
        self.read_u16(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_bounds_is_refused() {
        let m = SharedMem::new(64);
        assert!(m.read_u32(GuestAddr(60)).is_ok());
        assert!(m.read_u32(GuestAddr(61)).is_err());
        assert!(m.read_slice(GuestAddr(0), 65).is_err());
    }

    #[test]
    fn address_overflow_does_not_wrap_into_a_pass() {
        // The check that matters most. `addr + len` overflowing u64 would produce a small `end`
        // that compares fine against the region length, turning a wild access into an accepted one.
        let m = SharedMem::new(64);
        assert!(m.read_slice(GuestAddr(u64::MAX - 1), 8).is_err());
    }

    #[test]
    fn little_endian_on_the_wire() {
        let mut m = SharedMem::new(16);
        m.write_u32(GuestAddr(0), 0x1234_5678).unwrap();
        assert_eq!(m.read_slice(GuestAddr(0), 4).unwrap(), &[0x78, 0x56, 0x34, 0x12]);
    }
}
