//! Implementation for hardware interactions using the nusb crate.

use anyhow::{Error, Result};
use nusb::transfer::{
    Buffer, Bulk, BulkOrInterrupt, ControlIn, ControlOut, ControlType, In, Interrupt, Out,
    Recipient, TransferError,
};
use nusb::{Interface, MaybeFuture};
use tokio::select;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use crate::async_runtime::runtime;
use crate::device::bus::BusDeviceRef;
use crate::device::pci::trb::{
    CompletionCode, DataStageTrbData, EventDataTrbData, EventTrb, SetupStageTrbData,
    StatusStageTrbData,
};

use super::realdevice::{EndpointType, EndpointWorkerInfo, RealDevice, Speed};
use super::trb::{NormalTrbData, TransferTrb, TransferTrbVariant};
use std::cmp::Ordering::*;
use std::{fmt::Debug, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointMessage {
    Doorbell,
    // XXX remove once we can terminate
    #[allow(dead_code)]
    Terminate,
}

pub struct NusbDeviceWrapper {
    device: nusb::Device,
    interfaces: Vec<nusb::Interface>,
    endpoints: [Option<mpsc::Sender<EndpointMessage>>; 32],
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

impl TryFrom<nusb::Device> for NusbDeviceWrapper {
    type Error = Error;

    fn try_from(device: nusb::Device) -> Result<Self, Error> {
        // Claim all interfaces
        let mut interfaces = vec![];
        let desc = device.active_configuration()?;
        for interface in desc.interfaces() {
            let interface_number = interface.interface_number();
            debug!("Enabling interface {}", interface_number);
            interfaces.push(device.detach_and_claim_interface(interface_number).wait()?);
        }

        Ok(Self {
            device,
            interfaces,
            endpoints: std::array::from_fn(|_| None),
        })
    }
}

impl NusbDeviceWrapper {
    fn get_interface_number_containing_endpoint(&self, endpoint_id: u8) -> Option<usize> {
        self.interfaces.iter().position(|interface| {
            interface
                .descriptor()
                .unwrap()
                .endpoints()
                .any(|ep| ep.address() == endpoint_id)
        })
    }

    fn spawn_endpoint_worker(
        &self,
        endpoint_number: u8,
        endpoint_type: EndpointType,
        worker_info: EndpointWorkerInfo,
        receiver: mpsc::Receiver<EndpointMessage>,
    ) {
        // unwrap can fail when
        // - driver asks for invalid endpoint (driver's fault)
        // - driver switched interfaces to alternate modes, which could
        //   enable endpoint that we are currently not aware of (TODO)
        // In both cases, we cannot reasonably continue and want to see
        // what we encountered, so panicking is the intended behavior.
        let interface_of_endpoint: &Interface = &self.interfaces[self
            .get_interface_number_containing_endpoint(endpoint_number)
            .unwrap()];
        match endpoint_type {
            EndpointType::BulkOut => {
                let endpoint = interface_of_endpoint
                    .endpoint::<Bulk, Out>(endpoint_number)
                    .unwrap();
                runtime().spawn(transfer_out_worker(endpoint, worker_info, receiver));
            }
            EndpointType::BulkIn => {
                let endpoint = interface_of_endpoint
                    .endpoint::<Bulk, In>(endpoint_number)
                    .unwrap();
                runtime().spawn(transfer_in_worker(endpoint, worker_info, receiver));
            }
            EndpointType::InterruptIn => {
                let endpoint = interface_of_endpoint
                    .endpoint::<Interrupt, In>(endpoint_number)
                    .unwrap();
                runtime().spawn(transfer_in_worker(endpoint, worker_info, receiver));
            }
            EndpointType::InterruptOut => {
                let endpoint = interface_of_endpoint
                    .endpoint::<Interrupt, Out>(endpoint_number)
                    .unwrap();
                runtime().spawn(transfer_out_worker(endpoint, worker_info, receiver));
            }
            endpoint_type => {
                todo!(
                    "can not enable endpoint type {:?}; worker not yet implemented",
                    endpoint_type
                )
            }
        }
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

    fn transfer(&mut self, endpoint_id: u8) {
        // transfer requires targeted endpoint to be enabled, panic if not
        match self.endpoints[endpoint_id as usize].as_mut() {
            Some(sender) => {
                trace!("Sending wake up to worker of ep {}", endpoint_id);
                match sender.try_send(EndpointMessage::Doorbell) {
                    Ok(_) => {}
                    Err(e) => warn!("Failed to send doorbell to worker of {endpoint_id}: {e}"),
                }
            }
            None => panic!("transfer for uninitialized endpoint (EP{endpoint_id})"),
        }
    }

    fn enable_endpoint(&mut self, worker_info: EndpointWorkerInfo, endpoint_type: EndpointType) {
        let endpoint_id = worker_info.endpoint_id;
        assert!(
            (1..=31).contains(&endpoint_id),
            "request to enable invalid endpoint id on nusb device. endpoint_id = {endpoint_id}"
        );
        if self.endpoints[endpoint_id as usize].is_some() {
            // endpoint is already enabled.
            //
            // The Linux kernel configures and directly afterwards reconfigures
            // the endpoints (probably due to a very generic configuration
            // implementation), triggering multiple `enable_endpoint` calls.
            return;
        }

        let endpoint_number = endpoint_id / 2;

        // buffer of 10 is arbitrarily chosen. We do not expect messages to queue much at all.
        let (sender, receiver) = mpsc::channel(10);

        match endpoint_type {
            EndpointType::Control => {
                let device = self.device.clone();
                runtime().spawn(control_worker(device, worker_info, receiver));
            }
            endpoint_type => {
                let is_out_endpoint = endpoint_id.is_multiple_of(2);
                match is_out_endpoint {
                    true => {
                        self.spawn_endpoint_worker(
                            endpoint_number,
                            endpoint_type,
                            worker_info,
                            receiver,
                        );
                    }
                    false => {
                        // set directional bit to make it IN
                        let endpoint_number = 0x80 | endpoint_number;

                        self.spawn_endpoint_worker(
                            endpoint_number,
                            endpoint_type,
                            worker_info,
                            receiver,
                        );
                    }
                }
            }
        }
        self.endpoints[endpoint_id as usize] = Some(sender);
        debug!("enabled Endpoint ID/DCI: {} on real device", endpoint_id);
    }
}

async fn handle_setup_stage_trb(
    address: u64,
    setup_stage_trb_data: SetupStageTrbData,
    worker_info: &EndpointWorkerInfo,
    control: &mut ControlTransferDirection<'_>,
    device: &nusb::Device,
    data: &mut Vec<u8>,
) -> Result<(), TransferError> {
    let (recipient, control_type) = extract_recipient_and_type(setup_stage_trb_data.request_type);

    if setup_stage_trb_data.request_type & 0x80 != 0 {
        trace!("SetupStage TRB with ControlIn");
        let control_in = ControlIn {
            control_type,
            recipient,
            request: setup_stage_trb_data.request,
            value: setup_stage_trb_data.value,
            index: setup_stage_trb_data.index,
            length: setup_stage_trb_data.length,
        };

        match device
            .control_in(control_in, Duration::from_millis(200))
            .await
        {
            Ok(mut device_data) => {
                debug!("control in data {:?}", device_data);
                data.append(&mut device_data);

                data.resize(setup_stage_trb_data.length as usize, 0);
            }
            Err(error) => {
                warn!("control in request failed: {:?}", error);
                return Err(error);
            }
        }
        *control = ControlTransferDirection::In;
    } else {
        trace!("SetupStage TRB with ControlOut");
        *control = ControlTransferDirection::Out(ControlOut {
            control_type,
            recipient,
            request: setup_stage_trb_data.request,
            value: setup_stage_trb_data.value,
            index: setup_stage_trb_data.index,
            data: &[],
        });
    }

    if setup_stage_trb_data.interrupt_on_completion {
        interrupt_on_completion(address, CompletionCode::Success, worker_info, false);
    }
    Ok(())
}

async fn handle_data_stage_trb(
    address: u64,
    data_stage_trb_data: DataStageTrbData,
    worker_info: &EndpointWorkerInfo,
    control: &ControlTransferDirection<'_>,
    data: &mut Vec<u8>,
    dma_bus: BusDeviceRef,
) {
    match control {
        // slice the data and handle each trb
        ControlTransferDirection::In => {
            trace!("DataStage TRB with ControlIn");

            let byte_slice: Vec<u8> = data
                .drain(0..data_stage_trb_data.transfer_length.into())
                .collect();

            trace!("DataStage TRB slice: {:?}", byte_slice);
            dma_bus.write_bulk(data_stage_trb_data.data_pointer, &byte_slice);
        }
        // accumulate data needed for ControlOut from all TRB's of the stage
        ControlTransferDirection::Out(_) => {
            trace!("DataStage TRB with ControlOut");
            let mut byte_slice = vec![0; data_stage_trb_data.transfer_length as usize];
            dma_bus.read_bulk(data_stage_trb_data.data_pointer, &mut byte_slice);
            data.append(&mut byte_slice);
        }
        ControlTransferDirection::None => {
            panic!("this should never be reached with spec compliancy")
        }
    }

    if data_stage_trb_data.interrupt_on_completion {
        interrupt_on_completion(address, CompletionCode::Success, worker_info, false);
    }
}

// The clone is explicit so the control_worker loop will not error with a
// reference being double mutably borrowed.
#[allow(clippy::ptr_arg)]
async fn handle_status_stage_trb(
    address: u64,
    status_stage_trb_data: &StatusStageTrbData,
    worker_info: &EndpointWorkerInfo,
    control: &mut ControlTransferDirection<'_>,
    device: &nusb::Device,
    data: &Vec<u8>,
) -> Result<(), TransferError> {
    match control {
        ControlTransferDirection::In => {
            trace!("StatusStage TRB with ControlIn");
            // everything should be done:
            // - nusb transfer is done
            // - data is copied to the mmio buffer
        }
        ControlTransferDirection::Out(control_out) => {
            trace!("StatusStage TRB with ControlOut");
            // TODO do request and get the actual completion code
            // an actual hardware request needs to be done with now accumulated data
            let owned_data = data.clone();

            let tmp_control_out = ControlOut {
                control_type: control_out.control_type,
                recipient: control_out.recipient,
                request: control_out.request,
                value: control_out.value,
                index: control_out.index,
                data: &owned_data,
            };

            match device
                .control_out(tmp_control_out, Duration::from_millis(200))
                .await
            {
                Ok(_) => {
                    debug!("control out success");
                }
                Err(error) => {
                    warn!("control in request failed: {:?}", error);
                    return Err(error);
                }
            }
        }
        ControlTransferDirection::None => {
            panic!("this should never be reached with spec compliancy")
        }
    }

    if status_stage_trb_data.interrupt_on_completion {
        interrupt_on_completion(address, CompletionCode::Success, worker_info, false);
    }
    Ok(())
}

async fn handle_event_data_trb(
    address: u64,
    event_data_trb_data: &EventDataTrbData,
    worker_info: &EndpointWorkerInfo,
    edtla: &mut u64,
) {
    trace!("EventData TRB");

    // TODO get completion code of previous trb
    let completion_code = CompletionCode::Success;

    // TODO use edtla correctly -> does that mean "just" implement remaining bytes?

    let event = EventTrb::new_transfer_event_trb(
        event_data_trb_data.event_data,
        0, // residual bytes are not counted right now
        completion_code,
        true,
        worker_info.endpoint_id,
        worker_info.slot_id,
    );
    worker_info.event_ring.lock().unwrap().enqueue(&event);
    worker_info.interrupt_line.interrupt();

    *edtla = 0;

    if event_data_trb_data.interrupt_on_completion {
        interrupt_on_completion(address, CompletionCode::Success, worker_info, false);
    }
}

fn interrupt_on_completion(
    address: u64,
    completion_code: CompletionCode,
    worker_info: &EndpointWorkerInfo,
    event_data: bool,
) {
    let event = EventTrb::new_transfer_event_trb(
        address,
        0,
        completion_code,
        event_data,
        worker_info.endpoint_id,
        worker_info.slot_id,
    );
    worker_info.event_ring.lock().unwrap().enqueue(&event);
    worker_info.interrupt_line.interrupt();
}

#[derive(PartialEq, Eq)]
enum ControlTransferState {
    None,
    SetupStage,
    DataStage,
    StatusStage,
}
enum ControlTransferDirection<'a> {
    None,
    In,
    Out(ControlOut<'a>),
}

/// See XHCI specification Section 3.2.9 for an overview and further pointers
/// to more documentation (e.g. relevant chapters in the USB 2 and USB 3 specification).
///
/// The description of a "control chain" is in nusb terms a ControlIn or ControlOut
/// Object. The Control Transfer is only valid with multiple Control TRBs in a
/// ordered sequence
// cognitive complexity required because of the high cost of trace! messages
#[allow(clippy::cognitive_complexity)]
async fn control_worker(
    device: nusb::Device,
    worker_info: EndpointWorkerInfo,
    mut receiver: mpsc::Receiver<EndpointMessage>,
) {
    // This workers main loop.
    // Each loop will handle one control (TRB) chain to make one control transfer.
    loop {
        let mut state = ControlTransferState::None;
        let mut current_trb: TransferTrb;

        // A data buffer since nusb does not take individual control request TRB
        // but only a whole chain.
        let mut data: Vec<u8> = vec![];

        // Event Data Transfer Length Accumulator. Refer to section 4.11.5.2 for more information.
        let mut edtla: u64 = 0;

        // This value should always be overwritten by the SetupStage TRB at
        // the beginning of any control chain.
        let mut control = ControlTransferDirection::None;

        loop {
            match worker_info.transfer_ring.next_transfer_trb() {
                None => {
                    trace!(
                        "worker thread ep {}: No TRB on transfer ring, going to sleep",
                        worker_info.endpoint_id
                    );
                    match receiver
                        .recv()
                        .await
                        .expect("The worker channel should never close while the worker is alive.")
                    {
                        EndpointMessage::Doorbell => {
                            trace!(
                                "worker thread ep {}: Received wake up",
                                worker_info.endpoint_id
                            );
                            continue;
                        }
                        EndpointMessage::Terminate => {
                            debug!(
                                "worker thread ep {}: Stopped by terminate message",
                                worker_info.endpoint_id
                            );
                            return;
                        }
                    };
                }
                Some(transfer_trb) => {
                    debug!(
                        "Got a TransferTrb from the TransferRing: {:?}",
                        transfer_trb
                    );
                    current_trb = transfer_trb;
                }
            }

            // A upcoming/to-be-handled TRB can trigger the transition to the next stage.
            // This is also the check for a spec compliant control transfer chain.
            if (state == ControlTransferState::None)
                && matches!(current_trb.variant, TransferTrbVariant::SetupStage(_))
            {
                trace!("Control Chain Stage: None -> SetupStage");
                state = ControlTransferState::SetupStage;
                edtla = 0;
            } else if state == ControlTransferState::SetupStage
                && matches!(current_trb.variant, TransferTrbVariant::DataStage(_))
            {
                trace!("Control Chain Stage: SetupStage -> DataStage");
                state = ControlTransferState::DataStage;
                edtla = 0;
            } else if state == ControlTransferState::SetupStage
                && matches!(current_trb.variant, TransferTrbVariant::StatusStage(_))
            {
                trace!("Control Chain Stage: SetupStage -> StatusStage");
                state = ControlTransferState::StatusStage;
                edtla = 0;
            } else if state == ControlTransferState::DataStage
                && (matches!(current_trb.variant, TransferTrbVariant::DataStage(_))
                    || matches!(current_trb.variant, TransferTrbVariant::EventData(_)))
            {
                trace!("Control Chain Stage: DataStage -> DataStage");
                edtla += 1;
            } else if state == ControlTransferState::DataStage
                && (matches!(current_trb.variant, TransferTrbVariant::StatusStage(_))
                    || matches!(current_trb.variant, TransferTrbVariant::EventData(_)))
            {
                trace!("Control Chain Stage: DataStage -> StatusStage");
                state = ControlTransferState::StatusStage;
                edtla = 0;
            } else if state == ControlTransferState::StatusStage
                && matches!(current_trb.variant, TransferTrbVariant::EventData(_))
            {
                trace!("Control Chain Stage: StatusStage -> StatusStage");
                edtla += 1;
            } else {
                panic!("wrong order or unexpected {:?}", current_trb.variant);
            }

            // Handle the given TRB according to the current stage.
            // This will also prevent handling a TRB in the wrong stage.
            match state {
                ControlTransferState::None => {
                    panic!("We should never get here without recognizing a SetupStage TRB");
                }
                ControlTransferState::SetupStage => match current_trb {
                    TransferTrb {
                        address: _,
                        variant: TransferTrbVariant::SetupStage(setup_stage_trb_data),
                    } => {
                        match handle_setup_stage_trb(
                            current_trb.address,
                            setup_stage_trb_data,
                            &worker_info,
                            &mut control,
                            &device,
                            &mut data,
                        )
                        .await
                        {
                            Ok(_) => {}
                            Err(e) => {
                                todo!("handle the errors in a control chain properly (clear remaining chain from TransferRing) {e}");
                            }
                        }
                    }
                    _ => panic!("SetupStage: not a SetupStage TRB"),
                },
                ControlTransferState::DataStage => match current_trb {
                    TransferTrb {
                        address,
                        variant: TransferTrbVariant::DataStage(data_stage_trb_data),
                    } => {
                        handle_data_stage_trb(
                            address,
                            data_stage_trb_data,
                            &worker_info,
                            &control,
                            &mut data,
                            worker_info.dma_bus.clone(),
                        )
                        .await;
                    }
                    TransferTrb {
                        address,
                        variant: TransferTrbVariant::EventData(event_data_trb_data),
                    } => {
                        handle_event_data_trb(
                            address,
                            &event_data_trb_data,
                            &worker_info,
                            &mut edtla,
                        )
                        .await;
                    }
                    _ => panic!("DataStage: not a DataStage or EventData TRB"),
                },
                ControlTransferState::StatusStage => match current_trb {
                    TransferTrb {
                        address,
                        variant: TransferTrbVariant::StatusStage(status_stage_trb_data),
                    } => {
                        match handle_status_stage_trb(
                            address,
                            &status_stage_trb_data,
                            &worker_info,
                            &mut control,
                            &device,
                            &data,
                        )
                        .await
                        {
                            Ok(_) => {}
                            Err(e) => {
                                todo!("handle the errors in a control chain properly (clear remaining chain from TransferRing) {e}");
                            }
                        }

                        // one of two ways to successfully end a valid control chain
                        if !status_stage_trb_data.chain {
                            break;
                        }
                    }
                    TransferTrb {
                        address,
                        variant: TransferTrbVariant::EventData(event_data_trb_data),
                    } => {
                        handle_event_data_trb(
                            address,
                            &event_data_trb_data,
                            &worker_info,
                            &mut edtla,
                        )
                        .await;

                        // one of two ways to successfully end a valid control chain
                        break;
                    }
                    _ => panic!("StatusStage: not a StatusStage or EventData TRB"),
                },
            }
        }
    }
}

fn extract_recipient_and_type(request_type: u8) -> (Recipient, ControlType) {
    let recipient = match request_type & 0x1f {
        0 => Recipient::Device,
        1 => Recipient::Interface,
        2 => Recipient::Endpoint,
        val => panic!("invalid recipient {val}"),
    };
    let control_type = match (request_type >> 5) & 0x3 {
        0 => ControlType::Standard,
        1 => ControlType::Class,
        2 => ControlType::Vendor,
        val => panic!("invalid type {val}"),
    };
    (recipient, control_type)
}

// cognitive complexity required because of the high cost of trace! messages
#[allow(clippy::cognitive_complexity)]
async fn transfer_in_worker<EpType: BulkOrInterrupt>(
    mut endpoint: nusb::Endpoint<EpType, In>,
    worker_info: EndpointWorkerInfo,
    mut receiver: mpsc::Receiver<EndpointMessage>,
) {
    loop {
        let trb = match worker_info.transfer_ring.next_transfer_trb() {
            Some(trb) => trb,
            None => {
                trace!(
                    "worker thread ep {}: No TRB on transfer ring, going to sleep",
                    worker_info.endpoint_id
                );
                match receiver
                    .recv()
                    .await
                    .expect("The worker channel should never close while the worker is alive.")
                {
                    EndpointMessage::Doorbell => {
                        trace!(
                            "worker thread ep {}: Received wake up",
                            worker_info.endpoint_id
                        );
                        continue;
                    }
                    EndpointMessage::Terminate => {
                        debug!(
                            "worker thread ep {}: Stopped by terminate message",
                            worker_info.endpoint_id
                        );
                        return;
                    }
                };
            }
        };
        assert!(
            matches!(trb.variant, TransferTrbVariant::Normal(_)),
            "Expected Normal TRB but got {trb:?}"
        );

        // The assertion above guarantees that the TRB is a normal TRB. A wrong
        // TRB type is the only reason the unwrap can fail.
        let normal_data = extract_normal_trb_data(&trb).unwrap();
        let transfer_length = normal_data.transfer_length as usize;

        let buffer_size = determine_buffer_size(transfer_length, endpoint.max_packet_size());
        let buffer = Buffer::new(buffer_size);
        endpoint.submit(buffer);
        // doorbell might interrupt us, so we need the loop
        let buffer = loop {
            select! {
                buf = endpoint.next_complete() => break buf,
                msg = receiver.recv() => {
                    match msg.expect("The worker channel should never close while the worker is alive.") {
                        EndpointMessage::Doorbell => {},
                        EndpointMessage::Terminate => return,
                    }
                }
            };
        };
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
async fn transfer_out_worker<EpType: BulkOrInterrupt>(
    mut endpoint: nusb::Endpoint<EpType, Out>,
    worker_info: EndpointWorkerInfo,
    mut receiver: mpsc::Receiver<EndpointMessage>,
) {
    loop {
        let trb = match worker_info.transfer_ring.next_transfer_trb() {
            Some(trb) => trb,
            None => {
                trace!(
                    "worker thread ep {}: No TRB on transfer ring, going to sleep",
                    worker_info.endpoint_id
                );
                match receiver
                    .recv()
                    .await
                    .expect("The worker channel should never close while the worker is alive.")
                {
                    EndpointMessage::Doorbell => {
                        trace!(
                            "worker thread ep {}: Received wake up",
                            worker_info.endpoint_id
                        );
                        continue;
                    }
                    EndpointMessage::Terminate => {
                        debug!(
                            "worker thread ep {}: Stopped by terminate message",
                            worker_info.endpoint_id
                        );
                        return;
                    }
                };
            }
        };
        assert!(
            matches!(trb.variant, TransferTrbVariant::Normal(_)),
            "Expected Normal TRB but got {trb:?}"
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
        endpoint.next_complete().await;

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
