use core::panic;
use std::cmp::Ordering;
use std::{fmt::Debug, future::Future, pin::Pin};

use tracing::debug;
use tracing::trace;
use tracing::warn;

use crate::device::xhci::trb::NormalTrbData;
use crate::device::xhci::trb::{
    DataStageTrbData, EventDataTrbData, SetupStageTrbData, StatusStageTrbData,
};
use crate::device::{
    bus::BusDeviceRef,
    xhci::{
        hotplug_endpoint_handle::BaseEndpointHandle,
        interrupter::EventSender,
        real_endpoint_handle::{
            ControlRequestProcessingResult, InTrbProcessingResult, OutTrbProcessingResult,
            RealControlEndpointHandle, RealInEndpointHandle, RealOutEndpointHandle,
        },
        trb::{CompletionCode, EventTrb, RawTrb, TransferTrb, TransferTrbVariant},
        usbrequest::UsbRequest,
    },
};

pub const MASK_24BIT: u64 = 0xffffff;

pub trait EndpointHandle: BaseEndpointHandle {
    type TrbCompletionFuture<'a>: Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()>;
    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_>;
}

#[derive(Debug, Clone, Copy)]
pub enum TrbProcessingResult {
    Ok,
    Stall,
    TrbError,
    TransactionError,
    Disconnect,
}

pub type DummyEndpointHandle = ();
impl EndpointHandle for DummyEndpointHandle {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, _trb: RawTrb) -> anyhow::Result<()> {
        panic!("should never call functions of dummy endpoint handle");
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        panic!("should never call functions of dummy endpoint handle");
    }
}

impl BaseEndpointHandle for DummyEndpointHandle {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        panic!("should never call functions of dummy endpoint handle");
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        panic!("should never call functions of dummy endpoint handle");
    }
}

#[derive(Debug, PartialEq)]
enum ControlTransferStage {
    None,
    Error,
    SetupStage,
    //SetupStagePostHardware,
    DataStage,
    StatusStage,
    //StatusStagePostHardware,
}

// is this necessary when the state machine provides this?:
// ControlSubmissionState::AwaitingControlIn
// yes because mine is used pre-waiting for hardware and the other is not
#[derive(Debug, PartialEq)]
enum ControlTransferDirection {
    None,
    In(UsbRequest), // could be replaced by UsbRequest::request_type & 0x80 != 0
    Out(UsbRequest),
}

#[derive(Debug)]
pub struct ControlEndpointHandle<RCEH: RealControlEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    real_ep: RCEH,
    //trb_parser: ControlRequestParser,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: ControlSubmissionState,
    //
    state: ControlTransferStage, // know what td of the control chain is done to check if the chain is proper or in error state and needs to be cleaned up to recover
    direction: ControlTransferDirection, // holding the UsbRequest -> all things data
    // talking about residual bytes or transmitted bytes when used from an event data
    edtla: u64, // counter necessary for event_data_trb handling
    previous_completion_code: CompletionCode, // used when for event_data_trb handling
    current_trb_address: Option<u64>, // to have proper address for events to point to
    current_trb_data: Option<TransferTrbVariant>, // for IOC the trb address will be needed and unavailable in post_hardware (when called with next_completion)
}

impl<RCEH: RealControlEndpointHandle> ControlEndpointHandle<RCEH> {
    pub fn new(
        slot_id: u8,
        endpoint_id: u8,
        real_ep: RCEH,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) -> Self {
        Self {
            slot_id,
            endpoint_id,
            real_ep,
            //trb_parser: ControlRequestParser::new(dma_bus.clone()),
            dma_bus,
            event_sender,
            submission_state: ControlSubmissionState::NoTrbSubmitted,
            //
            state: ControlTransferStage::None,
            current_trb_address: None, // have trb fields like address available in next_completion()
            current_trb_data: None,
            previous_completion_code: CompletionCode::UndefinedError, // event data trb need the previous trb completion code
            direction: ControlTransferDirection::None,
            edtla: 0,
        }
    }

    /// return true when ControlIn (Hardware request pending)
    /// return false when ControlOut (reporting OK without actual hardware request)
    fn handle_setup_stage_trb_pre_hardware(
        &mut self,
        address: u64,
        setup_stage_trb_data: &SetupStageTrbData,
    ) -> anyhow::Result<bool> {
        let mut request = UsbRequest {
            address,
            request_type: setup_stage_trb_data.request_type,
            request: setup_stage_trb_data.request,
            value: setup_stage_trb_data.value,
            index: setup_stage_trb_data.index,
            length: setup_stage_trb_data.length,
            data_pointer: None,
            data: Some(vec![]),
        };

        if setup_stage_trb_data.request_type & 0x80 != 0 {
            trace!("SetupStage TRB with ControlIn");

            self.direction = ControlTransferDirection::In(request.clone());

            // trigger hardware data
            self.real_ep.submit_control_request(request)?;
            self.submission_state = ControlSubmissionState::AwaitingControlIn;

            Ok(true)
        } else {
            trace!("SetupStage TRB with ControlOut");
            request.data = Some(vec![]);

            self.direction = ControlTransferDirection::Out(request);

            // actual hardware request happens in status stage after consuming data stage td

            if setup_stage_trb_data.interrupt_on_completion {
                self.interrupt_on_completion(address, CompletionCode::Success, false)?;
            }

            self.state = ControlTransferStage::DataStage;
            self.edtla = 0;
            self.submission_state = ControlSubmissionState::ParserConsumedTrb;

            Ok(false)
        }
    }

