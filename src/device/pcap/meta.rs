use super::packet::{UsbDirection, UsbTransferType};

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
