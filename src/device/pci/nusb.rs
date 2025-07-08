use tracing::debug;

use super::{
    realdevice::{RealDevice, RealDeviceClone},
    usbrequest::UsbRequest,
};
use std::fmt::Debug;

pub struct NusbDeviceWrapper {
    device: nusb::Device,
}

impl NusbDeviceWrapper {
    pub fn new(device: nusb::Device) -> Self {
        Self { device }
    }
}

impl Debug for NusbDeviceWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("lolusb")
    }
}

impl RealDeviceClone for NusbDeviceWrapper {
    fn clone_box(&self) -> Box<dyn RealDevice> {
        panic!();
    }
}

impl RealDevice for NusbDeviceWrapper {
    fn control_transfer(&self, request: &UsbRequest) {
        debug!("I should now talk to NUSB {:?}", request);
    }
}