    fn handle_setup_stage_trb_post_hardware(
        &mut self,
        hardware_data: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        match &mut self.direction {
            // collect hardware data
            ControlTransferDirection::In(request) => {
                debug!("control in data {:?}", hardware_data);

                let address = self.current_trb_address.unwrap();
                let setup_stage_trb_data = match &self.current_trb_data {
                    Some(TransferTrbVariant::SetupStage(setup_stage_trb_data)) => {
                        setup_stage_trb_data
                    }
                    _ => panic!("TODO should not land here"),
                };

                // SAFETY: TODO wing it
                request.data.as_mut().unwrap().append(hardware_data);
                request
                    .data
                    .as_mut()
                    .unwrap()
                    .resize(setup_stage_trb_data.length as usize, 0);
                self.previous_completion_code = CompletionCode::Success;

                if setup_stage_trb_data.interrupt_on_completion {
                    self.interrupt_on_completion(address, CompletionCode::Success, false)?;
                }

                self.state = ControlTransferStage::DataStage;
                self.edtla = 0;
            }
            ControlTransferDirection::Out(_) => {
                panic!("TODO should not land here")
            }
            _ => panic!("TODO should not land here"),
        }
        Ok(())
    }

    fn handle_data_stage_trb(
        &mut self,
        address: u64,
        data_stage_trb_data: &DataStageTrbData,
    ) -> anyhow::Result<()> {
        match &mut self.direction {
            // slice the data and handle each trb
            ControlTransferDirection::In(usb_request) => {
                trace!("DataStage TRB with ControlIn");

                // All is done but to have to expected value in the Event we keep
                // count of singular pretend transfers.
                self.edtla += data_stage_trb_data.transfer_length as u64;

                let byte_slice: Vec<u8> = usb_request
                    .data
                    .as_mut()
                    .unwrap()
                    .drain(0..data_stage_trb_data.transfer_length.into())
                    .collect();

                trace!(
                    "DataStage TRB len: {} slice: {:?}",
                    byte_slice.len(),
                    byte_slice
                );
                self.dma_bus
                    .write_bulk(data_stage_trb_data.data_pointer, &byte_slice);
            }
            // accumulate data needed for ControlOut from all TRB's of the stage
            ControlTransferDirection::Out(control_out) => {
                trace!("DataStage TRB with ControlOut");

                // All is done but to have to expected value in the Event we keep
                // count of singular pretend transfers.
                self.edtla += data_stage_trb_data.transfer_length as u64;

                let mut byte_slice = vec![0; data_stage_trb_data.transfer_length as usize];
                self.dma_bus
                    .read_bulk(data_stage_trb_data.data_pointer, &mut byte_slice);

                // SAFETY: unwrap safe when system software is driver compliant
                control_out.data.as_mut().unwrap().append(&mut byte_slice);
            }
            ControlTransferDirection::None => {
                panic!("this should never be reached with spec compliancy")
            }
        }

        if data_stage_trb_data.interrupt_on_completion {
            self.interrupt_on_completion(address, CompletionCode::Success, false)?;
        }

        if !data_stage_trb_data.chain {
            self.state = ControlTransferStage::StatusStage;
            self.edtla = 0;
        }
        self.submission_state = ControlSubmissionState::ParserConsumedTrb;
        Ok(())
    }

    // The clone is explicit so the control_worker loop will not error with a
    // reference being double mutably borrowed.
    fn handle_status_stage_trb_pre_hardware(
        &mut self,
        address: u64,
        status_stage_trb_data: &StatusStageTrbData,
    ) -> anyhow::Result<()> {
        match &mut self.direction {
            ControlTransferDirection::In(_) => {
                trace!("StatusStage TRB with ControlIn");

                if status_stage_trb_data.interrupt_on_completion {
                    self.interrupt_on_completion(address, CompletionCode::Success, false)?;
                }

                if !status_stage_trb_data.chain {
                    self.state = ControlTransferStage::None;
                    self.edtla = 0;
                } // else: one event data trb will follow
                self.submission_state = ControlSubmissionState::ParserConsumedTrb;
            }
            ControlTransferDirection::Out(control_out) => {
                trace!("StatusStage TRB with ControlOut");

                self.real_ep.submit_control_request(control_out.clone())?;

                self.submission_state = ControlSubmissionState::AwaitingControlOut;
            }
            ControlTransferDirection::None => {
                panic!("this should never be reached with spec compliancy")
            }
        }
        Ok(())
    }

