use nusb::transfer::{Buffer, Bulk, ControlIn, ControlOut, ControlType, In, Out, Recipient};
use nusb::MaybeFuture;
use tracing::{debug, warn};

use crate::device::bus::BusDeviceRef;
use crate::device::pci::trb::CompletionCode;

use super::trb::{NormalTrbData, TransferTrb, TransferTrbVariant};
use super::{realdevice::RealDevice, usbrequest::UsbRequest};
use std::cmp::Ordering::*;
use std::{
    fmt::Debug,
    sync::atomic::{fence, Ordering},
    time::Duration,
};

pub struct NusbDeviceWrapper {
    device: nusb::Device,
    interface: nusb::Interface,
    ep_in: Option<nusb::Endpoint<Bulk, In>>,
    ep_out: Option<nusb::Endpoint<Bulk, Out>>,
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
    pub fn new(device: nusb::Device) -> Self {
        let interface = device.detach_and_claim_interface(0).wait().unwrap();
        Self {
            device,
            interface,
            ep_in: None,
            ep_out: None,
        }
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

    fn transfer_out(&mut self, trb: &TransferTrb, dma_bus: &BusDeviceRef) -> (CompletionCode, u32) {
        assert!(
            matches!(trb.variant, TransferTrbVariant::Normal(_)),
            "Expected Normal TRB but got {:?}",
            trb
        );

        let ep_out = self.ep_out.as_mut().unwrap();
        let normal_data = extract_normal_trb_data(trb).unwrap();

        let mut data = vec![0; normal_data.transfer_length as usize];
        dma_bus.read_bulk(normal_data.data_pointer, &mut data);
        ep_out.submit(data.into());
        ep_out
            .wait_next_complete(Duration::from_millis(400))
            .unwrap();
        (CompletionCode::Success, 0)
    }

    fn transfer_in(&mut self, trb: &TransferTrb, dma_bus: &BusDeviceRef) -> (CompletionCode, u32) {
        assert!(
            matches!(trb.variant, TransferTrbVariant::Normal(_)),
            "Expected Normal TRB but got {:?}",
            trb
        );

        let ep_in = self.ep_in.as_mut().unwrap();
        let normal_data = extract_normal_trb_data(trb).unwrap();
        let transfer_length = normal_data.transfer_length as usize;

        let buffer_size = determine_buffer_size(transfer_length, ep_in.max_packet_size());
        let buffer = Buffer::new(buffer_size);
        ep_in.submit(buffer);
        let buffer = ep_in
            .wait_next_complete(Duration::from_millis(400))
            .unwrap();
        let byte_count_dma = match buffer.actual_len.cmp(&transfer_length) {
            Greater => {
                // Got more data than requested. We must not write more data than
                // the guest driver requested with the transfer length, otherwise
                // we might write out of the buffer.
                //
                // Why does this case happen? Sometimes the driver asks for, e.g.,
                // 36 bytes. We have to request max_packet_size (e.g., 1024 bytes).
                // The real device then provides 1024 bytes of data (looks like
                // zero padding).
                transfer_length
            }
            Less => {
                // Got less data than requested. That case happens for example when
                // the driver sends a Mode Sense(6) SCSI command. The response size
                // is variable, so the driver asks for 192 bytes but is also fine
                // with less.
                //
                // We copy all the data over that we got.
                // TODO: currently, we just report success and 0 residual bytes,
                // even though we probably should report something like short
                // packet and the difference between requested and actual byte
                // count. We get away with the simplified handling for now.
                // The Mode Sense(6) response encodes the size of the response in
                // the first byte, so the driver is not unhappy that we reported
                // 192 bytes but only deliver, e.g., 36 bytes.
                buffer.actual_len
            }
            Equal => {
                // We got exactly the right amount of bytes.
                transfer_length
            }
        };
        dma_bus.write_bulk(normal_data.data_pointer, &buffer.buffer[..byte_count_dma]);
        (CompletionCode::Success, 0)
    }

    fn enable_endpoint(&mut self, endpoint_id: u8) {
        match endpoint_id {
            3 => {
                if self.ep_in.is_some() {
                    return;
                }
                self.ep_in = Some(self.interface.endpoint::<Bulk, In>(0x81).unwrap());
                debug!("enabled EP3 on real device");
            }
            4 => {
                if self.ep_out.is_some() {
                    return;
                }
                self.ep_out = Some(self.interface.endpoint::<Bulk, Out>(0x2).unwrap());
                debug!("enabled EP4 on real device");
            }
            1 => {}
            _ => todo!(),
        }
    }
}

const fn extract_normal_trb_data(trb: &TransferTrb) -> Option<&NormalTrbData> {
    match &trb.variant {
        TransferTrbVariant::Normal(data) => Some(data),
        _ => None,
    }
}

fn determine_buffer_size(guest_transfer_length: usize, max_packet_size: usize) -> usize {
    if guest_transfer_length < max_packet_size {
        max_packet_size
    } else if guest_transfer_length % max_packet_size == 0 {
        guest_transfer_length
    } else {
        panic!("unexpected IN transfer length {}", guest_transfer_length);
    }
}
