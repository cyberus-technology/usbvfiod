//! # PCI Constants
//!
//! This module collects PCI related constants. All definitions are derived from the PCI
//! Spec, either the "PCI Local Bus Specification" or newer "PCI Express Base Specification"
//! documents.

// Allow missing docs to avoid duplicating the PCI spec for all constants.
#![allow(missing_docs)]
// Allow unused constants that might come in handy at some point.
#![allow(unused)]

/// The maximum number of busses on a PCI segment.
pub const MAX_BUSES: usize = 256;

/// The maximum number of devices on a PCI bus.
pub const MAX_BUS_DEVICES: usize = 32;

/// The maximum number of functions in a PCI device.
pub const MAX_DEVICE_FUNCTIONS: usize = 8;

/// The maximum number of devices on a PCI segment.
pub const MAX_DEVICES: usize = MAX_BUSES * MAX_BUS_DEVICES * MAX_DEVICE_FUNCTIONS;

/// Constants related to the configuration space.
pub mod config_space {

    /// The config space size of a single PCI device in bytes.
    pub const SIZE: usize = 256;

    /// The maximum number of Base Address Registers (BARs) per device.
    pub const MAX_BARS: usize = 6;

    /// The size in bytes of a single BAR.
    pub const BAR_ENTRY_SIZE: usize = 4;

    /// Masks for various configuration space fields.
    pub mod mask {
        pub const CAPABILITIES_POINTER: u8 = 0xfc;
        pub const PIO_BAR_MARKER: u64 = 0x1;
        pub const PIO_BAR_ADDRESS: u64 = 0xffff_fffc;
        pub const MMIO_BAR_TYPE: u64 = 0x6;
        pub const MMIO_BAR_64_BIT: u64 = 0x4;
        pub const MMIO_BAR_ADDRESS: u64 = 0xffff_fff0;
    }

    /// The offsets of various fields in the configuration space.
    pub mod offset {
        pub const VENDOR: usize = 0x0;
        pub const DEVICE: usize = 0x2;
        pub const COMMAND: usize = 0x4;
        pub const STATUS: usize = 0x6;
        pub const REVISION: usize = 0x8;
        pub const PROG_IF: usize = 0x9;
        pub const SUBCLASS: usize = 0xA;
        pub const CLASS: usize = 0xB;
        pub const CACHE_LINE_SIZE: usize = 0xC;
        pub const LATENCY_TIMER: usize = 0xD;
        pub const HEADER_TYPE: usize = 0xE;
        pub const BIST: usize = 0xF;

        pub const BAR_0: usize = 0x10;
        pub const BAR_1: usize = 0x14;
        pub const BAR_2: usize = 0x18;
        pub const BAR_3: usize = 0x1C;
        pub const BAR_4: usize = 0x20;
        pub const BAR_5: usize = 0x24;

        pub const SUBSYSTEM_VENDOR_ID: usize = 0x2C;
        pub const SUBSYSTEM_ID: usize = 0x2E;
        pub const ROM_BAR: usize = 0x30;
        pub const CAPABILITIES_POINTER: usize = 0x34;
        pub const IRQ_LINE: usize = 0x3C;
        pub const IRQ_PIN: usize = 0x3D;
        pub const MIN_GNT: usize = 0x3E;
        pub const MAX_LAT: usize = 0x3F;
    }

    /// The device vendor.
    pub mod vendor {
        pub const INVALID: u16 = 0xFFFF;
        pub const INTEL: u16 = 0x8086;
        pub const REDHAT: u16 = 0x1b36;
        pub const VIRTIO: u16 = 0x1AF4;
    }

    pub mod device {
        pub const INVALID: u16 = 0xFFFF;
        pub const I440FX_HOST_BRIDGE: u16 = 0x1237;
        pub const PIIX4_ISA_BRIDGE: u16 = 0x7110;
        pub const PIIX4_PM_DEVICE: u16 = 0x7113;
        pub const REDHAT_XHCI: u16 = 0x000d;

