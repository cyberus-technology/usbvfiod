use nusb::transfer::{
    Buffer, Bulk, BulkOrInterrupt, ControlIn, ControlOut, ControlType, In, Interrupt, Out,
    Recipient,
};
use nusb::MaybeFuture;
use tracing::{debug, trace, warn};

use crate::device::bus::BusDeviceRef;
use crate::device::pci::trb::{CompletionCode, EventTrb};

use super::realdevice::{EndpointType, EndpointWorkerInfo, Speed};
use super::trb::{NormalTrbData, TransferTrb, TransferTrbVariant};
use super::{realdevice::RealDevice, usbrequest::UsbRequest};
use std::cmp::Ordering::*;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::{
    fmt::Debug,
    sync::atomic::{fence, Ordering},
    time::Duration,
};

pub struct NusbDeviceWrapper {
    device: nusb::Device,
    interfaces: Vec<nusb::Interface>,
    endpoints: [Option<Sender<()>>; 32],
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
        // Claim all interfaces
        let mut interfaces = vec![];
        // when we cannot get the active configuration, i.e., not properly talk
        // to the device, panicking is currently the desired behavior to
        // identify the situation in which the problem occurred.
        let desc = device.active_configuration().unwrap();
        for interface in desc.interfaces() {
            let interface_number = interface.interface_number();
            debug!("Enabling interface {}", interface_number);
            // when we cannot claim an interface of the device, panicking is
            // currently the desired behavior to identify the situation in which
            // the problem occurred.
            interfaces.push(
                device
                    .detach_and_claim_interface(interface_number)
                    .wait()
                    .unwrap(),
            );
        }

