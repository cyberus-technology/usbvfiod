use std::sync::{Arc, Mutex};

use crate::device::bus::{AddBusDeviceError, Bus, BusDevice, BusDeviceRef, Request};
use arc_swap::ArcSwap;

#[derive(Debug)]
struct DeviceEntry {
    start_addr: u64,
    device: BusDeviceRef,
}

impl DeviceEntry {
    fn cloned(&self) -> Self {
        Self {
            start_addr: self.start_addr,
            device: self.device.clone(),
        }
    }
}

#[derive(Default, Debug)]
pub struct DynamicBus {
    segments: Mutex<Vec<DeviceEntry>>,
    bus: Arc<ArcSwap<Bus>>,
}

impl DynamicBus {
    pub fn new() -> Self {
        Default::default()
    }

    /// Insert a mapping at `start_addr`, replacing a previous mapping that
    /// starts at the same address.
    ///
    /// This is the idempotent variant of a plain insert, which is required when
    /// a reconnecting vfio-user client re-maps the very same guest memory range,
    /// as the destination VMM does while taking over a device during a live
    /// migration.
    ///
    /// Mappings that overlap an existing one at a *different* start address are
    /// still rejected, and on error the bus is left unchanged.
    pub fn add(&self, start_addr: u64, device: BusDeviceRef) -> Result<(), AddBusDeviceError> {
        let mut segments = self.segments.lock().unwrap();

        let mut candidate: Vec<DeviceEntry> = segments
            .iter()
            .filter(|entry| entry.start_addr != start_addr)
            .map(DeviceEntry::cloned)
            .collect();
        candidate.push(DeviceEntry { start_addr, device });

        let new_bus = Self::build(&candidate)?;

        *segments = candidate;
        self.bus.store(Arc::new(new_bus));
        drop(segments);

        Ok(())
    }

    /// Drop every mapping whose start address lies in `[address, address + size)`.
    ///
    /// This is used to service `VFIO_USER_DMA_UNMAP` requests, which Cloud
    /// Hypervisor issues when it tears a device down.
    pub fn remove_range(&self, address: u64, size: u64) -> Result<(), AddBusDeviceError> {
        let mut segments = self.segments.lock().unwrap();
        let end = address.saturating_add(size);

        let candidate: Vec<DeviceEntry> = segments
            .iter()
            .filter(|entry| entry.start_addr < address || entry.start_addr >= end)
            .map(DeviceEntry::cloned)
            .collect();

        let new_bus = Self::build(&candidate)?;

        *segments = candidate;
        self.bus.store(Arc::new(new_bus));
        drop(segments);

        Ok(())
    }

    fn build(segments: &[DeviceEntry]) -> Result<Bus, AddBusDeviceError> {
        let mut bus = Bus::new("DMA bus", u64::MAX);
        for segment in segments {
            bus.add(segment.start_addr, segment.device.clone())?;
        }
        Ok(bus)
    }
}

impl BusDevice for DynamicBus {
    fn size(&self) -> u64 {
        self.bus.load().size()
    }

    fn read(&self, req: Request) -> u64 {
        self.bus.load().read(req)
    }

    fn write(&self, req: Request, value: u64) {
        self.bus.load().write(req, value);
    }

    fn read_bulk(&self, offset: u64, data: &mut [u8]) {
        self.bus.load().read_bulk(offset, data);
    }

    fn write_bulk(&self, offset: u64, data: &[u8]) {
        self.bus.load().write_bulk(offset, data);
    }

    fn compare_exchange_request(&self, req: Request, current: u64, new: u64) -> Result<u64, u64> {
        self.bus.load().compare_exchange_request(req, current, new)
    }
}

#[cfg(test)]
mod tests {
    use crate::device::bus::testutils::TestBusDevice;
    use crate::device::bus::RequestSize;

    use super::*;

    #[test]
    fn can_add_devices() {
        let bus = DynamicBus::default();
        let device1 = Arc::new(TestBusDevice::new(&[42u8; 0x1000]));

        assert_eq!(bus.read(Request::new(0x1000, RequestSize::Size1)), 0xFF);

        bus.add(0x1000, device1).unwrap();
        assert_eq!(bus.read(Request::new(0x1000, RequestSize::Size1)), 42);
    }

    #[test]
    fn overlapping_mapping_fails_and_keeps_the_bus_intact() {
        let bus = DynamicBus::default();
        let device1 = Arc::new(TestBusDevice::new(&[42u8; 0x1000]));
        let device2 = Arc::new(TestBusDevice::new(&[7u8; 0x1000]));

        bus.add(0x1000, device1).unwrap();
        // 0x1800 overlaps the existing 0x1000..0x2000 mapping.
        assert!(bus.add(0x1800, device2).is_err());

        // The failed add must not have changed the existing mapping.
        assert_eq!(bus.read(Request::new(0x1000, RequestSize::Size1)), 42);
    }

    #[test]
    fn upsert_replaces_an_existing_mapping() {
        let bus = DynamicBus::default();
        let device1 = Arc::new(TestBusDevice::new(&[42u8; 0x1000]));
        let device2 = Arc::new(TestBusDevice::new(&[7u8; 0x1000]));

        bus.add(0x1000, device1).unwrap();
        bus.add(0x1000, device2).unwrap();

        assert_eq!(bus.read(Request::new(0x1000, RequestSize::Size1)), 7);
    }

    #[test]
    fn remove_range_drops_covered_mappings() {
        let bus = DynamicBus::default();
        let device1 = Arc::new(TestBusDevice::new(&[42u8; 0x1000]));
        let device2 = Arc::new(TestBusDevice::new(&[1u8; 0x1000]));

        bus.add(0x1000, device1).unwrap();
        bus.add(0x3000, device2).unwrap();

        bus.remove_range(0x1000, 0x1000).unwrap();

        // The removed mapping now reads as unmapped ...
        assert_eq!(bus.read(Request::new(0x1000, RequestSize::Size1)), 0xFF);
        // ... while the other one is untouched.
        assert_eq!(bus.read(Request::new(0x3000, RequestSize::Size1)), 1);
    }
}