    // The clone is explicit so the control_worker loop will not error with a
    // reference being double mutably borrowed.
    fn handle_status_stage_trb_post_hardware(&mut self, address: u64) -> anyhow::Result<()> {
        match &mut self.direction {
            ControlTransferDirection::In(_) => {
                trace!("StatusStage TRB with ControlIn");
                panic!("TODO should not land here")
            }
            ControlTransferDirection::Out(_) => {
                trace!("StatusStage TRB with ControlOut");

                // use data from device -> insert into state to use in data stage
                let status_stage_trb_data = match &self.current_trb_data {
                    Some(TransferTrbVariant::StatusStage(status_stage_trb_data)) => {
                        status_stage_trb_data
                    }
                    _ => panic!("TODO should not land here"),
                };

                if !status_stage_trb_data.chain {
                    self.state = ControlTransferStage::None;
                    self.edtla = 0;
                } // else: one event data trb will follow

                if status_stage_trb_data.interrupt_on_completion {
                    self.interrupt_on_completion(address, CompletionCode::Success, false)?;
                }
                Ok(())
            }
            ControlTransferDirection::None => {
                panic!("this should never be reached with spec compliancy")
            }
        }
    }

    fn handle_event_data_trb(
        &mut self,
        address: u64,
        event_data_trb_data: &EventDataTrbData,
    ) -> anyhow::Result<()> {
        trace!("EventData TRB");

        // edlta is supposed to be a 24 bit value, it being larger is a spec violation we silently drop
        let masked_edtla = (MASK_24BIT & self.edtla) as u32;

        let event = EventTrb::new_transfer_event_trb(
            event_data_trb_data.event_data,
            masked_edtla,
            self.previous_completion_code,
            true,
            self.endpoint_id,
            self.slot_id,
        );

        self.event_sender.send(event)?;
        self.edtla = 0;

        if event_data_trb_data.interrupt_on_completion {
            self.interrupt_on_completion(address, CompletionCode::Success, false)?;
        }

        if !event_data_trb_data.chain {
            match self.state {
                ControlTransferStage::DataStage => {
                    self.state = ControlTransferStage::StatusStage;
                    self.edtla = 0;
                }
                ControlTransferStage::StatusStage => {
                    self.state = ControlTransferStage::None;
                    self.edtla = 0;
                }
                _ => {
                    panic!("TODO should not land here")
                }
            }
        }

        self.submission_state = ControlSubmissionState::ParserConsumedTrb;
        Ok(())
    }

    fn interrupt_on_completion(
        &self,
        address: u64,
        completion_code: CompletionCode,
        event_data: bool,
    ) -> anyhow::Result<()> {
        trace!("interrupt_on_completion triggered for address {}", address);
        let event = EventTrb::new_transfer_event_trb(
            address,
            0,
            completion_code,
            event_data,
            self.endpoint_id,
            self.slot_id,
        );

        self.event_sender.send(event)?;
        Ok(())
    }
}

#[derive(Debug, Default)]
enum ControlSubmissionState {
    #[default]
    NoTrbSubmitted,
    ParserConsumedTrb,
    // store address of trb that failed to parse.
    // needs to be specified inside the transfer event indicating the error.
    ParserError,
    AwaitingControlIn,
    AwaitingControlOut,
}

