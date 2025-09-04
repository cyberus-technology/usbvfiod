//! # PCI Configuration Space Helpers
//!
//! This module contains helpers for creating and emulating a PCI Configuration Space. To construct
//! a Configuration Space use [`ConfigSpaceBuilder`].

use crate::device::{
    bus::{Request, RequestSize, SingleThreadedBusDevice},
    register_set::{RegisterSet, RegisterSetBuilder},
};

use super::{
    constants::config_space::{
        self, command, header_type, mask::CAPABILITIES_POINTER as CAPABILITY_POINTER_MASK, offset,
        status, MAX_BARS,
    },
    traits::RequestKind,
};

/// The offset at which we start to allocate capabilities.
const INITIAL_CAPABILITY_OFFSET: u8 = 0x40;

/// Meta-information about a PCI BAR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarInfo {
    /// The size of the BAR in bytes.
    pub size: u32,

    /// The type of requests this BAR matches.
    pub kind: RequestKind,
}

impl BarInfo {
    const fn new(size: u32, kind: RequestKind) -> Self {
        Self { size, kind }
    }
}

/// A builder for [`ConfigSpace`] objects.
#[derive(Debug, Clone)]
pub struct ConfigSpaceBuilder {
    reg_builder: RegisterSetBuilder<{ config_space::SIZE }>,
    multifunction: bool,
    revision: u8,
    interrupt_pin: u8,
    interrupt_line: u8,
    status: u16,

    bars: [Option<BarInfo>; MAX_BARS],

    /// The offset in the Configuration Space where we add the next capability.
    ///
    /// This has to be a 4-byte aligned address as mandated by the PCI specification.
    next_capability_offset: u8,

    /// The offset where the capability pointer needs to be updated when we add a capability,
    last_capability_pointer: u8,

    /// Whether customer registers have been added.
    has_custom_registers: bool,
}

impl ConfigSpaceBuilder {
    /// Create a builder for [`ConfigSpace`] with default settings.
    ///
    /// This will create a Configuration Space with default behavior for standard fields.
    ///
    /// There are pre-defined constants for [`vendor`](super::constants::config_space::vendor) and
    /// [`device`](super::constants::config_space::device) IDs.
    #[must_use]
    pub fn new(vendor: u16, device: u16) -> Self {
        let mut reg_builder = RegisterSetBuilder::<{ config_space::SIZE }>::new();

        reg_builder
            .u16_le_ro_at(offset::VENDOR, vendor)
            .u16_le_ro_at(offset::DEVICE, device)
            .u16_le_at(offset::COMMAND, 0, command::WRITABLE_BITS)
            .u8_rw_at(offset::CACHE_LINE_SIZE, 0)
            .u8_rw_at(offset::LATENCY_TIMER, 0)
            .u8_ro_at(offset::BIST, 0)
            .u32_le_ro_at(offset::ROM_BAR, 0)
            .u8_ro_at(offset::MIN_GNT, 0)
            .u8_ro_at(offset::MAX_LAT, 0);

        for i in 0..MAX_BARS {
            // Unimplemented BARs are hardwired to zero.
            reg_builder.u32_le_ro_at(offset::BAR_0 + i * 4, 0);
        }

        Self {
            reg_builder,
            multifunction: false,
            revision: 0,
            interrupt_pin: 0,
            interrupt_line: 255,
            status: 0,
            bars: [None; MAX_BARS],

            // If you change the initial value, be sure to check whether we still set the `STATUS`
            // bit correctly when we finalize the Configuration Space.
            next_capability_offset: INITIAL_CAPABILITY_OFFSET,
            // SAFETY: PCI standard offset 0x34 fits in u8
            last_capability_pointer: offset::CAPABILITIES_POINTER.try_into().unwrap(),

            has_custom_registers: false,
        }
    }

