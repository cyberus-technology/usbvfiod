//! # MMIO Register Abstraction
//!
//! This module helps to create device emulation that needs contiguous MMIO regions.

use std::convert::TryInto;

use crate::device::bus::{Request, SingleThreadedBusDevice};

/// A builder for [`RegisterSet`] objects.
///
/// With this struct the MMIO region can be incrementally constructed
/// and finally converted into a matching `RegisterSet` struct, whose
/// layout is immutable.
///
/// # Examples
///
/// ```
/// use usbvfiod::device::register_set::*;
///
/// let region: RegisterSet::<8> = RegisterSetBuilder::<8>::new()
///     .u8_ro_at(0, 0xAB)        // A completely read-only byte register containing 0xAB at offset 0.
///     .u8_at(1, 0x10, 0x0F)     // A byte register with writable low nibble at offset 1.
///     .u16_le_rw_at(2, 0xCAFE)  // A little-endian fully writable 16-bit value.
///     .u32_le_w1c_at(4, 0xFFFF) // A 32-bit write-one-clear register, typically used for status registers.
///     .into();
/// ```
#[derive(Debug, Clone)]
pub struct RegisterSetBuilder<const SIZE: usize> {
    data: [u8; SIZE],
    rw_mask: [u8; SIZE],
    w1c_mask: [u8; SIZE],
}