impl<RCEH: RealControlEndpointHandle> EndpointHandle for ControlEndpointHandle<RCEH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        let address = trb.address;
        let transfer_trb = TransferTrbVariant::parse(trb.buffer);

        // to have the trb fields in the next_completion method
        self.current_trb_address = Some(address);
        self.current_trb_data = Some(transfer_trb.clone());

        // function as initialization and reset in case of error state
        if matches!(transfer_trb, TransferTrbVariant::SetupStage(_)) {
            self.state = ControlTransferStage::SetupStage;
        }

        match self.state {
            ControlTransferStage::None => {
                panic!("TODO should not land here")
            }
            ControlTransferStage::Error => {
                todo!("drop trb until we catch a setup stage trb to reset")
            }
            ControlTransferStage::SetupStage => match &transfer_trb {
                TransferTrbVariant::SetupStage(setup_stage_trb_data) => {
                    self.handle_setup_stage_trb_pre_hardware(address, setup_stage_trb_data)?;
                }
                _ => {
                    self.state = ControlTransferStage::Error;
                    self.submission_state = ControlSubmissionState::ParserError;
                    panic!("unexpected Control TRB in SetupStage")
                }
            },
            ControlTransferStage::DataStage => match &transfer_trb {
                TransferTrbVariant::DataStage(data_stage_trb_data) => {
                    self.handle_data_stage_trb(address, data_stage_trb_data)?;
                }
                TransferTrbVariant::Normal(_normal_trb_data) => {
                    todo!()
                    // TODO invalid if first of this state
                }
                TransferTrbVariant::EventData(event_data_trb_data) => {
                    self.handle_event_data_trb(address, event_data_trb_data)?;
                }
                // redirect in case there is no data stage trb
                TransferTrbVariant::StatusStage(status_stage_trb_data) => {
                    self.handle_status_stage_trb_pre_hardware(address, status_stage_trb_data)?;
                }
                _ => {
                    panic!("unexpected Control TRB in DataStage")
                }
            },
            ControlTransferStage::StatusStage => match &transfer_trb {
                TransferTrbVariant::StatusStage(status_stage_trb_data) => {
                    self.handle_status_stage_trb_pre_hardware(address, status_stage_trb_data)?;
                }
                TransferTrbVariant::EventData(event_data_trb_data) => {
                    self.handle_event_data_trb(address, event_data_trb_data)?;
                }
                _ => {
                    panic!("unexpected Control TRB in StatusStage")
                }
            },
        }
        trace!("this trb is done {address}");
        Ok(())

        /*
        >>>>>TBD
        if (self.state == ControlTransferStage::None)
            && matches!(transfer_trb, TransferTrbVariant::SetupStage(_))
        {
            trace!("Control Chain Stage: None -> SetupStage");
            self.state = ControlTransferStage::SetupStage;
            self.edtla = 0;
        } else if self.state == ControlTransferStage::Error {
            trace!(
                "Control Chain Stage: Error, attempting to clean the broken Control Transfer Chain"
            );
        } else if self.state == ControlTransferStage::SetupStage
            && matches!(transfer_trb, TransferTrbVariant::DataStage(_))
        {
            trace!("Control Chain Stage: SetupStage -> DataStage");
            self.state = ControlTransferStage::DataStage;
            self.edtla = 0;
        } else if self.state == ControlTransferStage::SetupStage
            && matches!(transfer_trb, TransferTrbVariant::StatusStage(_))
        {
            trace!("Control Chain Stage: SetupStage -> StatusStage");
            self.state = ControlTransferStage::StatusStage;
            self.edtla = 0;
        } else if self.state == ControlTransferStage::DataStage
            && (matches!(transfer_trb, TransferTrbVariant::DataStage(_))
                || matches!(transfer_trb, TransferTrbVariant::EventData(_)))
        {
            trace!("Control Chain Stage: DataStage -> DataStage");
        } else if self.state == ControlTransferStage::DataStage
            && (matches!(transfer_trb, TransferTrbVariant::StatusStage(_))
                || matches!(transfer_trb, TransferTrbVariant::EventData(_)))
        {
            trace!("Control Chain Stage: DataStage -> StatusStage");
            self.state = ControlTransferStage::StatusStage;
            self.edtla = 0;
        } else if self.state == ControlTransferStage::StatusStage
            && matches!(transfer_trb, TransferTrbVariant::EventData(_))
        {
            trace!("Control Chain Stage: StatusStage -> StatusStage");
        } else {
            error!("wrong order or unexpected {:?}", transfer_trb);
            self.submission_state = ControlSubmissionState::ParserError(trb.address);
        }

        // TODO call handler according to state
        match self.state {
            ControlTransferState::None => {
                panic!("unreachable");
            }
            ControlTransferState::Error => match current_trb {
                TransferTrb {
                    address: _,
                    variant: TransferTrbVariant::DataStage(_),
                } => {
                    trace!("in Error State, skipping data stage trb");
                }
                TransferTrb {
                    address: _,
                    variant: TransferTrbVariant::EventData(_),
                } => {
                    trace!("in Error State, skipping event data trb");
                }
                TransferTrb {
                    address: _,
                    variant: TransferTrbVariant::StatusStage(status_stage_trb_data),
                } => {
                    trace!("in Error State, skipping status stage");

                    // Status Stage Trb is usually last, unless followed by a single event data.
                    if status_stage_trb_data.chain {
                        worker_info.transfer_ring.next_transfer_trb();
                        trace!("in Error State, skipping event data after status stage");
                    }
                    break;
                }
                _ => {
                    todo!()
                }
            },
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
                        &mut previous_completion_code,
                    )
                    .await
                    {
                        Ok(_) => {
                            edtla += 8;
                        }
                        Err(e) => {
                            warn!("some TransferError: {e}");
                            state = ControlTransferState::Error;
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
                        &mut edtla,
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
                        &previous_completion_code,
                    )
                    .await;
                }
                // TODO add normal trb handler
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
                        &mut previous_completion_code,
                    )
                    .await
                    {
                        Ok(_) => {
                            edtla += 8;
                        }
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
                        &previous_completion_code,
                    )
                    .await;

                    // one of two ways to successfully end a valid control chain
                    break;
                }
                _ => panic!("StatusStage: not a StatusStage or EventData TRB"),
            },
        }
        // slice handler to return when the hardware request is submitted

        // <<<<<<<<<<<<<<<<<<<<<<

        let trb_address = trb.address;
        if let ControlFlow::Break(res) = self.trb_parser.trb(trb) {
            match res {
                Ok(request) => {
                    let request_copy = request.clone_without_data();
                    let is_out_request = request.request_type & 0x80 == 0;

                    self.real_ep.submit_control_request(request)?;

                    self.submission_state = match is_out_request {
                        true => ControlSubmissionState::AwaitingControlOut(request_copy),
                        false => ControlSubmissionState::AwaitingControlIn(request_copy),
                    };
                }
                Err(_) => {
                    self.submission_state = ControlSubmissionState::ParserError(trb_address);
                }
            }
        } else {
            self.submission_state = ControlSubmissionState::ParserConsumedTrb;
        }
        */
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let address = self.current_trb_address.unwrap();

            let result = match &self.submission_state {
                ControlSubmissionState::ParserConsumedTrb => TrbProcessingResult::Ok,
                ControlSubmissionState::ParserError => {
                    warn!("ControlSubmissionState::ParserError and reporting CompletionCode::TrbError");
                    let event = EventTrb::new_transfer_event_trb(
                        address,
                        0,
                        CompletionCode::TrbError,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(event)?;
                    TrbProcessingResult::TrbError
                }
                ControlSubmissionState::AwaitingControlIn => {
                    let processing_result = self.real_ep.next_completion().await?;
                    match processing_result {
                        ControlRequestProcessingResult::SuccessfulControlIn(mut data) => {
                            self.handle_setup_stage_trb_post_hardware(&mut data)?;
                            TrbProcessingResult::Ok
                        }
                        ControlRequestProcessingResult::SuccessfulControlOut => unreachable!(),
                        processing_error => {
                            self.handle_processing_error(processing_error, address)?
                        }
                    }
                }
                ControlSubmissionState::AwaitingControlOut => {
                    let processing_result = self.real_ep.next_completion().await?;
                    match processing_result {
                        ControlRequestProcessingResult::SuccessfulControlIn(_) => {
                            unreachable!()
                        }
                        ControlRequestProcessingResult::SuccessfulControlOut => {
                            self.handle_status_stage_trb_post_hardware(address)?;
                            TrbProcessingResult::Ok
                        }
                        processing_error => {
                            self.handle_processing_error(processing_error, address)?
                        }
                    }
                }
                ControlSubmissionState::NoTrbSubmitted => unreachable!(),
            };
            self.submission_state = ControlSubmissionState::NoTrbSubmitted;

            Ok(result)
        })
    }
}

