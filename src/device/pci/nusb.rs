use nusb::transfer::{Control, ControlType, Recipient};
use tracing::{debug, warn};

use crate::device::bus::BusDeviceRef;

use super::{realdevice::RealDevice, usbrequest::UsbRequest};
use std::{fmt::Debug, time::Duration};

pub struct NusbDeviceWrapper {
    device: nusb::Device,
}

impl Debug for NusbDeviceWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The active configuration is either cached or not available
        // for unconfigured devices. There is no I/O for this.
        f.debug_struct("NusbDeviceWrapper")
            .field("device", &self.device.active_configuration())
            .finish()
    }
}

impl NusbDeviceWrapper {
    pub const fn new(device: nusb::Device) -> Self {
        Self { device }
    }

    fn control_transfer_device_to_host(&self, request: &UsbRequest, dma_bus: &BusDeviceRef) {
        let control = Control {
            control_type: ControlType::Standard,
            recipient: Recipient::Device,
            request: request.request,
            value: request.value,
            index: request.index,
        };

        debug!("sending control in request to device");
        let mut data = vec![0; request.length as usize];
        match self
            .device
            .control_in_blocking(control, &mut data, Duration::from_millis(200))
        {
            Ok(result) => debug!("control in result: {:?}, data {:?}", result, data),
            Err(error) => warn!("control in request failed: {:?}", error),
        }

        // TODO: ideally the control transfer targets the right location for us and we get rid
        // of the additional DMA write here.
        dma_bus.write_bulk(request.data.unwrap(), &data);
    }

    fn control_transfer_host_to_device(&self, request: &UsbRequest, _dma_bus: &BusDeviceRef) {
        let control = Control {
            control_type: ControlType::Standard,
            recipient: Recipient::Device,
            request: request.request,
            value: request.value,
            index: request.index,
        };

        debug!("sending control out request to device");
        let data = if request.data.is_some() {
            todo!("cannot handle control out with data currently")
        } else {
            Vec::new()
        };
        match self
            .device
            .control_out_blocking(control, &data, Duration::from_millis(200))
        {
            Ok(result) => debug!("control out result: {:?}", result),
            Err(error) => warn!("control out request failed: {:?}", error),
        }
    }
}

impl RealDevice for NusbDeviceWrapper {
    fn control_transfer(&self, request: &UsbRequest, dma_bus: &BusDeviceRef) {
        let direction = request.request_type & 0x80 != 0;
        match direction {
            true => self.control_transfer_device_to_host(request, dma_bus),
            false => self.control_transfer_host_to_device(request, dma_bus),
        }
    }
}