impl<const SIZE: usize> Default for RegisterSetBuilder<SIZE> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const SIZE: usize> RegisterSetBuilder<SIZE> {
    /// Initialize a builder for a fully read-only MMIO region where
    /// all bits are set.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            data: [0xFF; SIZE],
            rw_mask: [0; SIZE],
            w1c_mask: [0; SIZE],
        }
    }

    fn init_u8(&mut self, pos: usize, value: u8, write_mask: u8, w1c_mask: u8) {
        assert!(pos < SIZE);

        self.data[pos] = value;
        self.rw_mask[pos] = write_mask;
        self.w1c_mask[pos] = w1c_mask;
    }

    fn init_u8_slice(
        &mut self,
        pos: usize,
        value_bytes: &[u8],
        write_mask_bytes: &[u8],
        w1c_mask_bytes: &[u8],
    ) {
        assert_eq!(value_bytes.len(), write_mask_bytes.len());
        assert_eq!(value_bytes.len(), w1c_mask_bytes.len());

        for offset in 0..value_bytes.len() {
            self.init_u8(
                pos + offset,
                value_bytes[offset],
                write_mask_bytes[offset],
                w1c_mask_bytes[offset],
            )
        }
    }

    fn init_u16_le(&mut self, pos: usize, value: u16, write_mask: u16, w1c_mask: u16) {
        self.init_u8_slice(
            pos,
            &value.to_le_bytes(),
            &write_mask.to_le_bytes(),
            &w1c_mask.to_le_bytes(),
        );
    }

    fn init_u32_le(&mut self, pos: usize, value: u32, write_mask: u32, w1c_mask: u32) {
        self.init_u8_slice(
            pos,
            &value.to_le_bytes(),
            &write_mask.to_le_bytes(),
            &w1c_mask.to_le_bytes(),
        );
    }

    fn init_u64_le(&mut self, pos: usize, value: u64, write_mask: u64, w1c_mask: u64) {
        self.init_u8_slice(
            pos,
            &value.to_le_bytes(),
            &write_mask.to_le_bytes(),
            &w1c_mask.to_le_bytes(),
        );
    }

    /// Place a byte at the specified address with a mask indicating
    /// which bits are writable.
    pub fn u8_at(&mut self, pos: usize, value: u8, write_mask: u8) -> &mut Self {
        self.init_u8(pos, value, write_mask, 0);
        self
    }

    /// Place a read-only byte at the given position.
    pub fn u8_ro_at(&mut self, pos: usize, value: u8) -> &mut Self {
        self.u8_at(pos, value, 0)
    }

    /// Place a writable byte at the given position.
    pub fn u8_rw_at(&mut self, pos: usize, value: u8) -> &mut Self {
        self.u8_at(pos, value, 0xFF)
    }

    /// Place a 8-bit write-one-clear (W1C) value at the given position. Bits flip to zero when they
    /// are written with a 1.
    #[allow(unused)]
    pub fn u8_w1c_at(&mut self, pos: usize, value: u8) -> &mut Self {
        self.init_u8(pos, value, 0, 0xFF);
        self
    }

    /// Place a 16-bit value at the specified address in little-endian
    /// order with a mask indicating which bits are writable.
    pub fn u16_le_at(&mut self, pos: usize, value: u16, write_mask: u16) -> &mut Self {
        self.init_u16_le(pos, value, write_mask, 0);
        self
    }

    /// Place a read-only 16-bit value at the given position in
    /// little-endian order.
    pub fn u16_le_ro_at(&mut self, pos: usize, value: u16) -> &mut Self {
        self.u16_le_at(pos, value, 0)
    }

    /// Place a writable 16-bit value at the given position in
    /// little-endian order.
    #[allow(unused)]
    pub fn u16_le_rw_at(&mut self, pos: usize, value: u16) -> &mut Self {
        self.u16_le_at(pos, value, 0xFFFF)
    }

    /// Place a little-endian 16-bit write-one-clear (W1C) value at the given position. Bits flip to
    /// zero when they are written with a 1.
    #[allow(unused)]
    pub fn u16_le_w1c_at(&mut self, pos: usize, value: u16) -> &mut Self {
        self.init_u16_le(pos, value, 0, 0xFFFF);
        self
    }

    /// Place a 32-bit value at the specified address in little-endian
    /// order with a mask indicating which bits are writable.
    pub fn u32_le_at(&mut self, pos: usize, value: u32, write_mask: u32) -> &mut Self {
        self.init_u32_le(pos, value, write_mask, 0);
        self
    }

    /// Place a read-only 32-bit value at the given position in
    /// little-endian order.
    pub fn u32_le_ro_at(&mut self, pos: usize, value: u32) -> &mut Self {
        self.u32_le_at(pos, value, 0)
    }

    /// Place a writable 32-bit value at the given position in
    /// little-endian order.
    pub fn u32_le_rw_at(&mut self, pos: usize, value: u32) -> &mut Self {
        self.u32_le_at(pos, value, 0xFFFF_FFFF)
    }

    /// Place a little-endian 32-bit write-one-clear (W1C) value at the given position. Bits flip to
    /// zero when they are written with a 1.
    #[allow(unused)]
    pub fn u32_le_w1c_at(&mut self, pos: usize, value: u32) -> &mut Self {
        self.init_u32_le(pos, value, 0, 0xFFFF_FFFF);
        self
    }

    /// Place a 64-bit value at the specified address in little-endian
    /// order with a mask indicating which bits are writable.
    pub fn u64_le_at(&mut self, pos: usize, value: u64, write_mask: u64) -> &mut Self {
        self.init_u64_le(pos, value, write_mask, 0);
        self
    }

    /// Place a read-only 64-bit value at the given position in
    /// little-endian order.
    #[allow(unused)]
    pub fn u64_le_ro_at(&mut self, pos: usize, value: u64) -> &mut Self {
        self.u64_le_at(pos, value, 0)
    }

    /// Place a writable 64-bit value at the given position in
    /// little-endian order.
    pub fn u64_le_rw_at(&mut self, pos: usize, value: u64) -> &mut Self {
        self.u64_le_at(pos, value, 0xFFFF_FFFF_FFFF_FFFF)
    }

    /// Place a little-endian 64-bit write-one-clear (W1C) value at the given position. Bits flip to
    /// zero when they are written with a 1.
    #[allow(unused)]
    pub fn u64_le_w1c_at(&mut self, pos: usize, value: u64) -> &mut Self {
        self.init_u64_le(pos, value, 0, 0xFFFF_FFFF_FFFF_FFFF);
        self
    }

    /// Place an already existing register set at the given position.
    ///
    /// This allows to compose larger register sets out of smaller ones. The newly created register
    /// set will inherit the current value and read-write attributes of the given part. The newly
    /// created register set will be completely stand-alone and modifications of its content will
    /// not be reflected in the `regs` parameter passed here or vice versa.
    pub fn register_set_at<const PART_SIZE: usize>(
        &mut self,
        pos: usize,
        regs: &RegisterSet<PART_SIZE>,
    ) -> &mut Self {
        assert!(
            PART_SIZE <= SIZE,
            "Trying to add a register set that is too large"
        );
        assert!(
            pos + PART_SIZE <= SIZE,
            "Not enough space for register set at given position"
        );

        self.data[pos..(pos + PART_SIZE)].copy_from_slice(&regs.data[..PART_SIZE]);
        self.rw_mask[pos..(pos + PART_SIZE)].copy_from_slice(&regs.rw_mask[..PART_SIZE]);
        self.w1c_mask[pos..(pos + PART_SIZE)].copy_from_slice(&regs.w1c_mask[..PART_SIZE]);

        self
    }

    /// Construct the final register set from the build instructions.
    #[must_use]
    pub fn build(&self) -> RegisterSet<SIZE> {
        Iterator::zip(self.rw_mask.iter(), self.w1c_mask.iter())
            .enumerate()
            .for_each(|(offset, (rw_mask, w1c_mask))| {
                let overlap = rw_mask & w1c_mask;
                assert_eq!(
                    overlap, 0,
                    "Writable and W1C bits overlap in register set at offset {offset:#x}: {overlap:#x}",
                );
            });

        RegisterSet {
            data: self.data,
            rw_mask: self.rw_mask,
            w1c_mask: self.w1c_mask,
        }
    }
}