impl<RCEH: RealControlEndpointHandle> BaseEndpointHandle for ControlEndpointHandle<RCEH> {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.cancel().await })
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.clear_halt().await })
    }
}

impl<RCEH: RealControlEndpointHandle> ControlEndpointHandle<RCEH> {
    fn handle_processing_error(
        &self,
        error: ControlRequestProcessingResult,
        request_address: u64,
    ) -> anyhow::Result<TrbProcessingResult> {
        let mapped = match error {
            ControlRequestProcessingResult::Disconnect => {
                // send transaction error event to driver
                // forward disconnect result, so that the hotplugendpointhandle can handle
                warn!("ControlRequestProcessingResult::Disconnect and reporting CompletionCode::UsbTransactionError");
                let event = EventTrb::new_transfer_event_trb(
                    request_address,
                    0,
                    CompletionCode::UsbTransactionError,
                    false,
                    self.endpoint_id,
                    self.slot_id,
                );
                self.event_sender.send(event)?;
                TrbProcessingResult::Disconnect
            }
            ControlRequestProcessingResult::Stall => {
                warn!("ControlRequestProcessingResult::Stall and reporting CompletionCode::StallError");
                let event = EventTrb::new_transfer_event_trb(
                    request_address,
                    0,
                    CompletionCode::StallError,
                    false,
                    self.endpoint_id,
                    self.slot_id,
                );
                self.event_sender.send(event)?;
                TrbProcessingResult::Stall
            }
            ControlRequestProcessingResult::TransactionError => {
                warn!("ControlRequestProcessingResult::TransactionError and reporting CompletionCode::UsbTransactionError");
                let event = EventTrb::new_transfer_event_trb(
                    request_address,
                    0,
                    CompletionCode::UsbTransactionError,
                    false,
                    self.endpoint_id,
                    self.slot_id,
                );
                self.event_sender.send(event)?;
                TrbProcessingResult::TransactionError
            }
            ControlRequestProcessingResult::SuccessfulControlIn(_) => {
                panic!("SuccessfulControlIn should be handled elsewhere")
            }
            ControlRequestProcessingResult::SuccessfulControlOut => {
                panic!("SuccessfulControlOut should be handled elsewhere")
            }
        };
        Ok(mapped)
    }
}

/*
/// Object to hold partial Control Transfer so they can aggregate over their multiple
/// TD/TRB until a single Control request can be submitted
#[derive(Debug)]
struct ControlRequestParser {
    state: ControlRequestParserState,
    dma_bus: BusDeviceRef,
    request_builder: UsbRequest,
    edtla: u64,
}

impl ControlRequestParser {
    fn new(dma_bus: BusDeviceRef) -> Self {
        Self {
            state: ControlRequestParserState::None,
            dma_bus: dma_bus,
            request_builder: Default::default(),
            edtla: 0,
        }
    }
}


#[derive(Debug, PartialEq)]
enum ControlRequestParserState {
    None,
    Error,
    SetupStage,
    DataStage,
    StatusStage,
}

impl ControlRequestParser {
    fn trb(&mut self, trb: RawTrb) -> ControlFlow<Result<UsbRequest, ()>> {
        let transfer_trb = TransferTrbVariant::parse(trb.buffer);

        loop {
            // parsing according to state
            match &self.state {
                ControlRequestParserState::None => {
                    panic!("unreachable");
                }
                ControlRequestParserState::Error => return ControlFlow::Break(Err(())),
                ControlRequestParserState::SetupStage => match transfer_trb {
                    TransferTrbVariant::SetupStage(setup_trb_data) => {
                        let request = UsbRequest {
                            address: 0,
                            request_type: setup_trb_data.request_type,
                            request: setup_trb_data.request,
                            value: setup_trb_data.value,
                            index: setup_trb_data.index,
                            length: setup_trb_data.length,
                            data_pointer: None,
                            data: None,
                        };
                        self.request_builder = request;
                        return ControlFlow::Continue(());
                    }
                    _ => return ControlFlow::Break(Err(())),
                },
                ControlRequestParserState::DataStage => match transfer_trb {
                    TransferTrbVariant::DataStage(data_trb_data) => {
                        let mut data = vec![0; self.request_builder.length as usize];
                        self.dma_bus
                            .read_bulk(data_trb_data.data_pointer, &mut data);

                        self.request_builder.data = Some(data);
                        self.request_builder.data_pointer = Some(data_trb_data.data_pointer);
                        return ControlFlow::Continue(());
                    }
                    TransferTrbVariant::StatusStage(_) => {
                        self.state = ControlRequestParserState::DataStage;
                        continue;
                    }
                    _ => return ControlFlow::Break(Err(())),
                },
                ControlRequestParserState::StatusStage => match transfer_trb {
                    TransferTrbVariant::StatusStage(_) => {
                        self.request_builder.address = trb.address;
                        let request = mem::take(&mut self.request_builder);
                        self.request_builder = UsbRequest::default();
                        self.state = ControlRequestParserState::None;
                        return ControlFlow::Break(Ok(request));
                    }
                    _ => return ControlFlow::Break(Err(())),
                },
            }
        }
    }
}
*/

