//! # MSI-X Table Emulation
//!
//! MSI-X interrupts are configured via a memory-mapped region in one of the PCI device's BARs. This
//! module contains emulation code for this table. See [MsixTable].

use crate::device::{
    bus::{Request, RequestSize, SingleThreadedBusDevice},
    msi_message::MsiMessage,
    pci::constants::config_space,
    register_set::{RegisterSet, RegisterSetBuilder},
};

/// The size of a single "row" of the MSI-X table in bytes.
pub const MSIX_ENTRY_SIZE: usize = 16;

/// Offsets of fields in an MSI-X table entry.
pub mod offset {
    /// The 64-bit MSI address field.
    pub const MESSAGE_ADDRESS: usize = 0;

    /// The 32-bit MSI data field.
    pub const MESSAGE_DATA: usize = 2 * 4;

    /// Offset of the Control Word.
    pub const CONTROL: usize = 3 * 4;
}

/// A bit in the Control Word that indicates whether this vector is masked.
pub const CONTROL_MASKED: u32 = 1 << 0;

/// The table of MSI-X entries.
///
/// See Figure 6-11 in the PCI Local Bus 3.0 specification.
///
/// Due to [limitations](https://github.com/rust-lang/rust/issues/44580) in Rust's generic
/// programming, this type has to be instantiated with the **size in bytes** instead of the number
/// of desired vectors.
#[derive(Debug)]
pub struct MsixTable<const SIZE_BYTES: usize> {
    registers: RegisterSet<{ SIZE_BYTES }>,
}

impl<const SIZE_BYTES: usize> MsixTable<SIZE_BYTES> {
    /// Construct a MSI-X table with default content.
    #[must_use]
    pub fn new() -> Self {
        assert_eq!(
            SIZE_BYTES % MSIX_ENTRY_SIZE,
            0,
            "The MSI-X table size must be an integer multiple of MSIX_ENTRY_SIZE"
        );
        assert!(SIZE_BYTES > 0);
        assert!(SIZE_BYTES <= usize::from(config_space::msix::MAX_VECTORS) * MSIX_ENTRY_SIZE);

        let mut builder = RegisterSetBuilder::<{ SIZE_BYTES }>::new();

        (0..usize::from(Self::vector_count()))
            .map(|v| v * MSIX_ENTRY_SIZE)
            .for_each(|offset| {
                builder
                    .u64_le_rw_at(offset + offset::MESSAGE_ADDRESS, 0)
                    .u32_le_rw_at(offset + offset::MESSAGE_DATA, 0)
                    .u32_le_rw_at(offset + offset::CONTROL, CONTROL_MASKED);
            });

        Self {
            registers: builder.into(),
        }
    }

    /// Return the number of MSI-X vectors supported by this table.
    #[must_use]
    pub const fn vector_count() -> u16 {
        (SIZE_BYTES / MSIX_ENTRY_SIZE) as u16
    }

    /// Return the MSI address/data pair for the given vector.
    #[must_use]
    #[allow(unused)]
    pub fn vector(&self, vector: u16) -> Option<MsiMessage> {
        assert!(vector < Self::vector_count());

        // SAFETY: MSI-X entry size 16 always fits in u64
        let entry_offset = u64::from(vector) * u64::try_from(MSIX_ENTRY_SIZE).unwrap();

        let field_read = |foffset: usize, size: RequestSize| {
            // SAFETY: MSI-X table entry field offsets (0-12) always fit in u64
            self.registers.read(Request::new(
                entry_offset + u64::try_from(foffset).unwrap(),
                size,
            ))
        };

        // Only bit 0 contains useful data. No need to read the whole word.
        let control = field_read(offset::CONTROL, RequestSize::Size1);

        (control & u64::from(CONTROL_MASKED) == 0).then(|| {
            MsiMessage::new(
                field_read(offset::MESSAGE_ADDRESS, RequestSize::Size8),
                field_read(offset::MESSAGE_DATA, RequestSize::Size2)
                    .try_into()
                    // SAFETY: This unwrap is safe, because we explicitly read a 16-bit value.
                    .unwrap(),
            )
        })
    }
}

impl<const VECTORS: usize> Default for MsixTable<VECTORS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const VECTORS: usize> SingleThreadedBusDevice for MsixTable<VECTORS> {
    fn size(&self) -> u64 {
        self.registers.size()
    }

    fn read(&mut self, req: Request) -> u64 {
        self.registers.read(req)
    }

    fn write(&mut self, req: Request, value: u64) {
        self.registers.write(req, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type ExampleTable = MsixTable<{ 16 * MSIX_ENTRY_SIZE }>;

    #[test]
    fn vector_count_is_correctly_computed() {
        assert_eq!(ExampleTable::vector_count(), 16);
    }

    #[test]
    fn all_vectors_are_masked_by_default() {
        let mut table = ExampleTable::new();

        // The entries are masked for the guest.
        assert!((0..ExampleTable::vector_count())
            .map(|v| u64::from(v) * (MSIX_ENTRY_SIZE as u64) + (offset::CONTROL as u64))
            .map(|offset| table.read(Request::new(offset, RequestSize::Size4)))
            .all(|control| control & u64::from(CONTROL_MASKED) != 0));

        // The entries are masked from the device model perspective.
        assert_eq!(
            (0..ExampleTable::vector_count())
                .filter_map(|v| table.vector(v))
                .count(),
            0
        );
    }

    #[test]
    fn configured_vectors_are_visible() {
        let example_address = 0xcafe_d00d_feed_face;
        let example_data: u16 = 0xbeef;

        let mut table = ExampleTable::new();
        let entry_1_offset: usize = MSIX_ENTRY_SIZE;

        table.write(
            Request::new(
                (entry_1_offset + offset::MESSAGE_ADDRESS) as u64,
                RequestSize::Size8,
            ),
            example_address,
        );
        table.write(
            Request::new(
                (entry_1_offset + offset::MESSAGE_DATA) as u64,
                RequestSize::Size4,
            ),
            example_data.into(),
        );

        assert_eq!(table.vector(1), None);

        table.write(
            Request::new(
                (entry_1_offset + offset::CONTROL) as u64,
                RequestSize::Size4,
            ),
            0,
        );
        assert_eq!(
            table.vector(1),
            Some(MsiMessage {
                address: example_address,
                data: example_data,
            })
        );
    }
}
