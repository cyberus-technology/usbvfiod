use super::usbrequest::UsbRequest;
use std::fmt::Debug;

pub trait RealDevice: Debug + RealDeviceClone {
    fn control_transfer(&self, request: &UsbRequest);
}

pub trait RealDeviceClone {
    fn clone_box(&self) -> Box<dyn RealDevice>;
}

impl Clone for Box<dyn RealDevice> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}