/// A helper for implementing MMIO regions.
///
/// Each `RegisterSet` contains a compile-time sized memory region with
/// configurable writability.
///
/// `RegisterSets` are constructed using [`RegisterSetBuilder`].
#[derive(Debug, Clone)]
pub struct RegisterSet<const SIZE: usize> {
    data: [u8; SIZE],
    rw_mask: [u8; SIZE],
    w1c_mask: [u8; SIZE],
}

impl<const SIZE: usize> RegisterSet<SIZE> {
    /// Write the underlying register value regardless of register writability
    /// or W1C semantics.
    ///
    /// This is typically used by the device emulation logic itself to update
    /// read-only or W1C registers.
    #[allow(unused)]
    pub fn write_direct(&mut self, req: Request, val: u64) {
        let le_bytes = val.to_le_bytes();

        for (req, &byte) in req.iter_bytes().zip(&le_bytes) {
            let off: usize = req.addr.try_into().unwrap();

            self.data[off] = byte;
        }
    }
}

impl<const SIZE: usize> From<&mut RegisterSetBuilder<SIZE>> for RegisterSet<SIZE> {
    fn from(builder: &mut RegisterSetBuilder<SIZE>) -> Self {
        builder.build()
    }
}

impl<const SIZE: usize> From<RegisterSetBuilder<SIZE>> for RegisterSet<SIZE> {
    fn from(builder: RegisterSetBuilder<SIZE>) -> Self {
        builder.build()
    }
}

/// Fold a sequence of bytes into a little-endian value.
///
/// **Note**: This function will cause a runtime error in case the
/// iterator yields more bytes than fit into an u64.
fn fold_iter_le(it: impl Iterator<Item = u8>) -> u64 {
    it.enumerate().fold(0, |acc, (pos, byte)| {
        let bytes_in_u64 = 8;
        assert!(pos < bytes_in_u64);

        let shifted_byte: u64 = u64::from(byte) << (pos * 8);
        acc | shifted_byte
    })
}

impl<const SIZE: usize> RegisterSet<SIZE> {
    /// Same as `read` from [`SingleThreadedBusDevice`], but without requiring a mutable reference.
    #[must_use]
    pub fn read(&self, req: Request) -> u64 {
        fold_iter_le(req.iter_bytes().map(|r| -> u8 {
            let off: usize = r.addr.try_into().unwrap();
            self.data[off]
        }))
    }
}