    /// Configure the class and subclass field.
    ///
    /// When these are not set, they default to `0xFF`, which is the undefined device class and
    /// subclass.
    ///
    /// There are pre-defined constants for [`class`](super::constants::config_space::class) and
    /// [`subclass`](super::constants::config_space::subclass) fields.
    #[must_use]
    pub fn class(mut self, class: u8, subclass: u8, prog_if: u8) -> Self {
        self.reg_builder
            .u8_ro_at(offset::CLASS, class)
            .u8_ro_at(offset::SUBCLASS, subclass)
            .u8_ro_at(offset::PROG_IF, prog_if);

        self
    }

    /// Configure the revision field for this device.
    ///
    /// When not specified, the revision defaults to 0.
    #[must_use]
    #[allow(unused)]
    pub const fn revision(mut self, revision: u8) -> Self {
        self.revision = revision;

        self
    }

    /// Configure the subsystem and subsystem vendor IDs.
    ///
    /// The `subsystem_vendor_id` uses the same values as the normal PCI device [vendor
    /// ID](super::constants::config_space::vendor).
    #[must_use]
    #[allow(unused)]
    pub fn subsystem(mut self, subsystem_vendor_id: u16, subsystem_id: u16) -> Self {
        self.reg_builder
            .u16_le_ro_at(offset::SUBSYSTEM_VENDOR_ID, subsystem_vendor_id)
            .u16_le_ro_at(offset::SUBSYSTEM_ID, subsystem_id);

        self
    }

    /// Mark the device as a multifunction device.
    ///
    /// This is necessary for guests to probe additional functions on this device.
    #[must_use]
    #[allow(unused)]
    pub const fn multifunction(mut self) -> Self {
        self.multifunction = true;
        self
    }

    /// Configure the PCI interrupt pin information field for this device.
    ///
    /// When not specified, the interrupt pin defaults to 0 (None).
    #[must_use]
    #[allow(unused)]
    pub const fn interrupt_pin(mut self, irq_pin: u8) -> Self {
        self.interrupt_pin = irq_pin;

        self
    }

    /// Configure the PCI interrupt line information field for this device.
    ///
    /// When not specified, the interrupt line defaults to `0xff` (not connected).
    #[must_use]
    #[allow(unused)]
    pub const fn interrupt_line(mut self, irq_line: u8) -> Self {
        self.interrupt_line = irq_line;

        self
    }

    /// Add non-standard registers to the Configuration Space.
    ///
    /// Do not use this function to re-define standard PCI Configuration Space fields. This function
    /// must also not be used when PCI capabilities have been added with
    /// [`capability`](Self::capability).
    #[allow(unused)]
    pub fn custom_registers<F>(mut self, custom_regs_fn: F) -> Self
    where
        F: FnOnce(&mut RegisterSetBuilder<{ config_space::SIZE }>),
    {
        assert_eq!(
            self.next_capability_offset, INITIAL_CAPABILITY_OFFSET,
            "Cannot add custom registers when PCI capabilities have been added"
        );
        custom_regs_fn(&mut self.reg_builder);
        self.has_custom_registers = true;

        self
    }

    /// Add a Base Address Register (BAR) for a non-prefetchable 32-bit memory region.
    ///
    /// This is the typical BAR type for MMIO regions.
    ///
    /// Size must be a power of 2 and at least 16 bytes, but 4 KiB is the recommended minimum.
    ///
    /// Guest operating systems typically fare better when MMIO regions are at least as large as the
    /// page size (4 KiB). This is especially true when userspace drivers are used. When BARs are
    /// smaller than the page size, BARs from multiple devices may point to a single frame of
    /// physical memory. This frame can then not be safely mapped to userspace.
    #[must_use]
    pub fn mem32_nonprefetchable_bar(mut self, index: u8, size: u32) -> Self {
        let index: usize = index.into();

        assert!(index < MAX_BARS);
        assert_eq!(self.bars[index], None);

        assert!(size.is_power_of_two());
        assert!(size >= 16);

        self.reg_builder
            .u32_le_at(config_space::offset::BAR_0 + index * 4, 0, !(size - 1));

        self.bars[index] = Some(BarInfo::new(size, RequestKind::Memory));
        self
    }

