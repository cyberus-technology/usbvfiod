use super::writer::{UsbDirection, UsbTransferType};

/// Context passed from endpoint handlers to PCAP logging, describing
/// which USB endpoint a record belongs to.
#[derive(Clone, Copy)]
pub struct EndpointPcapMeta {
    pub bus_number: u16,
    pub device_address: u8,
    pub endpoint_id: u8,
    pub transfer_type: UsbTransferType,
    pub direction: UsbDirection,
}

impl EndpointPcapMeta {
    pub fn control_in(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Control,
            direction: UsbDirection::DeviceToHost,
        }
    }

    pub fn control_out(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Control,
            direction: UsbDirection::HostToDevice,
        }
    }

    pub fn bulk_in(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Bulk,
            direction: UsbDirection::DeviceToHost,
        }
    }

    pub fn bulk_out(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Bulk,
            direction: UsbDirection::HostToDevice,
        }
    }

    pub fn interrupt_in(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Interrupt,
            direction: UsbDirection::DeviceToHost,
        }
    }

    pub fn interrupt_out(bus: u16, dev: u8, ep: u8) -> Self {
        Self {
            bus_number: bus,
            device_address: dev,
            endpoint_id: ep,
            transfer_type: UsbTransferType::Interrupt,
            direction: UsbDirection::HostToDevice,
        }
    }
}
