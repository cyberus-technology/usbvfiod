use super::packet::{UsbDirection, UsbTransferType};
use crate::device::xhci::real_endpoint_handle::{
    ControlRequestProcessingResult, InTrbProcessingResult, OutTrbProcessingResult,
};

const LINUX_ENODEV: i32 = 19;
const LINUX_EPIPE: i32 = 32;
const LINUX_EPROTO: i32 = 71;

const fn errno_status(errno: i32) -> i32 {
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
    pub direction: UsbDirection,
}

impl EndpointPcapMeta {
    pub const fn control(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Control,
            // Placeholder only: control direction is determined per request and overwritten later.
            direction: UsbDirection::HostToDevice,
        }
    }

    pub const fn bulk_in(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Bulk,
            direction: UsbDirection::DeviceToHost,
        }
    }

    pub const fn bulk_out(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Bulk,
            direction: UsbDirection::HostToDevice,
        }
    }

    pub const fn interrupt_in(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Interrupt,
            direction: UsbDirection::DeviceToHost,
        }
    }

    pub const fn interrupt_out(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Interrupt,
            direction: UsbDirection::HostToDevice,
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

pub const fn in_error_status(error: &InTrbProcessingResult) -> i32 {
    match error {
        InTrbProcessingResult::Disconnect => errno_status(LINUX_ENODEV),
        InTrbProcessingResult::Stall => errno_status(LINUX_EPIPE),
        InTrbProcessingResult::TransactionError => errno_status(LINUX_EPROTO),
        InTrbProcessingResult::Success(_) => 0,
    }
}

pub const fn out_error_status(error: &OutTrbProcessingResult) -> i32 {
    match error {
        OutTrbProcessingResult::Disconnect => errno_status(LINUX_ENODEV),
        OutTrbProcessingResult::Stall => errno_status(LINUX_EPIPE),
        OutTrbProcessingResult::TransactionError => errno_status(LINUX_EPROTO),
        OutTrbProcessingResult::Success => 0,
    }
}