impl<const SIZE: usize> SingleThreadedBusDevice for RegisterSet<SIZE> {
    fn size(&self) -> u64 {
        // SAFETY: usize always fits in u64
        SIZE.try_into().unwrap()
    }

    fn write(&mut self, req: Request, val: u64) {
        let le_bytes = val.to_le_bytes();

        for (req, &byte) in req.iter_bytes().zip(&le_bytes) {
            let off: usize = req.addr.try_into().unwrap();

            // Set writable bits to zero.
            self.data[off] &= !self.rw_mask[off];

            // Populate writable bits with new content.
            self.data[off] |= byte & self.rw_mask[off];

            // Clear all W1C bits that were written with 1.
            self.data[off] &= !(byte & self.w1c_mask[off]);
        }
    }

    fn read(&mut self, req: Request) -> u64 {
        (self as &Self).read(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::device::bus::RequestSize;

    #[test]
    fn fold_iter_le_works() {
        let no_bytes: [u8; 0] = [];
        assert_eq!(fold_iter_le(no_bytes.iter().copied()), 0);

        let some_bytes: [u8; 2] = [0x11, 0x22];
        assert_eq!(fold_iter_le(some_bytes.iter().copied()), 0x2211);

        let all_bytes: [u8; 8] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
        assert_eq!(
            fold_iter_le(all_bytes.iter().copied()),
            0x8877_6655_4433_2211
        );
    }

    #[test]
    fn unspecified_registers_are_ro_and_have_all_bits_set() {
        let mut region: RegisterSet<8> = RegisterSetBuilder::<8>::new().into();

        // Reads of different sizes.
        assert_eq!(region.read(Request::new(1, RequestSize::Size1)), 0xFF);
        assert_eq!(
            region.read(Request::new(0, RequestSize::Size8)),
            0xFFFF_FFFF_FFFF_FFFF
        );

        // We can't change anything.
        region.write(Request::new(0, RequestSize::Size2), 0);
        assert_eq!(region.read(Request::new(0, RequestSize::Size2)), 0xFFFF);
    }

    #[test]
    fn byte_order_is_observed() {
        let region: RegisterSet<2> = RegisterSetBuilder::<2>::new()
            .u16_le_ro_at(0, 0xCAFE)
            .into();

        assert_eq!(region.read(Request::new(0, RequestSize::Size1)), 0xFE);
        assert_eq!(region.read(Request::new(1, RequestSize::Size1)), 0xCA);
    }

    #[test]
    fn read_only_registers_are_not_writable() {
        let mut region: RegisterSet<2> = RegisterSetBuilder::<2>::new()
            .u16_le_ro_at(0, 0xCAFE)
            .into();

        assert_eq!(region.read(Request::new(0, RequestSize::Size2)), 0xCAFE);

        region.write(Request::new(0, RequestSize::Size2), 0);
        assert_eq!(region.read(Request::new(0, RequestSize::Size2)), 0xCAFE);
    }

    #[test]
    fn writable_registers_are_writable() {
        let mut region: RegisterSet<2> = RegisterSetBuilder::<2>::new()
            .u16_le_rw_at(0, 0xCAFE)
            .into();

        assert_eq!(region.read(Request::new(0, RequestSize::Size2)), 0xCAFE);

        region.write(Request::new(0, RequestSize::Size2), 0xD00D);
        assert_eq!(region.read(Request::new(0, RequestSize::Size2)), 0xD00D);
    }

    #[test]
    fn partially_writable_registers_observe_write_mask() {
        let mut region: RegisterSet<2> = RegisterSetBuilder::<2>::new()
            .u16_le_at(0, 0xCAFE, 0x0F0F)
            .into();

        assert_eq!(region.read(Request::new(0, RequestSize::Size2)), 0xCAFE);

        region.write(Request::new(0, RequestSize::Size2), 0x4433);
        assert_eq!(region.read(Request::new(0, RequestSize::Size2)), 0xC4F3);
    }

    #[test]
    fn cross_register_accesses_are_handled() {
        let region: RegisterSet<8> = RegisterSetBuilder::<8>::new()
            .u16_le_ro_at(0, 0xCAFE)
            .u16_le_ro_at(2, 0xD00D)
            .u16_le_ro_at(4, 0x0BAD)
            .u16_le_ro_at(6, 0xF00D)
            .into();

        assert_eq!(
            region.read(Request::new(0, RequestSize::Size8)),
            0xF00D_0BAD_D00D_CAFE
        );
    }

    #[test]
    fn can_place_register_set() {
        let part: RegisterSet<4> = RegisterSetBuilder::<4>::new()
            .u32_le_at(0, 0x12345678, 0xFFFF0000)
            .into();
        let mut whole: RegisterSet<16> = RegisterSetBuilder::<16>::new()
            .register_set_at(4, &part)
            .into();

        // Check whether the initial data is copied.
        assert_eq!(whole.read(Request::new(4, RequestSize::Size4)), 0x12345678);

        // Write over the partially read-only value to see whether the RW mask is copied.
        whole.write(Request::new(4, RequestSize::Size4), 0xABCDEF12);
        assert_eq!(whole.read(Request::new(4, RequestSize::Size4)), 0xABCD5678);
    }

    #[test]
    fn write_clear_bits_are_cleared() {
        let mut region: RegisterSet<32> = RegisterSetBuilder::<32>::new()
            .u8_w1c_at(1, 0xFF)
            .u16_le_w1c_at(4, 0xFFFF)
            .u32_le_w1c_at(8, 0xFFFF_FFFF)
            .u64_le_w1c_at(16, 0xFFFF_FFFF_FFFF_FFFF)
            .into();

        // u8
        assert_eq!(region.read(Request::new(1, RequestSize::Size1)), 0xFF);
        region.write(Request::new(1, RequestSize::Size1), 0x10);
        assert_eq!(region.read(Request::new(1, RequestSize::Size1)), 0xEF);

        // u16
        assert_eq!(region.read(Request::new(4, RequestSize::Size2)), 0xFFFF);
        region.write(Request::new(4, RequestSize::Size2), 0x1020);
        assert_eq!(region.read(Request::new(4, RequestSize::Size2)), 0xEFDF);

        // u32
        assert_eq!(
            region.read(Request::new(8, RequestSize::Size4)),
            0xFFFF_FFFF
        );
        region.write(Request::new(8, RequestSize::Size4), 0x1020_3040);
        assert_eq!(
            region.read(Request::new(8, RequestSize::Size4)),
            0xEFDF_CFBF
        );

        // u64
        assert_eq!(
            region.read(Request::new(16, RequestSize::Size8)),
            0xFFFF_FFFF_FFFF_FFFF
        );
        region.write(Request::new(16, RequestSize::Size8), 0x1020_3040_5060_7080);
        assert_eq!(
            region.read(Request::new(16, RequestSize::Size8)),
            0xEFDF_CFBF_AF9F_8F7F
        );
    }

    #[test]
    fn write_clear_bits_are_copied() {
        let subregion: RegisterSet<4> = RegisterSetBuilder::<4>::new().u8_w1c_at(1, 0xFF).into();
        let mut region: RegisterSet<8> = RegisterSetBuilder::<8>::new()
            .register_set_at(4, &subregion)
            .into();

        assert_eq!(region.read(Request::new(5, RequestSize::Size1)), 0xFF);
        region.write(Request::new(5, RequestSize::Size1), 0x10);
        assert_eq!(region.read(Request::new(5, RequestSize::Size1)), 0xEF);
    }

    #[test]
    fn write_direct_works() {
        let mut region: RegisterSet<1> = RegisterSetBuilder::<1>::new().u8_w1c_at(0, 0xFF).into();

        region.write(Request::new(0, RequestSize::Size1), 0xf0);
        assert_eq!(region.read(Request::new(0, RequestSize::Size1)), 0x0f);

        // Test if the bits with w1c semantic are overwritten.
        region.write_direct(Request::new(0, RequestSize::Size1), 0xf0);
        assert_eq!(region.read(Request::new(0, RequestSize::Size1)), 0xf0);
    }
}