        /// Virtio devices occupy a range of device IDs.
        ///
        /// The concrete device ID is computed by adding the virtio-specific type ID to this value.
        pub const VIRTIO_DEVICE: u16 = 0x1040;
    }

    /// Command Register Constants.
    pub mod command {
        pub const WRITABLE_BITS: u16 = 0x077F;
    }

    /// Status Register Constants.
    pub mod status {
        /// The device has a list of capabilities starting at
        /// [`CAPABILITIES_POINTER`](super::offset::CAPABILITIES_POINTER).
        pub const CAPABILITIES: u16 = 1 << 4;
    }

    /// PCI class constants.
    pub mod class {
        pub const BRIDGE: u8 = 0x6;
        pub const SERIAL: u8 = 0xc;
        pub const UNASSIGNED: u8 = 0xFF;
    }

    /// PCI sub-class constants.
    pub mod subclass {
        pub const HOST_BRIDGE: u8 = 0x0;
        pub const PCI_TO_ISA_BRIDGE: u8 = 0x1;
        pub const OTHER_BRIDGE: u8 = 0x80;
        pub const SERIAL_USB: u8 = 0x03;
        pub const UNASSIGNED: u8 = 0xFF;
    }

    /// PCI programming interface constants.
    pub mod progif {
        pub const USB_XHCI: u8 = 0x30;
    }

    /// PCI header type.
    ///
    /// This is usually type 0, except for PCI-to-PCI bridges and other exotic devices such as
    /// Cardbus bridges.
    pub mod header_type {
        pub const TYPE_00: u8 = 0;
        pub const MULTIFUNCTION: u8 = 1 << 7;
    }

    /// IDs for PCI Capabilities.
    pub mod capability_id {
        pub const MSI: u8 = 0x05;
        pub const VENDOR_SPECIFIC: u8 = 0x09;
        pub const MSI_X: u8 = 0x11;
    }

    /// Markers for iterating the list of capabilities.
    pub mod capability_list {
        pub const END_OF_LIST: u8 = 0;
    }

    /// Constants for the MSI capability.
    pub mod msi {
        /// Size of the capability in bytes.
        pub const SIZE: usize = 16;

        /// The offset of the message control register.
        pub const CONTROL: u64 = 2;
        /// The offset of the lower address part.
        pub const ADDRESS_LOW: u64 = 4;
        /// The offset of the high address part of a 64 bit address.
        pub const ADDRESS_HIGH: u64 = 8;
        /// The offset of the data field.
        pub const DATA: u64 = 12;

        /// Constants for the Control field.
        pub mod control {
            pub const ENABLE: u16 = 1 << 0;
        }
    }

    /// Constants for the MSI-X capability.
    pub mod msix {
        /// The size of the MSI-X capability.
        pub const SIZE: usize = 12;

        /// The maximum number of MSI-X vectors.
        ///
        /// Note that the table size field in the [`control`] register contains the _last valid
        /// index_, not the maximum number.
        pub const MAX_VECTORS: u16 = 0x800;

        /// The offset of the message control register.
        pub const CONTROL: u64 = 2;
        /// The offset for MSI-X Table Offset and BAR indicator.
        pub const TABLE_INFO: u64 = 4;
        /// The offset for MSI-X Pending Bit Array Offset and BAR indicator.
        pub const PBA_INFO: u64 = 8;

        /// Masks of the table info field.
        pub mod table_info {
            pub const REGION: u8 = 0b111;
            pub const OFFSET: u32 = !0b111;
        }

        /// Constants for the Control field.
        pub mod control {
            pub const ENABLE: u16 = 1 << 15;
            pub const FUNCTION_MASK: u16 = 1 << 14;

            pub const WRITABLE_BITS: u16 = ENABLE | FUNCTION_MASK;
        }
    }
}

/// Constants related to the XHCI MMIO space.
pub mod xhci {