    /// Add a PCI capability to the Configuration Space.
    ///
    /// The given `regs` must not contain the generic PCI Capability header (ID and next
    /// pointer). These fields will be added automatically.
    ///
    /// As capabilities are placed automatically in the free space of the Config Space, they
    /// interact poorly with any registers that are added via
    /// [`custom_registers`](Self::custom_registers). As such, adding capabilities and custom
    /// registers is not allowed at the same time.
    #[must_use]
    pub fn capability<const CAP_SIZE: usize>(
        mut self,
        capability_id: u8,
        regs: &RegisterSet<CAP_SIZE>,
    ) -> Self {
        let offset = self.next_capability_offset;
        assert_eq!(offset & !CAPABILITY_POINTER_MASK, 0);
        assert!(
            !self.has_custom_registers,
            "Custom registers interact poorly with our automated placement of capabilities"
        );

        let header_size = 2;
        let next_offset = usize::from(offset) + header_size + CAP_SIZE;
        assert!(next_offset <= u8::MAX.into());

        // The next capability must start at an aligned address.
        self.next_capability_offset =
            ((next_offset + !usize::from(CAPABILITY_POINTER_MASK)) as u8) & CAPABILITY_POINTER_MASK;

        self.reg_builder
            // Extend the capability pointer list to include the new capability.
            .u8_ro_at(self.last_capability_pointer.into(), offset)
            // Add the capability header. The next pointer will be written when we add the next
            // capability or when we finalize the Configuration Space.
            .u8_ro_at(offset.into(), capability_id)
            // Add the register body.
            .register_set_at(usize::from(offset) + header_size, regs);

        self.last_capability_pointer = offset + 1;
        self
    }

    /// Check whether there is a configured BAR of the right kind and with at least the given size.
    fn has_bar(&self, bar_no: u8, required_kind: RequestKind, minimum_size: u32) -> bool {
        if let Some(BarInfo { size, kind }) = self.bars[usize::from(bar_no)] {
            kind == required_kind && size >= minimum_size
        } else {
            false
        }
    }

    /// Add a MSI-X capability.
    ///
    /// MSI-X allows devices to configure a large number of MSIs via two regions in their memory BARs:
    ///
    /// - the MSI-X table, an array of MSI address/data fields per MSI plus control bits,
    /// - the Pending Bit Array (PBA), a bit field that indicates which of these interrupts is currently pending.
    ///
    /// Where these regions are is configured via the MSI-X capability.
    /// The PBA is not typically used or emulated.
    ///
    /// # Parameters
    ///
    /// - `msix_count`: The number of MSI-X vectors.
    /// - `table_bar_no`: The index of the BAR that contains the MSI-X table.
    /// - `table_bar_offset`: The offset of the MSI-X table in the given BAR in bytes. Must be 4-byte aligned.
    /// - `pba_bar_no`: The index of the BAR that contains the PBA.
    /// - `pba_bar_offset`: The offset of the PBA in the given BAR in bytes. Must be 4-byte aligned.
    #[must_use]
    pub fn msix_capability(
        self,
        msix_count: u16,
        table_bar_no: u8,
        table_bar_offset: u32,
        pba_bar_no: u8,
        pba_bar_offset: u32,
    ) -> Self {
        assert!(msix_count > 0);
        assert!(msix_count <= config_space::msix::MAX_VECTORS);

        // The size of an entry in the MSI-X table.
        const MSIX_TABLE_ENTRY_SIZE: u32 = 16;
        assert_eq!(table_bar_offset & 0x3, 0);
        assert!(
            self.has_bar(
                table_bar_no,
                RequestKind::Memory,
                table_bar_offset + u32::from(msix_count) * MSIX_TABLE_ENTRY_SIZE
            ),
            "MSI-X capability points to mismatching BAR for the MSI-X table"
        );

        assert_eq!(pba_bar_offset & 0x3, 0);

        // The size of the Pending Bit Array in full bytes.
        let pba_bytes = u32::from(msix_count).div_ceil(8);

        assert!(self.has_bar(
            pba_bar_no,
            RequestKind::Memory,
            // The PBA size must be rounded to 8 byte.
            pba_bar_offset + pba_bytes.div_ceil(8)
        ));

        let msix_cap: RegisterSet<10> = RegisterSetBuilder::<10>::new()
            // The capability stores the last valid MSI-X table index.
            .u16_le_at(
                0,
                msix_count - 1,
                config_space::msix::control::WRITABLE_BITS,
            )
            .u32_le_ro_at(2, table_bar_offset | u32::from(table_bar_no))
            .u32_le_rw_at(6, pba_bar_offset | u32::from(pba_bar_no))
            .into();

        self.capability(config_space::capability_id::MSI_X, &msix_cap)
    }

