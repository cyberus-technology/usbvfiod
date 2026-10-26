use super::packet::UsbTransferType;
use crate::device::xhci::real_endpoint_handle::{
    ControlRequestProcessingResult, InTrbProcessingStatus, OutTrbProcessingStatus,
};

const LINUX_ENODEV: i32 = 19;
const LINUX_EINVAL: i32 = 22;
const LINUX_EPIPE: i32 = 32;
const LINUX_EPROTO: i32 = 71;

pub const fn errno_status(errno: i32) -> i32 {
    -errno
}

/// Context passed from endpoint handlers to PCAP logging, describing
/// which USB endpoint a record belongs to.
#[derive(Clone, Copy, Debug)]
pub struct EndpointPcapMeta {
    pub bus_number: u16,
    pub device_address: u8,
    pub endpoint_id: u8,
    pub transfer_type: UsbTransferType,
}

impl EndpointPcapMeta {
    pub const fn control(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Control,
        }
    }

    pub const fn bulk(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Bulk,
        }
    }

    pub const fn interrupt(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Interrupt,
        }
    }
}

pub const fn control_error_status(error: &ControlRequestProcessingResult) -> i32 {
    match error {
        ControlRequestProcessingResult::Disconnect => errno_status(LINUX_ENODEV),
        ControlRequestProcessingResult::Stall => errno_status(LINUX_EPIPE),
        ControlRequestProcessingResult::TransactionError => errno_status(LINUX_EPROTO),
        ControlRequestProcessingResult::SuccessfulControlIn(_)
        | ControlRequestProcessingResult::SuccessfulControlOut => 0,
    }
}

pub const fn trb_error_status() -> i32 {
    errno_status(LINUX_EINVAL)
}

pub const fn in_status(status: &InTrbProcessingStatus) -> i32 {
    match status {
        InTrbProcessingStatus::Disconnect => errno_status(LINUX_ENODEV),
        InTrbProcessingStatus::Stall => errno_status(LINUX_EPIPE),
        InTrbProcessingStatus::TransactionError => errno_status(LINUX_EPROTO),
        InTrbProcessingStatus::Success => 0,
    }
}

pub const fn out_status(status: &OutTrbProcessingStatus) -> i32 {
    match status {
        OutTrbProcessingStatus::Disconnect => errno_status(LINUX_ENODEV),
        OutTrbProcessingStatus::Stall => errno_status(LINUX_EPIPE),
        OutTrbProcessingStatus::TransactionError => errno_status(LINUX_EPROTO),
        OutTrbProcessingStatus::Success => 0,
    }
}