#[derive(Debug, Default)]
enum NormalSubmissionState {
    #[default]
    NoTrbSubmitted,
    UnsupportedTrbType(RawTrb),
    AwaitingRealTransfer,
    ConsumedEventDataTrb,
}

#[derive(Debug)]
pub struct OutEndpointHandle<ROEH: RealOutEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    real_ep: ROEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: NormalSubmissionState,
    //
    edtla: u64,
    previous_completion_code: CompletionCode, // used when for event_data_trb handling
    current_trb_address: Option<u64>,         // to have proper address for events to point to
    current_trb_data: Option<TransferTrbVariant>, // for IOC the trb address will be needed and unavailable in post_hardware (when called with next_completion)
}

impl<ROEH: RealOutEndpointHandle> OutEndpointHandle<ROEH> {
    pub fn new(
        slot_id: u8,
        endpoint_id: u8,
        real_ep: ROEH,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) -> Self {
        Self {
            slot_id,
            endpoint_id,
            real_ep,
            dma_bus,
            event_sender,
            submission_state: NormalSubmissionState::NoTrbSubmitted,
            //
            edtla: 0,
            previous_completion_code: CompletionCode::UndefinedError,
            current_trb_address: None,
            current_trb_data: None,
        }
    }

    fn handle_normal_trb_pre_hardware(
        &mut self,
        normal_trb_data: &NormalTrbData,
    ) -> anyhow::Result<()> {
        trace!("handle_normal_trb_pre_hardware Out");
        if !normal_trb_data.chain {
            self.edtla = 0;
        }

        let mut data = vec![0; normal_trb_data.transfer_length as usize];
        self.dma_bus
            .read_bulk(normal_trb_data.data_pointer, &mut data);
        self.real_ep.submit(data)?;

        self.submission_state = NormalSubmissionState::AwaitingRealTransfer;
        self.previous_completion_code = CompletionCode::Success;

        Ok(())
    }

    fn handle_normal_trb_post_hardware(&mut self) -> anyhow::Result<()> {
        trace!("handle_normal_trb_post_hardware Out");

        match self.current_trb_data.clone().unwrap() {
            TransferTrbVariant::Normal(normal_trb_data) => {
                self.edtla += normal_trb_data.transfer_length as u64;

                if normal_trb_data.interrupt_on_completion {
                    self.interrupt_on_completion(
                        self.current_trb_address.clone().unwrap(),
                        CompletionCode::Success,
                        false,
                    )?;
                }
            }
            _ => panic!("TODO should never land here"),
        }

        self.previous_completion_code = CompletionCode::Success;
        Ok(())
    }

    fn handle_event_data_trb(
        &mut self,
        address: u64,
        event_data_trb_data: &EventDataTrbData,
    ) -> anyhow::Result<()> {
        trace!("EventData TRB");

        // edlta is supposed to be a 24 bit value, it being larger is a spec violation we silently drop
        let masked_edtla = (MASK_24BIT & self.edtla) as u32;

        let event = EventTrb::new_transfer_event_trb(
            event_data_trb_data.event_data,
            masked_edtla,
            self.previous_completion_code,
            true,
            self.endpoint_id,
            self.slot_id,
        );

        self.event_sender.send(event)?;
        self.edtla = 0;

        if event_data_trb_data.interrupt_on_completion {
            self.interrupt_on_completion(address, CompletionCode::Success, false)?;
        }

        self.submission_state = NormalSubmissionState::ConsumedEventDataTrb;
        Ok(())
    }

    fn interrupt_on_completion(
        &self,
        address: u64,
        completion_code: CompletionCode,
        event_data: bool,
    ) -> anyhow::Result<()> {
        trace!("interrupt_on_completion triggered for address {}", address);
        let event = EventTrb::new_transfer_event_trb(
            address,
            0,
            completion_code,
            event_data,
            self.endpoint_id,
            self.slot_id,
        );

        self.event_sender.send(event)?;
        Ok(())
    }
}