    /// Create the finalized Configuration Space object.
    #[must_use]
    pub fn config_space(mut self) -> ConfigSpace {
        ConfigSpace {
            bars: self.bars,
            config_space: self
                .reg_builder
                // This field is written by firmware at boot time to indicate which PIC pin the
                // interrupt is routed to. A value of 255 means "no connection" and this is a good
                // default.
                .u8_rw_at(offset::IRQ_LINE, self.interrupt_line)
                // This is the physical PCI interrupt pin the device is connected to. A value of 0 means
                // that its not connected to any interrupt line.
                .u8_ro_at(offset::IRQ_PIN, self.interrupt_pin)
                // The status field is not actually read-only in hardware. It has error bits that can be
                // cleared by writing 1 into them. As we can never set these bits, we get the correct
                // semantics by hardcoding the error bits to zero.
                .u16_le_ro_at(
                    offset::STATUS,
                    self.status
                        | if self.next_capability_offset == INITIAL_CAPABILITY_OFFSET {
                            0
                        } else {
                            status::CAPABILITIES
                        },
                )
                .u8_ro_at(offset::REVISION, self.revision)
                .u8_ro_at(
                    offset::HEADER_TYPE,
                    header_type::TYPE_00
                        | if self.multifunction {
                            header_type::MULTIFUNCTION
                        } else {
                            0
                        },
                )
                // Finalize the list of capabilities by ending the pointer chain.
                .u8_ro_at(self.last_capability_pointer.into(), 0)
                .into(),
        }
    }
}

/// The result of matching a request against a BAR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarMatch {
    /// A request relative to the BAR itself.
    pub request: Request,

    /// The index of the BAR that matched.
    pub bar_no: u8,
}

/// The Configuration Space of a PCI device.
///
/// Use [`ConfigSpaceBuilder`] to construct this.
///
/// # Limitations
///
/// This Configuration Space emulation is currently limited by not supporting any side effects for
/// writes. That means any register in the config space that needs to behave differently from memory
/// cannot be represented. This stems from the underlying limitation of [`RegisterSet`].
#[derive(Debug, Clone)]
pub struct ConfigSpace {
    config_space: RegisterSet<{ config_space::SIZE }>,
    bars: [Option<BarInfo>; MAX_BARS],
}

/// An iterator that yields offsets of standard PCI capabilities.
struct CapabilityIterator<'a> {
    config_space: &'a ConfigSpace,
    cap_offset: u8,
}

impl Iterator for CapabilityIterator<'_> {
    type Item = u8;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cap_offset == 0 {
            return None;
        }

        let cap_ptr =
            self.config_space
                .read(Request::new(self.cap_offset.into(), RequestSize::Size1)) as u8
                & CAPABILITY_POINTER_MASK;

        if cap_ptr == 0 {
            self.cap_offset = 0;
            None
        } else {
            // The pointer points to the ID field. The next offset is one byte after it.
            self.cap_offset = cap_ptr + 1;
            Some(cap_ptr)
        }
    }
}

impl ConfigSpace {
    /// Same as `read` from [`SingleThreadedBusDevice`], but without requiring a mutable reference.
    #[must_use]
    pub fn read(&self, req: Request) -> u64 {
        self.config_space.read(req)
    }

