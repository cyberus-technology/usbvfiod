use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient};
use nusb::MaybeFuture;
use tracing::{debug, warn};

use crate::device::bus::BusDeviceRef;

use super::trb::{CompletionCode, EventTrb, TransferTrb};
use super::{realdevice::RealDevice, usbrequest::UsbRequest};
use std::{
    fmt::Debug,
    sync::atomic::{fence, Ordering},
    time::Duration,
};

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
        let control = ControlIn {
            control_type: ControlType::Standard,
            recipient: Recipient::Device,
            request: request.request,
            value: request.value,
            index: request.index,
            length: request.length,
        };

        debug!("sending control in request to device");
        let data = match self
            .device
            .control_in(control, Duration::from_millis(200))
            .wait()
        {
            Ok(data) => {
                debug!("control in data {:?}", data);
                data
            }
            Err(error) => {
                warn!("control in request failed: {:?}", error);
                vec![0; 0]
            }
        };

        // TODO: ideally the control transfer targets the right location for us and we get rid
        // of the additional DMA write here.
        dma_bus.write_bulk(request.data.unwrap(), &data);

        // Ensure the data copy to guest memory completes before the subsequent
        // transfer event write completes.
        fence(Ordering::Release);
    }

    fn control_transfer_host_to_device(&self, request: &UsbRequest, _dma_bus: &BusDeviceRef) {
        let data = Vec::new();
        let control = ControlOut {
            control_type: ControlType::Standard,
            recipient: Recipient::Device,
            request: request.request,
            value: request.value,
            index: request.index,
            data: &data,
        };

        debug!("sending control out request to device");
        if request.data.is_some() {
            todo!("cannot handle control out with data currently")
        };
        match self
            .device
            .control_out(control, Duration::from_millis(200))
            .wait()
        {
            Ok(_) => debug!("control out success"),
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

    fn out(&mut self, trb: &TransferTrb, dma_bus: &BusDeviceRef) -> (CompletionCode, u32) {
        todo!();
    }

    fn in_(&mut self, trb: &TransferTrb, dma_bus: &BusDeviceRef) -> (CompletionCode, u32) {
        todo!();
    }

    fn enable_endpoint(&mut self, endpoint_id: u8) {
        todo!();
    }
}