    /// Value for the operational base as returned for reading CAPLENGTH.
    pub const OP_BASE: u64 = 0x40;
    /// Runtime register base offset.
    pub const RUN_BASE: u64 = 0x3000;
    /// Number of USB 3 ports that we use
    pub const NUM_USB3_PORTS: u64 = 2;
    /// Number of USB 2 ports that we use
    pub const NUM_USB2_PORTS: u64 = 1;
    /// Maximum number of supported ports.
    pub const MAX_PORTS: u64 = NUM_USB3_PORTS + NUM_USB2_PORTS;
    /// Maximum number of supported interrupter register sets.
    pub const MAX_INTRS: u64 = 1;
    /// Maximum number of supported device slots.
    pub const MAX_SLOTS: u64 = 1;
    /// Maximum Event Ring Segment Table size as an exponent.
    ///
    /// The actual maximum number of segments is 2^MAX_ERST_SIZE_EXP.
    /// This value is encoded in the HCSPARAMS2 register to inform
    /// the driver about the Event Ring capabilities.
    /// Current value allows up to 2^15 = 32768 segments.
    pub const MAX_ERST_SIZE_EXP: u64 = 15;

    /// Offsets of various fields from the start of the XHCI MMIO region.
    pub mod offset {
        /// Capability Register Offsets
        pub const CAPLENGTH: u64 = 0x0;
        pub const HCIVERSION: u64 = 0x2;
        pub const HCSPARAMS1: u64 = 0x4;
        pub const HCSPARAMS2: u64 = 0x8;
        pub const HCSPARAMS3: u64 = 0xc;
        pub const HCCPARAMS1: u64 = 0x10;
        pub const DBOFF: u64 = 0x14;
        pub const RTSOFF: u64 = 0x18;
        pub const HCCPARAMS2: u64 = 0x1c;

        /// Extended Capabilities
        pub const SUPPORTED_PROTOCOLS: u64 = 0x20;
        pub const SUPPORTED_PROTOCOLS_CONFIG: u64 = 0x28;
        pub const SUPPORTED_PROTOCOLS_USB2: u64 = 0x30;
        pub const SUPPORTED_PROTOCOLS_USB2_CONFIG: u64 = 0x38;

        /// Operational Register Offsets
        pub const USBCMD: u64 = super::OP_BASE;
        pub const USBSTS: u64 = super::OP_BASE + 0x4;
        pub const PAGESIZE: u64 = super::OP_BASE + 0x8;
        pub const DNCTL: u64 = super::OP_BASE + 0x14;
        pub const CRCR: u64 = super::OP_BASE + 0x18;
        pub const CRCR_HI: u64 = super::OP_BASE + 0x1c;
        pub const DCBAAP: u64 = super::OP_BASE + 0x30;
        pub const DCBAAP_HI: u64 = super::OP_BASE + 0x34;
        pub const CONFIG: u64 = super::OP_BASE + 0x38;
        pub const PORT_STRIDE: u64 = 0x10;

        /// USB3 Port 1 base address - each port uses PORT_STRIDE bytes: PORTSC(+0x0), PORTPMSC(+0x4), PORTLI(+0x8)
        pub const PORTSC_USB3: u64 = super::OP_BASE + 0x400; /* +(PORT_STRIDE * (portnr-1)) */
        pub const PORTPMSC_USB3: u64 = super::OP_BASE + 0x404;
        pub const PORTLI_USB3: u64 = super::OP_BASE + 0x408;

        /// USB2 port comes after all USB3 ports (NUM_USB3_PORTS * PORT_STRIDE bytes later)
        pub const PORTSC_USB2: u64 = super::OP_BASE + 0x400 + (super::NUM_USB3_PORTS * PORT_STRIDE);
        pub const PORTPMSC_USB2: u64 = PORTSC_USB2 + 0x4;
        pub const PORTLI_USB2: u64 = PORTSC_USB2 + 0x8;

        /// Runtime Register Offsets
        pub const MFINDEX: u64 = super::RUN_BASE;

        /// Per Interruptor Runtime Register Offsets
        pub const IR0: u64 = super::RUN_BASE + 0x20;