    /// Iterate over all capabilities of the Configuration Space.
    ///
    /// The resulting iterator returns the Configuration Space offset of each standard PCI
    /// capability.
    #[allow(unused)]
    pub fn iter_capability_offsets(&self) -> impl Iterator<Item = u8> + '_ {
        CapabilityIterator {
            config_space: self,
            // SAFETY: PCI standard offset 0x34 fits in u8
            cap_offset: config_space::offset::CAPABILITIES_POINTER
                .try_into()
                .unwrap(),
        }
    }

    /// Retrieve information about a specific BAR.
    pub fn bar(&self, bar_no: u8) -> Option<BarInfo> {
        self.bars.get(usize::from(bar_no)).and_then(|&b| b)
    }
}

impl SingleThreadedBusDevice for ConfigSpace {
    fn size(&self) -> u64 {
        self.config_space.size()
    }

    fn read(&mut self, req: Request) -> u64 {
        self.config_space.read(req)
    }

    fn write(&mut self, req: Request, value: u64) {
        self.config_space.write(req, value)
    }
}

#[cfg(test)]
mod tests {
    use crate::device::bus::RequestSize;

    use super::*;

    #[test]
    fn device_vendor_id_are_set() {
        let example_vendor_id = 0xDEAD;
        let example_device_id = 0xBEEF;
        let cfg_space: ConfigSpace =
            ConfigSpaceBuilder::new(example_vendor_id, example_device_id).config_space();

        for (offset, value) in [
            (offset::VENDOR, example_vendor_id),
            (offset::DEVICE, example_device_id),
        ] {
            assert_eq!(
                cfg_space.read(Request::new(offset as u64, RequestSize::Size2)),
                u64::from(value)
            );
        }
    }

    #[test]
    fn class_codes_are_set() {
        let example_class = 0xDE;
        let example_subclass = 0xAD;
        let example_prog_if = 0x11;
        let cfg_space: ConfigSpace = ConfigSpaceBuilder::new(0, 0)
            .class(example_class, example_subclass, example_prog_if)
            .config_space();

        for (offset, value) in [
            (offset::CLASS, example_class),
            (offset::SUBCLASS, example_subclass),
            (offset::PROG_IF, example_prog_if),
        ] {
            assert_eq!(
                cfg_space.read(Request::new(offset as u64, RequestSize::Size1)),
                u64::from(value)
            );
        }
    }

    #[test]
    fn subsystem_ids_are_set() {
        let example_subsystem_vendor = 0xDEAD;
        let example_subsystem = 0xBEEF;
        let cfg_space: ConfigSpace = ConfigSpaceBuilder::new(0, 0)
            .subsystem(example_subsystem_vendor, example_subsystem)
            .config_space();

        for (offset, value) in [
            (offset::SUBSYSTEM_VENDOR_ID, example_subsystem_vendor),
            (offset::SUBSYSTEM_ID, example_subsystem),
        ] {
            assert_eq!(
                cfg_space.read(Request::new(offset as u64, RequestSize::Size2)),
                u64::from(value)
            );
        }
    }

    #[test]
    fn create_single_function_device_by_default() {
        let cfg_space: ConfigSpace = ConfigSpaceBuilder::new(0, 0).config_space();

        assert_eq!(
            cfg_space.read(Request::new(offset::HEADER_TYPE as u64, RequestSize::Size1))
                & u64::from(header_type::MULTIFUNCTION),
            0
        )
    }

    #[test]
    fn can_create_multifunction_device() {
        let cfg_space: ConfigSpace = ConfigSpaceBuilder::new(0, 0).multifunction().config_space();

        assert_eq!(
            cfg_space.read(Request::new(offset::HEADER_TYPE as u64, RequestSize::Size1))
                & u64::from(header_type::MULTIFUNCTION),
            u64::from(header_type::MULTIFUNCTION)
        )
    }

