use nusb::transfer::{Control, ControlType, Recipient};
use tracing::debug;

use crate::device::bus::BusDeviceRef;

use super::{
    realdevice::{RealDevice, RealDeviceClone},
    usbrequest::UsbRequest,
};
use std::{fmt::Debug, time::Duration};

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
    fn control_transfer(&self, request: &UsbRequest, dma_bus: &BusDeviceRef) {
        let direction = request.request_type & 0x80 != 0;
        // direction true -> device-to-host, false -> host-to-device
        if direction {
            // device-to-host
            debug!("sending control in request to device");
            let control = Control {
                control_type: ControlType::Standard,
                recipient: Recipient::Device,
                request: request.request,
                value: request.value,
                index: request.index,
            };
            let mut data = vec![0; request.length as usize];
            let result =
                self.device
                    .control_in_blocking(control, &mut data, Duration::from_millis(100));
            debug!("control in result: {:?}, data: {:?}", result, data);
            dma_bus.write_bulk(request.data.unwrap(), &data);
        } else {
            // host-to-device
            debug!("sending control out request to device");
            let control = Control {
                control_type: ControlType::Standard,
                recipient: Recipient::Device,
                request: request.request,
                value: request.value,
                index: request.index,
            };
            let data = if request.data.is_some() {
                panic!("cannot handle control out with data currently")
            } else {
                Vec::new()
            };
            let result =
                self.device
                    .control_out_blocking(control, &data, Duration::from_millis(100));
            debug!("control out result: {:?}", result);
        }
    }
}