        Self {
            device,
            interfaces,
            endpoints: std::array::from_fn(|_| None),
        }
    }

    fn extract_recipient_and_type(request_type: u8) -> (Recipient, ControlType) {
        let recipient = match request_type & 0x1f {
            0 => Recipient::Device,
            1 => Recipient::Interface,
            2 => Recipient::Endpoint,
            val => panic!("invalid recipient {}", val),
        };
        let control_type = match (request_type >> 5) & 0x3 {
            0 => ControlType::Standard,
            1 => ControlType::Class,
            2 => ControlType::Vendor,
            val => panic!("invalid type {}", val),
        };
        (recipient, control_type)
    }

    fn control_transfer_device_to_host(&self, request: &UsbRequest, dma_bus: &BusDeviceRef) {
        let (recipient, control_type) = Self::extract_recipient_and_type(request.request_type);
        let control = ControlIn {
            control_type,
            recipient,
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

    fn control_transfer_host_to_device(&self, request: &UsbRequest, dma_bus: &BusDeviceRef) {
        let data = request.data.map_or_else(Vec::new, |addr| {
            let mut data = vec![0; request.length as usize];
            dma_bus.read_bulk(addr, &mut data);
            data
        });
        let (recipient, control_type) = Self::extract_recipient_and_type(request.request_type);
        let control = ControlOut {
            control_type,
            recipient,
            request: request.request,
            value: request.value,
            index: request.index,
            data: &data,
        };

        debug!("sending control out request to device");
        match self
            .device
            .control_out(control, Duration::from_millis(200))
            .wait()
        {
            Ok(_) => debug!("control out success"),
            Err(error) => warn!("control out request failed: {:?}", error),
        }
    }

    fn get_interface_number_containing_endpoint(&self, endpoint_id: u8) -> Option<usize> {
        self.interfaces.iter().position(|interface| {
            interface
                .descriptor()
                .unwrap()
                .endpoints()
                .any(|ep| ep.address() == endpoint_id)
        })
    }
}

impl From<nusb::Speed> for Speed {
    fn from(value: nusb::Speed) -> Self {
        match value {
            nusb::Speed::Low => Self::Low,
            nusb::Speed::Full => Self::Full,
            nusb::Speed::High => Self::High,
            nusb::Speed::Super => Self::Super,
            nusb::Speed::SuperPlus => Self::SuperPlus,
            _ => todo!("new USB speed was added to non-exhaustive enum"),
        }
    }
}

impl RealDevice for NusbDeviceWrapper {
    fn speed(&self) -> Option<Speed> {
        self.device.speed().map(|speed| speed.into())
    }

    fn control_transfer(&self, request: &UsbRequest, dma_bus: &BusDeviceRef) {
        let direction = request.request_type & 0x80 != 0;
        match direction {
            true => self.control_transfer_device_to_host(request, dma_bus),
            false => self.control_transfer_host_to_device(request, dma_bus),
        }
    }

    fn transfer(&mut self, endpoint_id: u8) {
        // transfer requires targeted endpoint to be enabled, panic if not
        match self.endpoints[endpoint_id as usize].as_mut() {
            // Currently we start an endpoint worker once and never stop it,
            // so sending should never fail. When the worker has panicked, it
            // makes sense for us to panic as well.
            Some(sender) => {
                trace!("Sending wake up to worker of ep {}", endpoint_id);
                sender.send(()).unwrap();
            }
            None => panic!("transfer for uninitialized endpoint (EP{})", endpoint_id),
        };
    }

    fn enable_endpoint(&mut self, worker_info: EndpointWorkerInfo, endpoint_type: EndpointType) {
        let endpoint_id = worker_info.endpoint_id;
        assert!(
            (2..=31).contains(&endpoint_id),
            "request to enable invalid endpoint id on nusb device. endpoint_id = {}",
            endpoint_id
        );
        if self.endpoints[endpoint_id as usize].is_some() {
            // endpoint is already enabled.
            //
            // The Linux kernel configures and directly afterwards reconfigures
            // the endpoints (probably due to a very generic configuration
            // implementation), triggering multiple `enable_endpoint` calls.
            return;
        }

        let endpoint_index = endpoint_id / 2;
        let is_out_endpoint = endpoint_id % 2 == 0;
        let name = format!(
            "worker Slot {} Endpoint {} (EP{} {}, {:?})",
            worker_info.slot_id,
            endpoint_id,
            endpoint_index,
            if is_out_endpoint { "OUT" } else { "IN" },
            endpoint_type,
        );
        let endpoint_sender = match is_out_endpoint {
            true => {
                // unwrap can fail when
                // - driver asks for invalid endpoint (driver's fault)
                // - driver switched interfaces to alternate modes, which could
                //   enable endpoint that we are currently not aware of (TODO)
                // In both cases, we cannot reasonably continue and want to see
                // what we encountered, so panicking is the intended behavior.
                let interface_of_endpoint = &self.interfaces[self
                    .get_interface_number_containing_endpoint(endpoint_index)
                    .unwrap()];
                let endpoint = interface_of_endpoint
                    .endpoint::<Bulk, Out>(endpoint_index)
                    .unwrap();
                let (sender, receiver) = mpsc::channel();
                thread::Builder::new()
                    .name(name.clone())
                    .spawn(move || transfer_out_worker(endpoint, worker_info, receiver))
                    .unwrap_or_else(|_| panic!("Failed to launch endpoint worker thread {name}"));
                sender
            }
            false => {
                let endpoint_index = 0x80 | endpoint_index;
                // unwrap can fail when
                // - driver asks for invalid endpoint (driver's fault)
                // - driver switched interfaces to alternate modes, which could
                //   enable endpoint that we are currently not aware of (TODO)
                // In both cases, we cannot reasonably continue and want to see
                // what we encountered, so panicking is the intended behavior.
                let interface_of_endpoint = &self.interfaces[self
                    .get_interface_number_containing_endpoint(endpoint_index)
                    .unwrap()];
                let (sender, receiver) = mpsc::channel();
                match endpoint_type {
                    EndpointType::BulkIn => {
                        let endpoint = interface_of_endpoint
                            .endpoint::<Bulk, In>(endpoint_index)
                            .unwrap();
                        thread::Builder::new()
                            .name(name.clone())
                            .spawn(move || {
                                transfer_in_worker::<Bulk>(endpoint, worker_info, receiver)
                            })
                            .unwrap_or_else(|_| {
                                panic!("Failed to launch endpoint worker thread {name}")
                            });
                    }
                    EndpointType::InterruptIn => {
                        let endpoint = interface_of_endpoint
                            .endpoint::<Interrupt, In>(endpoint_index)
                            .unwrap();
                        thread::Builder::new()
                            .name(name.clone())
                            .spawn(move || {
                                transfer_in_worker::<Interrupt>(endpoint, worker_info, receiver)
                            })
                            .unwrap_or_else(|_| {
                                panic!("Failed to launch endpoint worker thread {name}")
                            });
                    }
                    _ => {
                        panic!(
                            "Unexpected endpoint type for IN endpoint: {:?}",
                            endpoint_type
                        );
                    }
                }
                sender
            }
        };
        self.endpoints[endpoint_id as usize] = Some(endpoint_sender);
        debug!("enabled EP{} on real device", endpoint_id);
    }
}

// cognitive complexity required because of the high cost of trace! messages
#[allow(clippy::cognitive_complexity)]
fn transfer_in_worker<EpType: BulkOrInterrupt>(
    mut endpoint: nusb::Endpoint<EpType, In>,
    worker_info: EndpointWorkerInfo,
    wakeup: Receiver<()>,
) {
    loop {
        let trb = match worker_info.transfer_ring.next_transfer_trb() {
            Some(trb) => trb,
            None => {
                trace!(
                    "worker thread ep {}: No TRB on transfer ring, going to sleep",
                    worker_info.endpoint_id
                );
                // We currently assume that the main thread always keeps the
                // channel open, so unwrap is safe.
                wakeup.recv().unwrap();
                trace!(
                    "worker thread ep {}: Received wake up",
                    worker_info.endpoint_id
                );
                continue;
            }
        };
        assert!(
            matches!(trb.variant, TransferTrbVariant::Normal(_)),
            "Expected Normal TRB but got {:?}",
            trb
        );

        // The assertion above guarantees that the TRB is a normal TRB. A wrong
        // TRB type is the only reason the unwrap can fail.
        let normal_data = extract_normal_trb_data(&trb).unwrap();
        let transfer_length = normal_data.transfer_length as usize;

        let buffer_size = determine_buffer_size(transfer_length, endpoint.max_packet_size());
        let buffer = Buffer::new(buffer_size);
        endpoint.submit(buffer);
        // We do not want to time out on requests. We should probably use async
        // because nusb supports either async requests or synchronous variants
        // with timeouts. Manually implementing polling seems overkill here.
        let buffer = endpoint.wait_next_complete(Duration::MAX).unwrap();
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
        worker_info
            .dma_bus
            .write_bulk(normal_data.data_pointer, &buffer.buffer[..byte_count_dma]);

        if !normal_data.interrupt_on_completion {
            trace!("Processed TRB without IOC flag; sending no transfer event");
            continue;
        }

        let (completion_code, residual_bytes) = (CompletionCode::Success, 0);

        let transfer_event = EventTrb::new_transfer_event_trb(
            trb.address,
            residual_bytes,
            completion_code,
            false,
            worker_info.endpoint_id,
            worker_info.slot_id,
        );
        // Mutex lock unwrap fails only if other threads panicked while holding
        // the lock. In that case it is reasonable we also panic.
        worker_info
            .event_ring
            .lock()
            .unwrap()
            .enqueue(&transfer_event);
        worker_info.interrupt_line.interrupt();
        debug!("sent Transfer Event and signaled interrupt");
    }
}

// cognitive complexity required because of the high cost of trace! messages
#[allow(clippy::cognitive_complexity)]
fn transfer_out_worker(
    mut endpoint: nusb::Endpoint<Bulk, Out>,
    worker_info: EndpointWorkerInfo,
    wakeup: Receiver<()>,
) {
    loop {
        let trb = match worker_info.transfer_ring.next_transfer_trb() {
            Some(trb) => trb,
            None => {
                trace!(
                    "worker thread ep {}: No TRB on transfer ring, going to sleep",
                    worker_info.endpoint_id
                );
                // We currently assume that the main thread always keeps the
                // channel open, so unwrap is safe.
                wakeup.recv().unwrap();
                trace!(
                    "worker thread ep {}: Received wake up",
                    worker_info.endpoint_id
                );
                continue;
            }
        };
        assert!(
            matches!(trb.variant, TransferTrbVariant::Normal(_)),
            "Expected Normal TRB but got {:?}",
            trb
        );

        // The assertion above guarantees that the TRB is a normal TRB. A wrong
        // TRB type is the only reason the unwrap can fail.
        let normal_data = extract_normal_trb_data(&trb).unwrap();

        let mut data = vec![0; normal_data.transfer_length as usize];
        worker_info
            .dma_bus
            .read_bulk(normal_data.data_pointer, &mut data);
        if normal_data.transfer_length == 31 {
            debug!("OUT data: {:?}", data);
        }
        endpoint.submit(data.into());
        // Timeout indicates device unresponsive - no reasonable recovery possible
        endpoint.wait_next_complete(Duration::MAX).unwrap();

        if !normal_data.interrupt_on_completion {
            trace!("Processed TRB without IOC flag; sending no transfer event");
            continue;
        }

        let (completion_code, residual_bytes) = (CompletionCode::Success, 0);

        let transfer_event = EventTrb::new_transfer_event_trb(
            trb.address,
            residual_bytes,
            completion_code,
            false,
            worker_info.endpoint_id,
            worker_info.slot_id,
        );
        // Mutex lock unwrap fails only if other threads panicked while holding
        // the lock. In that case it is reasonable we also panic.
        worker_info
            .event_ring
            .lock()
            .unwrap()
            .enqueue(&transfer_event);
        worker_info.interrupt_line.interrupt();
        debug!("sent Transfer Event and signaled interrupt");
    }
}

const fn extract_normal_trb_data(trb: &TransferTrb) -> Option<&NormalTrbData> {
    match &trb.variant {
        TransferTrbVariant::Normal(data) => Some(data),
        _ => None,
    }
}

const fn determine_buffer_size(guest_transfer_length: usize, max_packet_size: usize) -> usize {
    if guest_transfer_length <= max_packet_size {
        max_packet_size
    } else {
        guest_transfer_length.div_ceil(max_packet_size) * max_packet_size
    }
}