    #[test]
    fn can_add_custom_registers() {
        let example_offset = 0xC0;
        let example_value = 0xAA;
        let mut cfg_space: ConfigSpace = ConfigSpaceBuilder::new(0, 0)
            .custom_registers(|r| {
                r.u8_rw_at(example_offset, example_value);
            })
            .config_space();

        let req = Request::new(example_offset as u64, RequestSize::Size1);

        assert_eq!(cfg_space.read(req), u64::from(example_value));

        cfg_space.write(req, 0xBB);
        assert_eq!(cfg_space.read(req), 0xBB);
    }

    #[test]
    fn revision_defaults_to_zero() {
        let cfg_space: ConfigSpace = ConfigSpaceBuilder::new(0, 0).config_space();

        assert_eq!(
            cfg_space.read(Request::new(offset::REVISION as u64, RequestSize::Size1)),
            0
        )
    }

    #[test]
    fn can_set_revision() {
        let example_revision = 0x12;
        let cfg_space: ConfigSpace = ConfigSpaceBuilder::new(0, 0)
            .revision(example_revision)
            .config_space();

        assert_eq!(
            cfg_space.read(Request::new(offset::REVISION as u64, RequestSize::Size1)),
            u64::from(example_revision)
        )
    }

    #[test]
    fn expose_no_capabilities_by_default() {
        let cfg_space: ConfigSpace = ConfigSpaceBuilder::new(0, 0).config_space();

        assert_eq!(
            cfg_space.read(Request::new(offset::STATUS as u64, RequestSize::Size2))
                & u64::from(status::CAPABILITIES),
            0
        );

        // This is not strictly necessary, if we don't announce capabilities in the status register,
        // but there is also no harm in not putting garbage here.
        assert_eq!(
            cfg_space.read(Request::new(
                offset::CAPABILITIES_POINTER as u64,
                RequestSize::Size1
            )),
            0
        );
    }

    #[test]
    fn can_add_one_capability() {
        let example_id = 0x12;
        let example_capability: RegisterSet<2> = RegisterSetBuilder::<2>::new()
            .u16_le_ro_at(0, 0xAABB)
            .into();

        let cfg_space: ConfigSpace = ConfigSpaceBuilder::new(0, 0)
            .capability(example_id, &example_capability)
            .config_space();

        // We announce a capability list.
        assert_eq!(
            cfg_space.read(Request::new(offset::STATUS as u64, RequestSize::Size2))
                & u64::from(status::CAPABILITIES),
            u64::from(status::CAPABILITIES)
        );

        let cap_ptr = cfg_space.read(Request::new(
            offset::CAPABILITIES_POINTER as u64,
            RequestSize::Size1,
        )) & u64::from(CAPABILITY_POINTER_MASK);

        // At the announced capability offset, we see its header and content.
        assert_eq!(
            cfg_space.read(Request::new(cap_ptr, RequestSize::Size1)),
            u64::from(example_id)
        );

        // The capability list terminates at this capability.
        assert_eq!(
            cfg_space.read(Request::new(cap_ptr + 1, RequestSize::Size1)),
            0
        );

        assert_eq!(
            cfg_space.read(Request::new(cap_ptr + 2, RequestSize::Size2)),
            0xAABB
        );
    }

    #[test]
    fn capabilities_are_correctly_chained() {
        let example_id_1 = 0x12;
        let example_capability_1: RegisterSet<4> = RegisterSetBuilder::<4>::new()
            .u32_le_ro_at(0, 0xAABBCCDD)
            .into();

        let example_id_2 = 0x23;
        let example_capability_2: RegisterSet<2> = RegisterSetBuilder::<2>::new()
            .u16_le_ro_at(0, 0x1122)
            .into();

        let cfg_space: ConfigSpace = ConfigSpaceBuilder::new(0, 0)
            .capability(example_id_1, &example_capability_1)
            .capability(example_id_2, &example_capability_2)
            .config_space();

        let cap_1_ptr = cfg_space.read(Request::new(
            offset::CAPABILITIES_POINTER as u64,
            RequestSize::Size1,
        )) & u64::from(CAPABILITY_POINTER_MASK);

        let cap_2_ptr = cfg_space.read(Request::new(cap_1_ptr + 1, RequestSize::Size1))
            & u64::from(CAPABILITY_POINTER_MASK);

        assert_eq!(
            cfg_space.read(Request::new(cap_2_ptr, RequestSize::Size1)),
            u64::from(example_id_2)
        );

        assert_eq!(
            cfg_space.read(Request::new(cap_2_ptr + 1, RequestSize::Size1)),
            0
        );

        assert_eq!(
            cfg_space.read(Request::new(cap_2_ptr + 2, RequestSize::Size2)),
            0x1122
        );
    }