impl<ROEH: RealOutEndpointHandle> EndpointHandle for OutEndpointHandle<ROEH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        assert!(
            matches!(self.submission_state, NormalSubmissionState::NoTrbSubmitted),
            "submit_trb called twice without calling next_completion"
        );

        let transfer_trb = TransferTrbVariant::parse(trb.buffer);
        self.current_trb_address = Some(trb.address);
        self.current_trb_data = Some(transfer_trb.clone());

        match &transfer_trb {
            TransferTrbVariant::Normal(normal_trb_data) => {
                self.handle_normal_trb_pre_hardware(normal_trb_data)?;
            }
            TransferTrbVariant::EventData(event_data_trb_data) => {
                self.handle_event_data_trb(trb.address, event_data_trb_data)?;
            }
            _ => self.submission_state = NormalSubmissionState::UnsupportedTrbType(trb),
        }

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        assert!(
            !matches!(self.submission_state, NormalSubmissionState::NoTrbSubmitted),
            "next_completion called without prior submit_trb"
        );

        Box::pin(async {
            let result = match self.submission_state {
                NormalSubmissionState::ConsumedEventDataTrb => {
                    trace!(
                        "Slot {} Endpoint {} Consumed Event Data Trb",
                        self.slot_id,
                        self.endpoint_id
                    );
                    TrbProcessingResult::Ok
                }
                NormalSubmissionState::UnsupportedTrbType(ref trb) => {
                    warn!("NormalSubmissionState::UnsupportedTrbType and reporting CompletionCode::TrbError");
                    let transfer_event = EventTrb::new_transfer_event_trb(
                        trb.address,
                        0,
                        CompletionCode::TrbError,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(transfer_event)?;

                    TrbProcessingResult::TrbError
                }
                NormalSubmissionState::AwaitingRealTransfer => {
                    match &self.real_ep.next_completion().await? {
                        OutTrbProcessingResult::Disconnect => {
                            warn!("MARKER DC");
                            TrbProcessingResult::Disconnect
                        }
                        OutTrbProcessingResult::Stall => {
                            warn!("MARKER stall");
                            TrbProcessingResult::Stall
                            // TODO handle this for usb 3
                        }
                        OutTrbProcessingResult::TransactionError => {
                            warn!("MARKER transactionerror");
                            TrbProcessingResult::TransactionError
                        }
                        OutTrbProcessingResult::Success => {
                            self.handle_normal_trb_post_hardware()?;
                            TrbProcessingResult::Ok
                        }
                    }
                }
                NormalSubmissionState::NoTrbSubmitted => unreachable!(),
            };
            self.submission_state = NormalSubmissionState::NoTrbSubmitted;

            Ok(result)
        })
    }
}

impl<ROEH: RealOutEndpointHandle> BaseEndpointHandle for OutEndpointHandle<ROEH> {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.cancel().await })
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.clear_halt().await })
    }
}

// >>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>>><

#[derive(Debug)]
pub struct InEndpointHandle<RIEH: RealInEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    real_ep: RIEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: NormalSubmissionState,
    //
    edtla: u64,
    previous_completion_code: CompletionCode, // used when for event_data_trb handling
    current_trb_address: Option<u64>,         // to have proper address for events to point to
    current_trb_data: Option<TransferTrbVariant>, // for IOC the trb address will be needed and unavailable in post_hardware (when called with next_completion)
}

impl<RIEH: RealInEndpointHandle> InEndpointHandle<RIEH> {
    pub fn new(
        slot_id: u8,
        endpoint_id: u8,
        real_ep: RIEH,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) -> Self {
        Self {
            slot_id,
            endpoint_id,
            real_ep,
            dma_bus,
            event_sender,
            submission_state: NormalSubmissionState::NoTrbSubmitted,
            //
            edtla: 0,
            previous_completion_code: CompletionCode::UndefinedError,
            current_trb_address: None,
            current_trb_data: None,
        }
    }

    fn handle_normal_trb_pre_hardware(
        &mut self,
        normal_trb_data: &NormalTrbData,
    ) -> anyhow::Result<()> {
        trace!("handle_normal_trb_pre_hardware In");

        if !normal_trb_data.chain {
            self.edtla = 0;
        }

        self.real_ep
            .submit(normal_trb_data.transfer_length as usize)?;

        self.submission_state = NormalSubmissionState::AwaitingRealTransfer;
        self.previous_completion_code = CompletionCode::Success;

        Ok(())
    }

