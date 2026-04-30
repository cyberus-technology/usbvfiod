use core::panic;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::io::Write;
use std::{fmt::Debug, future::Future, pin::Pin};

use tracing::trace;
use tracing::warn;
use tracing::{debug, error};

use crate::device::xhci::trb::NormalTrbData;
use crate::device::xhci::trb::{
    DataStageTrbData, EventDataTrbData, SetupStageTrbData, StatusStageTrbData,
};
use crate::device::{
    bus::BusDeviceRef,
    pcap::{self, EndpointPcapMeta},
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
    DataStage,
    StatusStage,
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
    pcap_meta: EndpointPcapMeta,
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
        pcap_meta: EndpointPcapMeta,
        real_ep: RCEH,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) -> Self {
        Self {
            slot_id,
            endpoint_id,
            pcap_meta,
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
            trace!("SetupStage TRB with ControlIn pre hardware");

            self.direction = ControlTransferDirection::In(request.clone());

            // trigger hardware data
            self.real_ep.submit_control_request(request)?;
            self.submission_state = ControlSubmissionState::AwaitingControlIn;

            Ok(true)
        } else {
            trace!("SetupStage TRB with ControlOut pre hardware");
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
                trace!("SetupStage TRB with ControlIn post hardware");
                trace!("ControlIn in data {:?}", hardware_data);

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
                trace!("StatusStage TRB with ControlIn pre hardware");

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
                trace!("StatusStage TRB with ControlOut pre hardware");

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
                trace!("StatusStage TRB with ControlIn post hardware");
                unreachable!("no hardware request should happen at this point");
            }
            ControlTransferDirection::Out(_) => {
                trace!("StatusStage TRB with ControlOut post hardware");

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
        if MASK_24BIT < self.edtla {
            panic!("edlta");
        }

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
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let address = self.current_trb_address.unwrap();

            let result = match &self.submission_state {
                ControlSubmissionState::ParserConsumedTrb => TrbProcessingResult::Ok,
                ControlSubmissionState::ParserError => {
                    pcap::trb_error(self.pcap_meta, address);
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
                    let usb_request = match &self.direction {
                        ControlTransferDirection::In(request) => request,
                        _ => unreachable!(""),
                    };
                    let processing_result = self.real_ep.next_completion().await?;
                    match processing_result {
                        ControlRequestProcessingResult::SuccessfulControlIn(mut data) => {
                            pcap::control_completion_in(self.pcap_meta, usb_request.address, &data);
                            self.handle_setup_stage_trb_post_hardware(&mut data)?;
                            TrbProcessingResult::Ok
                        }
                        ControlRequestProcessingResult::SuccessfulControlOut => unreachable!(),
                        processing_error => {
                            pcap::control_in_error(self.pcap_meta, usb_request, &processing_error);
                            self.handle_processing_error(processing_error, address)?
                        }
                    }
                }
                ControlSubmissionState::AwaitingControlOut => {
                    let usb_request = match &self.direction {
                        ControlTransferDirection::Out(request) => request,
                        _ => unreachable!(""),
                    };
                    let processing_result = self.real_ep.next_completion().await?;
                    match processing_result {
                        ControlRequestProcessingResult::SuccessfulControlIn(_) => {
                            unreachable!()
                        }
                        ControlRequestProcessingResult::SuccessfulControlOut => {
                            pcap::control_completion_out(
                                self.pcap_meta,
                                usb_request.address,
                                u32::from(usb_request.length),
                            );
                            self.handle_status_stage_trb_post_hardware(address)?;
                            TrbProcessingResult::Ok
                        }
                        processing_error => {
                            pcap::control_out_error(self.pcap_meta, usb_request, &processing_error);
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
                warn!("ControlRequestProcessingResult::TransactionError and reporting something");
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

#[derive(Debug, Default)]
enum NormalSubmissionState {
    #[default]
    NoTrbSubmitted,
    UnsupportedTrbType(RawTrb),
    AwaitingRealTransfer,
    ConsumedEventDataTrb,
    ConsumedNormalDataTrb,
}

#[derive(Debug)]
pub struct OutEndpointHandle<ROEH: RealOutEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    pcap_meta: EndpointPcapMeta,
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
        pcap_meta: EndpointPcapMeta,
        real_ep: ROEH,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) -> Self {
        Self {
            slot_id,
            endpoint_id,
            pcap_meta,
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
                error!(
                    "writing normal trb of length {}",
                    normal_trb_data.transfer_length
                );
                if normal_trb_data.interrupt_on_completion {
                    self.interrupt_on_completion(
                        self.current_trb_address.unwrap(),
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
        debug!("data field: {:#x}", event_data_trb_data.event_data);

        // edlta is supposed to be a 24 bit value, it being larger is a spec violation we silently drop
        let masked_edtla = (MASK_24BIT & self.edtla) as u32;
        if MASK_24BIT < self.edtla {
            panic!("edlta");
        }

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
                debug!(
                    "Calling pcap out submission with {:?} and {:?}",
                    normal_trb_data, trb
                );

                let mut data = vec![0; normal_trb_data.transfer_length as usize];
                self.dma_bus
                    .read_bulk(normal_trb_data.data_pointer, &mut data);

                pcap::out_submission(
                    self.pcap_meta,
                    trb.address,
                    &data,
                    normal_trb_data.transfer_length,
                );
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
                NormalSubmissionState::ConsumedNormalDataTrb => {
                    unreachable!("Out endpoints do not use a buffer.");
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
                            pcap::out_error(
                                self.pcap_meta,
                                self.current_trb_address.unwrap(),
                                &OutTrbProcessingResult::Disconnect,
                                &[],
                            );
                            warn!("MARKER DC");
                            TrbProcessingResult::Disconnect
                        }
                        OutTrbProcessingResult::Stall => {
                            pcap::out_error(
                                self.pcap_meta,
                                self.current_trb_address.unwrap(),
                                &OutTrbProcessingResult::Stall,
                                &[],
                            );
                            warn!("MARKER stall");

                            warn!("OutTrbProcessingResult::Stall and reporting CompletionCode::StallError");
                            let event = EventTrb::new_transfer_event_trb(
                                self.current_trb_address.unwrap(),
                                0,
                                CompletionCode::StallError,
                                false,
                                self.endpoint_id,
                                self.slot_id,
                            );
                            self.event_sender.send(event)?;

                            TrbProcessingResult::Stall
                            // TODO handle this for usb 3
                        }
                        OutTrbProcessingResult::TransactionError => {
                            pcap::out_error(
                                self.pcap_meta,
                                self.current_trb_address.unwrap(),
                                &OutTrbProcessingResult::TransactionError,
                                &[],
                            );
                            warn!("MARKER transactionerror");
                            TrbProcessingResult::TransactionError
                        }
                        OutTrbProcessingResult::Success => {
                            self.handle_normal_trb_post_hardware()?;
                            pcap::out_completion(
                                self.pcap_meta,
                                self.current_trb_address.unwrap(),
                                match self.current_trb_data.clone().unwrap() {
                                    TransferTrbVariant::Normal(data) => data.transfer_length,
                                    _ => unreachable!(""),
                                },
                            );
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

#[derive(Debug)]
pub struct InEndpointHandle<RIEH: RealInEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    pcap_meta: EndpointPcapMeta,
    real_ep: RIEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: NormalSubmissionState,
    //
    edtla: u64,
    previous_completion_code: CompletionCode, // used when for event_data_trb handling
    current_trb_address: Option<u64>,         // to have proper address for events to point to
    current_trb_data: Option<TransferTrbVariant>, // for IOC the trb address will be needed and unavailable in post_hardware (when called with next_completion)
    buffer: VecDeque<u8>, // with windows a normal trb chain requested block sizes 411, 4096 and 2144 so the beginning was thrown away while it was needed at the end
}

impl<RIEH: RealInEndpointHandle> InEndpointHandle<RIEH> {
    pub fn new(
        slot_id: u8,
        endpoint_id: u8,
        pcap_meta: EndpointPcapMeta,
        real_ep: RIEH,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) -> Self {
        Self {
            slot_id,
            endpoint_id,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
            submission_state: NormalSubmissionState::NoTrbSubmitted,
            //
            edtla: 0,
            previous_completion_code: CompletionCode::UndefinedError,
            current_trb_address: None,
            current_trb_data: None,
            buffer: VecDeque::new(),
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

        // if buffer has content we might not need to request as much or at all
        if self.buffer.len() > 0 {
            // do not request anything if we already have all in the buffer
            if normal_trb_data.transfer_length as usize == self.buffer.len() {
                // use buffer instead of hardware request and avoid next_completion() fully

                trace!("doing no request because the buffer already has enough data");

                self.edtla += self.buffer.len() as u64;

                let mut returned_data: Vec<u8> = Vec::new();
                // if less than requested the buffer is not used and emptied
                for n in 0..self.buffer.len() {
                    // SAFETY: no safety
                    returned_data.push(self.buffer.pop_front().unwrap());
                }
                self.dma_bus
                    .write_bulk(normal_trb_data.data_pointer, &returned_data);

                if normal_trb_data.interrupt_on_completion {
                    self.interrupt_on_completion(
                        self.current_trb_address.unwrap(),
                        CompletionCode::Success,
                        false,
                    )?;
                }

                trace!("buffer after buffer only dma: {}", self.buffer.len());

                // TODO add data to move the lower pcap stuff back to the state machine match with the rest
                self.submission_state = NormalSubmissionState::ConsumedNormalDataTrb;
                self.previous_completion_code = CompletionCode::Success;

                pcap::in_completion(
                    self.pcap_meta,
                    self.current_trb_address.unwrap(),
                    &returned_data,
                );

                return Ok(());
            }
            // we might still have enough to make a smaller request
            else {
                let mut needed = normal_trb_data.transfer_length as usize - self.buffer.len();
                if (needed % 512 > 0) {
                    needed = ((needed / 512) + 1) * 512;
                }
                self.real_ep.submit(needed)?;
            }
        }
        // no buffer -> full request
        else {
            self.real_ep
                .submit(normal_trb_data.transfer_length as usize)?;
        }

        self.submission_state = NormalSubmissionState::AwaitingRealTransfer;
        self.previous_completion_code = CompletionCode::Success;

        Ok(())
    }

    fn handle_normal_trb_post_hardware(&mut self, hardware_data: Vec<u8>) -> anyhow::Result<()> {
        trace!("handle_normal_trb_post_hardware In");

        let completion_code: CompletionCode;

        // TODO calculation for buffer usage and storage is heavily incomplete and does not yet work
        match self.current_trb_data.clone().unwrap() {
            TransferTrbVariant::Normal(normal_trb_data) => {
                let dma_length: usize = match hardware_data
                    .len()
                    .cmp(&(normal_trb_data.transfer_length as usize))
                {
                    Ordering::Less => {
                        debug!("received less than requested");
                        completion_code = CompletionCode::ShortPacket;
                        // short packet
                        hardware_data.len() + self.buffer.len()
                    }
                    Ordering::Equal => {
                        debug!("received exactly as requested");
                        completion_code = CompletionCode::Success;
                        // we good with either
                        hardware_data.len()
                    }
                    Ordering::Greater => {
                        warn!("received more than requested");
                        completion_code = CompletionCode::Success;
                        // device responded with more than requested
                        // idk where thehandle_normal_trb_pre_hardware overhead goes but we track the requested amount
                        normal_trb_data.transfer_length as usize
                    }
                };

                let mut hardware = VecDeque::from(hardware_data.clone());
                self.buffer.append(&mut hardware);

                self.edtla += dma_length as u64;

                let mut returned_data: Vec<u8> = Vec::new();
                // if less than requested the buffer is not used and emptied
                for n in 0..dma_length {
                    // SAFETY: no safety
                    returned_data.push(self.buffer.pop_front().unwrap());
                }

                self.dma_bus
                    .write_bulk(normal_trb_data.data_pointer, &returned_data);

                if normal_trb_data.interrupt_on_completion {
                    self.interrupt_on_completion(
                        self.current_trb_address.unwrap(),
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
        if MASK_24BIT < self.edtla {
            panic!("edlta");
        }

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

        // TODO this might result in funky business
        //self.buffer.clear();

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
                pcap::in_submission(self.pcap_meta, trb.address, normal_trb_data.transfer_length);
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
                NormalSubmissionState::ConsumedNormalDataTrb => {
                    trace!(
                        "Slot {} Endpoint {} used buffer to consume Normal Data Trb (no actual hardware request was done)",
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
                    debug!("NormalSubmissionState::AwaitingRealTransfer");
                    match self.real_ep.next_completion().await? {
                        InTrbProcessingResult::Disconnect => {
                            pcap::in_error(
                                self.pcap_meta,
                                self.current_trb_address.unwrap(),
                                &InTrbProcessingResult::Disconnect,
                            );
                            warn!("MARKER DISCON");
                            TrbProcessingResult::Disconnect
                        }
                        InTrbProcessingResult::Stall => {
                            pcap::in_error(
                                self.pcap_meta,
                                self.current_trb_address.unwrap(),
                                &InTrbProcessingResult::Stall,
                            );
                            // TODO handle this for usb 3; currently nothing happens
                            warn!("InTrbProcessingResult::Stall and reporting CompletionCode::StallError");
                            let event = EventTrb::new_transfer_event_trb(
                                self.current_trb_address.unwrap(),
                                0,
                                CompletionCode::StallError,
                                false,
                                self.endpoint_id,
                                self.slot_id,
                            );
                            self.event_sender.send(event)?;

                            TrbProcessingResult::Stall
                        }
                        InTrbProcessingResult::TransactionError => {
                            pcap::in_error(
                                self.pcap_meta,
                                self.current_trb_address.unwrap(),
                                &InTrbProcessingResult::Stall,
                            );
                            warn!("MARKER transactionerror");
                            TrbProcessingResult::TransactionError
                        }
                        InTrbProcessingResult::Success(data) => {
                            // might need more action to handle short packets correctly
                            self.handle_normal_trb_post_hardware(data.clone())?;

                            pcap::in_completion(
                                self.pcap_meta,
                                self.current_trb_address.unwrap(),
                                &data,
                            );

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
