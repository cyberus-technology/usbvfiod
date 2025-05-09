use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use usbvfiod::device::bus::{AddBusDeviceError, Bus, BusDevice, BusDeviceRef, Request};

#[derive(Debug)]
struct DeviceEntry {
    start_addr: u64,
    device: BusDeviceRef,
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

    pub fn add(&self, start_addr: u64, device: BusDeviceRef) -> Result<(), AddBusDeviceError> {
        let mut new_bus = Bus::new("DMA bus", u64::MAX);
        let mut segments = self.segments.lock().unwrap();

        segments.push(DeviceEntry { start_addr, device });

        for segment in segments.iter() {
            new_bus.add(segment.start_addr, segment.device.clone())?;
        }

        // It's okay to use store here, because we only have a single
        // writer (serialized by the mutex).
        self.bus.store(Arc::new(new_bus));

        Ok(())
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
        self.bus.load().write(req, value)
    }

    fn read_bulk(&self, offset: u64, data: &mut [u8]) {
        self.bus.load().read_bulk(offset, data)
    }

    fn write_bulk(&self, offset: u64, data: &[u8]) {
        self.bus.load().write_bulk(offset, data)
    }

    fn compare_exchange_request(&self, req: Request, current: u64, new: u64) -> Result<u64, u64> {
        self.bus.load().compare_exchange_request(req, current, new)
    }
}

#[cfg(test)]
mod tests {
    use usbvfiod::device::bus::RequestSize;

    use super::*;

    #[derive(Debug, Default)]
    struct TestDevice {}

    impl BusDevice for TestDevice {
        fn size(&self) -> u64 {
            0x1000
        }

        fn read(&self, _req: Request) -> u64 {
            42
        }

        fn write(&self, _req: Request, _value: u64) {
            // Ignore
        }
    }

    #[test]
    fn can_add_devices() {
        let bus = DynamicBus::default();
        let device1 = Arc::new(TestDevice::default());

        assert_eq!(bus.read(Request::new(0x1000, RequestSize::Size1)), 0xFF);

        bus.add(0x1000, device1).unwrap();
        assert_eq!(bus.read(Request::new(0x1000, RequestSize::Size1)), 42);
    }
}