    #[test]
    fn bars_sizing_works() {
        const BAR_SIZE: u32 = 0x1000;

        let mut cfg_space = ConfigSpaceBuilder::new(0, 0)
            .mem32_nonprefetchable_bar(1, BAR_SIZE)
            .config_space();

        // Guest operating systems determine the size of the region behind the BAR by checking which
        // lower bits don't toggle. If 12 lower bits don't toggle, the BAR describes 2^12 byte
        // region.
        cfg_space.write(
            Request::new(offset::BAR_1 as u64, RequestSize::Size4),
            0xFFFF_FFFF,
        );
        let bar_val = cfg_space.read(Request::new(offset::BAR_1 as u64, RequestSize::Size4));

        assert_eq!(bar_val, 0xFFFF_F000);
    }

    #[test]
    #[should_panic]
    fn can_only_refer_to_existing_bars_in_msix_cap() {
        let _ = ConfigSpaceBuilder::new(0, 0)
            .msix_capability(16, 1, 0x1234_5670, 2, 0x2345_6780)
            .config_space();
    }

    #[test]
    fn can_create_msix_capability() {
        let cfg_space = ConfigSpaceBuilder::new(0, 0)
            .mem32_nonprefetchable_bar(1, 0x8000_0000)
            .mem32_nonprefetchable_bar(2, 0x8000_0000)
            .msix_capability(16, 1, 0x1234_5670, 2, 0x2345_6780)
            .config_space();

        let msix_ptr = cfg_space.read(Request::new(
            offset::CAPABILITIES_POINTER as u64,
            RequestSize::Size1,
        )) & u64::from(CAPABILITY_POINTER_MASK);

        assert_eq!(
            cfg_space.read(Request::new(msix_ptr, RequestSize::Size4)),
            0x0f0011
        );
        assert_eq!(
            cfg_space.read(Request::new(msix_ptr + 4, RequestSize::Size4)),
            0x1234_5671
        );
        assert_eq!(
            cfg_space.read(Request::new(msix_ptr + 8, RequestSize::Size4)),
            0x2345_6782
        );
    }

    #[test]
    fn capability_iterator_works() {
        let no_cap_cfg_space = ConfigSpaceBuilder::new(0, 0).config_space();

        assert_eq!(no_cap_cfg_space.iter_capability_offsets().next(), None);

        let example_id_1 = 0x23;
        let example_id_2 = 0x34;
        let empty_capability: RegisterSet<0> = RegisterSetBuilder::<0>::new().into();

        let cfg_space: ConfigSpace = ConfigSpaceBuilder::new(0, 0)
            .capability(example_id_1, &empty_capability)
            .capability(example_id_2, &empty_capability)
            .config_space();

        let offsets: Vec<u8> = cfg_space.iter_capability_offsets().collect();

        assert_eq!(offsets.len(), 2);

        assert_eq!(
            cfg_space.read(Request::new(offsets[0].into(), RequestSize::Size1)),
            u64::from(example_id_1)
        );
        assert_eq!(
            cfg_space.read(Request::new(offsets[1].into(), RequestSize::Size1)),
            u64::from(example_id_2)
        );
    }

    #[test]
    fn can_query_bars() {
        let cfg_space = ConfigSpaceBuilder::new(0, 0)
            .mem32_nonprefetchable_bar(0, 0x8000_0000)
            .config_space();

        assert_eq!(
            cfg_space.bar(0),
            Some(BarInfo {
                size: 0x8000_0000,
                kind: RequestKind::Memory
            })
        );
        assert_eq!(cfg_space.bar(1), None);
    }
}