    fn handle_normal_trb_post_hardware(&mut self, hardware_data: Vec<u8>) -> anyhow::Result<()> {
        trace!("handle_normal_trb_post_hardware In");

        let completion_code: CompletionCode;

        match self.current_trb_data.clone().unwrap() {
            TransferTrbVariant::Normal(normal_trb_data) => {
                let dma_length: usize = match hardware_data
                    .len()
                    .cmp(&(normal_trb_data.transfer_length as usize))
                {
                    Ordering::Less => {
                        completion_code = CompletionCode::ShortPacket;
                        // short packet
                        hardware_data.len()
                    }
                    Ordering::Equal => {
                        completion_code = CompletionCode::Success;
                        // we good with either
                        hardware_data.len()
                    }
                    Ordering::Greater => {
                        warn!("received more than requested");
                        completion_code = CompletionCode::Success;
                        // device responded with more than requested
                        // idk where the overhead goes but we track the requested amount
                        normal_trb_data.transfer_length as usize
                    }
                };

                self.edtla += dma_length as u64;
                self.dma_bus
                    .write_bulk(normal_trb_data.data_pointer, &hardware_data[..dma_length]);

                if normal_trb_data.interrupt_on_completion {
                    self.interrupt_on_completion(
                        self.current_trb_address.clone().unwrap(),
                        CompletionCode::Success,
                        false,
                    )?;
                }
            }
            _ => panic!("TODO should never land here"),
        }

        self.previous_completion_code = completion_code;

        Ok(())
    }

    fn handle_event_data_trb(
        &mut self,
        address: u64,
        event_data_trb_data: &EventDataTrbData,
    ) -> anyhow::Result<()> {
        trace!("EventData TRB");

        // edlta is supposed to be a 24 bit value, it being larger is a spec violation we silently drop
        let masked_edtla = MASK_24BIT & self.edtla;

        let event = EventTrb::new_transfer_event_trb(
            event_data_trb_data.event_data,
            masked_edtla as u32,
            self.previous_completion_code,
            true,
            self.endpoint_id,
            self.slot_id,
        );

        self.event_sender.send(event)?;
        self.edtla = 0;

        if event_data_trb_data.interrupt_on_completion {
            self.interrupt_on_completion(address, CompletionCode::Success, false)?;
        }

        self.submission_state = NormalSubmissionState::ConsumedEventDataTrb;
        Ok(())
    }

    fn interrupt_on_completion(
        &self,
        address: u64,
        completion_code: CompletionCode,
        event_data: bool,
    ) -> anyhow::Result<()> {
        trace!("interrupt_on_completion triggered for address {}", address);
        let event = EventTrb::new_transfer_event_trb(
            address,
            0,
            completion_code,
            event_data,
            self.endpoint_id,
            self.slot_id,
        );

        self.event_sender.send(event)?;
        Ok(())
    }
}

impl<RIEH: RealInEndpointHandle> EndpointHandle for InEndpointHandle<RIEH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        assert!(
            matches!(self.submission_state, NormalSubmissionState::NoTrbSubmitted),
            "submit_trb called twice without calling next_completion"
        );

        let transfer_trb = TransferTrbVariant::parse(trb.buffer);
        self.current_trb_address = Some(trb.address);
        self.current_trb_data = Some(transfer_trb.clone());

        match &transfer_trb {
            TransferTrbVariant::Normal(normal_trb_data) => {
                self.handle_normal_trb_pre_hardware(normal_trb_data)?;
            }
            TransferTrbVariant::EventData(event_data_trb_data) => {
                self.handle_event_data_trb(trb.address, event_data_trb_data)?;
            }
            _ => self.submission_state = NormalSubmissionState::UnsupportedTrbType(trb),
        }

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        assert!(
            !matches!(self.submission_state, NormalSubmissionState::NoTrbSubmitted),
            "next_completion called without prior submit_trb"
        );

        Box::pin(async {
            let result = match self.submission_state {
                NormalSubmissionState::ConsumedEventDataTrb => {
                    trace!(
                        "Slot {} Endpoint {} Consumed Event Data Trb",
                        self.slot_id,
                        self.endpoint_id
                    );
                    TrbProcessingResult::Ok
                }

                NormalSubmissionState::UnsupportedTrbType(ref trb) => {
                    warn!("NormalSubmissionState::UnsupportedTrbType and reporting CompletionCode::TrbError");
                    let transfer_event = EventTrb::new_transfer_event_trb(
                        trb.address,
                        0,
                        CompletionCode::TrbError,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(transfer_event)?;

                    TrbProcessingResult::TrbError
                }
                NormalSubmissionState::AwaitingRealTransfer => {
                    warn!("MARKER NormalSubmissionState::AwaitingRealTransfer");
                    match self.real_ep.next_completion().await? {
                        InTrbProcessingResult::Disconnect => {
                            warn!("MARKER DISCON");
                            TrbProcessingResult::Disconnect
                        }
                        InTrbProcessingResult::Stall => {
                            // TODO handle this for usb 3
                            warn!("MARKER stall");
                            TrbProcessingResult::Stall
                        }
                        InTrbProcessingResult::TransactionError => {
                            warn!("MARKER transactionerror");
                            TrbProcessingResult::TransactionError
                        }
                        InTrbProcessingResult::Success(data) => {
                            // might need more action to handle short packets correctly
                            self.handle_normal_trb_post_hardware(data)?;
                            TrbProcessingResult::Ok
                        }
                    }
                }
                NormalSubmissionState::NoTrbSubmitted => unreachable!(),
            };
            self.submission_state = NormalSubmissionState::NoTrbSubmitted;

            Ok(result)
        })
    }
}

impl<RIEH: RealInEndpointHandle> BaseEndpointHandle for InEndpointHandle<RIEH> {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.cancel().await })
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.clear_halt().await })
    }
}
