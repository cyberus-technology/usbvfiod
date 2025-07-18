use crate::device::bus::BusDeviceRef;

use super::usbrequest::UsbRequest;
use std::fmt::Debug;

pub trait RealDevice: Debug {
    fn control_transfer(&self, request: &UsbRequest, dma_bus: &BusDeviceRef);
}