        pub const IMAN: u64 = IR0;
        pub const IMOD: u64 = IR0 + 0x4;
        pub const ERSTSZ: u64 = IR0 + 0x8;
        pub const ERSTBA: u64 = IR0 + 0x10;
        pub const ERSTBA_HI: u64 = IR0 + 0x14;
        pub const ERDP: u64 = IR0 + 0x18;
        pub const ERDP_HI: u64 = IR0 + 0x1c;

        /// Relevant doorbell registers
        pub const DOORBELL_CONTROLLER: u64 = 0x2000;
        pub const DOORBELL_DEVICE: u64 = 0x2004;
    }

    /// Constants for the capability register.
    pub mod capability {
        /// We only emulate version 1.0.0 of the XHCI spec for simplicity.
        pub const HCIVERSION: u64 = 0x100;
        pub const HCSPARAMS1: u64 =
            (super::MAX_PORTS << 24) | (super::MAX_INTRS << 8) | super::MAX_SLOTS;
        pub const HCSPARAMS2: u64 = super::MAX_ERST_SIZE_EXP << 4;
        pub const HCCPARAMS1: u64 = super::offset::SUPPORTED_PROTOCOLS << 14;

        pub mod supported_protocols {
            const ID: u64 = 2;
            const MAJOR: u64 = 0x03;
            const MINOR: u64 = 0x20;
            const NEXT: u64 = (super::super::offset::SUPPORTED_PROTOCOLS_USB2
                - super::super::offset::SUPPORTED_PROTOCOLS)
                >> 2;
            pub const CAP_INFO: u64 = ID | (MAJOR << 24) | (MINOR << 16) | (NEXT << 8);
            pub const CONFIG: u64 = 1 | (super::super::NUM_USB3_PORTS << 8);
        }

        pub mod supported_protocols_usb2 {
            const ID: u64 = 2;
            const MAJOR: u64 = 0x02;
            const MINOR: u64 = 0x00;
            const NEXT: u64 = 0;
            pub const CAP_INFO: u64 = ID | (MAJOR << 24) | (MINOR << 16) | (NEXT << 8);
            pub const CONFIG: u64 =
                (super::super::NUM_USB3_PORTS + 1) | (super::super::NUM_USB2_PORTS << 8);
        }
    }

    /// Constants for the operational registers.
    pub mod operational {
        pub mod crcr {
            pub const DEQUEUE_POINTER_MASK: u64 = !0x3fu64;
            pub const RCS: u64 = 0x1;
            pub const CS: u64 = 0x2;
            pub const CA: u64 = 0x4;
            pub const CRR: u64 = 0x8;
        }

        pub mod portsc {
            pub const CCS: u64 = 0x1;
            pub const PED: u64 = 0x2;
            pub const OCA: u64 = 0x8;
            pub const PR: u64 = 0x10;
            pub const PLS: u64 = 0x1e0;
            pub const PP: u64 = 0x200;
            pub const PORT_SPEED: u64 = 0x3c00;
            pub const PIC: u64 = 0xc000;
            pub const LWS: u64 = 0x10000;
            pub const CSC: u64 = 0x20000;
            pub const PEC: u64 = 0x40000;
            pub const WRC: u64 = 0x80000;
            pub const OCC: u64 = 0x100000;
            pub const PRC: u64 = 0x200000;
            pub const PLC: u64 = 0x400000;
            pub const CEC: u64 = 0x800000;
            pub const CAS: u64 = 0x1000000;
            pub const WCE: u64 = 0x2000000;
            pub const WDE: u64 = 0x4000000;
            pub const WOE: u64 = 0x8000000;
            pub const DR: u64 = 0x40000000;
            pub const WPR: u64 = 0x80000000;
        }

        pub mod usbsts {
            pub const HCH: u64 = 0x1;
            pub const HSE: u64 = 0x4;
            pub const EINT: u64 = 0x8;
            pub const PCD: u64 = 0x10;
            pub const SSS: u64 = 0x100;
            pub const RSS: u64 = 0x200;
            pub const SRE: u64 = 0x400;
            pub const CNR: u64 = 0x800;
            pub const HCE: u64 = 0x1000;
        }
    }

    /// Constants for the runtime registers.
    pub mod runtime {
        /// The default minimum interrupt interval of ~1ms (4000 * 250ns).
        pub const IMOD_DEFAULT: u64 = 4000;
    }

    /// Constants for the rings
    pub mod rings {
        /// TRB size in bytes
        pub const TRB_SIZE: usize = 16;

        /// The identifiers of transfer request blocks
        pub mod trb_types {
            pub const NORMAL: u8 = 1;
            pub const SETUP_STAGE: u8 = 2;
            pub const DATA_STAGE: u8 = 3;
            pub const STATUS_STAGE: u8 = 4;
            pub const ISOCH: u8 = 5;
            pub const LINK: u8 = 6;
            pub const EVENT_DATA: u8 = 7;
            pub const NO_OP: u8 = 8;

            pub const ENABLE_SLOT_COMMAND: u8 = 9;
            pub const DISABLE_SLOT_COMMAND: u8 = 10;
            pub const ADDRESS_DEVICE_COMMAND: u8 = 11;
            pub const CONFIGURE_ENDPOINT_COMMAND: u8 = 12;
            pub const EVALUATE_CONTEXT_COMMAND: u8 = 13;
            pub const RESET_ENDPOINT_COMMAND: u8 = 14;
            pub const STOP_ENDPOINT_COMMAND: u8 = 15;
            pub const SET_TR_DEQUEUE_POINTER_COMMAND: u8 = 16;
            pub const RESET_DEVICE_COMMAND: u8 = 17;
            pub const FORCE_EVENT_COMMAND: u8 = 18;
            pub const NEGOTIATE_BANDWIDTH_COMMAND: u8 = 19;
            pub const SET_LATENCY_TOLERANCE_VALUE_COMMAND: u8 = 20;
            pub const GET_PORT_BANDWIDTH_COMMAND: u8 = 21;
            pub const FORCE_HEADER_COMMAND: u8 = 22;
            pub const NO_OP_COMMAND: u8 = 23;
            pub const GET_EXTENDED_PROPERTY_COMMAND: u8 = 24;
            pub const SET_EXTENDED_PROPERTY_COMMAND: u8 = 25;

            pub const TRANSFER_EVENT: u8 = 32;
            pub const COMMAND_COMPLETION_EVENT: u8 = 33;
            pub const PORT_STATUS_CHANGE_EVENT: u8 = 34;
            pub const BANDWIDTH_REQUEST_EVENT: u8 = 35;
            pub const DOORBELL_EVENT: u8 = 36;
            pub const HOST_CONTROLLER_EVENT: u8 = 37;
            pub const DEVICE_NOTIFICATION_EVENT: u8 = 38;
            pub const MFINDEX_WRAP_EVENT: u8 = 39;
        }
        /// Constants specific to the event rings
        pub mod event_ring {
            /// The offsets to fields in Event Ring Segment Table Entries (ERSTE)
            pub mod segments_table_entry_offsets {
                pub const SEGMENT_BASE: u64 = 0;
                pub const SIZE: u64 = 8;
            }
        }
    }

    /// Constants specific to device slots and their context structures
    pub mod device_slots {
        /// The slot state encoded in the slot context
        pub mod slot_state {
            pub const DISABLED_ENABLED: u8 = 0;
            pub const DEFAULT: u8 = 1;
            pub const ADDRESSED: u8 = 2;
            pub const CONFIGURED: u8 = 3;
        }
        /// The endpoint state encoded in endpoint contexts
        pub mod endpoint_state {
            pub const DISABLED: u8 = 0;
            pub const RUNNING: u8 = 1;
            pub const HALTED: u8 = 2;
            pub const STOPPED: u8 = 3;
            pub const ERROR: u8 = 4;
        }
    }
}
