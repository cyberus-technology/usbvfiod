use core::panic;
use std::cmp::Ordering;
use std::{fmt::Debug, future::Future, pin::Pin};

use tracing::{debug, error, info, trace, warn};

use crate::device::xhci::trb::TransferTrb;
use crate::device::{
    bus::BusDeviceRef,
    xhci::{
        hotplug_endpoint_handle::BaseEndpointHandle,
        interrupter::EventSender,
        real_endpoint_handle::{
            ControlRequestProcessingResult, InTrbProcessingResult, OutTrbProcessingResult,
            RealControlEndpointHandle, RealInEndpointHandle, RealOutEndpointHandle,
        },
        trb::{CompletionCode, EventTrb, RawTrb, TransferTrbVariant},
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

#[derive(Debug)]
pub enum ControlTransferStage {
    Initial,
    ConsumedSetupStageTd,
    ConsumedDataStageTd,
    ConsumedStatusStageTrb,
}

// The state machine provides the information partially as ControlSubmissionState::AwaitingControlIn.
// This is used to not modify the state machine.
#[derive(Debug)]
pub enum ControlTransferDirection {
    In(UsbRequest),
    Out(UsbRequest),
}

#[derive(Debug)]
pub struct ControlEndpointHandle<RCEH: RealControlEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    real_ep: RCEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: ControlSubmissionState,
    control_transfer_state: ControlTransferState,
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
            dma_bus,
            event_sender,
            submission_state: ControlSubmissionState::NoTrbSubmitted,
            control_transfer_state: ControlTransferState::new(ControlTransferDirection::In(
                UsbRequest {
                    address: 0,
                    request_type: 0,
                    request: 0,
                    value: 0,
                    index: 0,
                    length: 0,
                    data_pointer: None,
                    data: None,
                },
            )),
        }
    }

    fn handle_setup_stage_trb_pre_hardware(
        &mut self,
        transfer_trb: TransferTrb,
    ) -> anyhow::Result<()> {
        let setup_stage_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::SetupStage(setup_stage_trb_data) => setup_stage_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        let mut request = UsbRequest {
            address: transfer_trb.address,
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

            self.control_transfer_state =
                ControlTransferState::new(ControlTransferDirection::In(request.clone()));

            self.real_ep.submit_control_request(request)?;
            self.submission_state = ControlSubmissionState::AwaitingControlIn(transfer_trb);
        } else {
            trace!("SetupStage TRB with ControlOut");
            request.data = Some(vec![]);

            self.control_transfer_state =
                ControlTransferState::new(ControlTransferDirection::Out(request));

            // actual hardware request happens in status stage after consuming the data stage td

            if setup_stage_trb_data.interrupt_on_completion {
                self.interrupt_on_completion(transfer_trb.address, CompletionCode::Success, false)?;
            }

            self.control_transfer_state.state = ControlTransferStage::ConsumedSetupStageTd;
            self.control_transfer_state.edtla = 0;
            self.submission_state = ControlSubmissionState::ParserConsumedTrb(transfer_trb);
        }

        Ok(())
    }

    fn handle_setup_stage_trb_post_hardware(
        &mut self,
        transfer_trb: TransferTrb,
        hardware_data: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        match &mut self.control_transfer_state.direction {
            // collect hardware data
            ControlTransferDirection::In(request) => {
                debug!("control in data {:?}", hardware_data);

                let setup_stage_trb_data = match &transfer_trb.variant {
                    TransferTrbVariant::SetupStage(setup_stage_trb_data) => setup_stage_trb_data,
                    _ => unreachable!("checked variant before calling this handle"),
                };

                // SAFETY: is always set in the preceding setup stage pre hardware part
                request.data.as_mut().unwrap().append(hardware_data);

                request
                    .data
                    .as_mut()
                    .unwrap()
                    .resize(setup_stage_trb_data.length as usize, 0);
                self.control_transfer_state.previous_completion_code = CompletionCode::Success;

                if setup_stage_trb_data.interrupt_on_completion {
                    self.interrupt_on_completion(
                        transfer_trb.address,
                        CompletionCode::Success,
                        false,
                    )?;
                }

                self.control_transfer_state.state = ControlTransferStage::ConsumedSetupStageTd;
                self.control_transfer_state.edtla = 0;
            }
            ControlTransferDirection::Out(_) => {
                unreachable!(
                    "ControlOut SetupTrb have insufficient information to do the Hardware request"
                )
            }
        }
        Ok(())
    }

    fn handle_data_stage_trb(&mut self, transfer_trb: TransferTrb) -> anyhow::Result<()> {
        let data_stage_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::DataStage(data_stage_trb_data) => data_stage_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        match &mut self.control_transfer_state.direction {
            // Slice the received data to handle each trb.
            ControlTransferDirection::In(usb_request) => {
                trace!("DataStage TRB with ControlIn");

                // All transfers are done but to have the expected value in the
                // created Events we keep count of pretend transfers.
                self.control_transfer_state.edtla += data_stage_trb_data.transfer_length as u64;

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
            // Accumulate in the data buffer to later trigger one ControlOut hardware request.
            ControlTransferDirection::Out(control_out) => {
                trace!("DataStage TRB with ControlOut");

                // No transfer happened yet but to have to expected value in the
                // created Events we keep count of pretend transfers.
                self.control_transfer_state.edtla += data_stage_trb_data.transfer_length as u64;

                let mut byte_slice = vec![0; data_stage_trb_data.transfer_length as usize];
                self.dma_bus
                    .read_bulk(data_stage_trb_data.data_pointer, &mut byte_slice);

                // SAFETY: is always set in the preceding setup stage
                control_out.data.as_mut().unwrap().append(&mut byte_slice);
            }
        }

        if data_stage_trb_data.interrupt_on_completion {
            self.interrupt_on_completion(transfer_trb.address, CompletionCode::Success, false)?;
        }

        if !data_stage_trb_data.chain {
            self.control_transfer_state.state = ControlTransferStage::ConsumedDataStageTd;
            self.control_transfer_state.edtla = 0;
        }
        self.submission_state = ControlSubmissionState::ParserConsumedTrb(transfer_trb);
        Ok(())
    }

    fn handle_status_stage_trb_pre_hardware(
        &mut self,
        transfer_trb: TransferTrb,
    ) -> anyhow::Result<()> {
        let status_stage_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::StatusStage(status_stage_trb_data) => status_stage_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        match &mut self.control_transfer_state.direction {
            ControlTransferDirection::In(_) => {
                trace!("StatusStage TRB with ControlIn");

                if status_stage_trb_data.interrupt_on_completion {
                    self.interrupt_on_completion(
                        transfer_trb.address,
                        CompletionCode::Success,
                        false,
                    )?;
                }

                if status_stage_trb_data.chain {
                    self.control_transfer_state.state =
                        ControlTransferStage::ConsumedStatusStageTrb;
                    // one more EventDataTrb until Control Transfer is done
                } else {
                    self.control_transfer_state.state = ControlTransferStage::Initial;
                    self.control_transfer_state.edtla = 0;
                }
                self.submission_state = ControlSubmissionState::ParserConsumedTrb(transfer_trb);
            }
            ControlTransferDirection::Out(control_out) => {
                trace!("StatusStage TRB with ControlOut");

                self.real_ep.submit_control_request(control_out.clone())?;

                self.submission_state = ControlSubmissionState::AwaitingControlOut(transfer_trb);
            }
        }
        Ok(())
    }

    fn handle_status_stage_trb_post_hardware(
        &mut self,
        transfer_trb: TransferTrb,
    ) -> anyhow::Result<()> {
        let status_stage_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::StatusStage(status_stage_trb_data) => status_stage_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        match &mut self.control_transfer_state.direction {
            ControlTransferDirection::In(_) => {
                unreachable!("ControlIn requests do the Hardware request in the SetupStage")
            }
            ControlTransferDirection::Out(_) => {
                trace!("StatusStage TRB with ControlOut");

                if status_stage_trb_data.chain {
                    self.control_transfer_state.state =
                        ControlTransferStage::ConsumedStatusStageTrb;
                    // one more EventDataTrb until Control Transfer is done
                } else {
                    self.control_transfer_state.state = ControlTransferStage::Initial;
                    self.control_transfer_state.edtla = 0;
                }

                if status_stage_trb_data.interrupt_on_completion {
                    self.interrupt_on_completion(
                        transfer_trb.address,
                        CompletionCode::Success,
                        false,
                    )?;
                }
                Ok(())
            }
        }
    }

    fn handle_event_data_trb(&mut self, transfer_trb: TransferTrb) -> anyhow::Result<()> {
        trace!("EventData TRB");

        let event_data_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::EventData(event_data_trb_data) => event_data_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        // the driver shall set IOC bit on event data trb
        assert!(event_data_trb_data.interrupt_on_completion);

        // edlta is supposed to be a 24 bit value, it being larger is a spec violation we silently drop
        let masked_edtla = (MASK_24BIT & self.control_transfer_state.edtla) as u32;

        let event = EventTrb::new_transfer_event_trb(
            event_data_trb_data.event_data,
            masked_edtla,
            self.control_transfer_state.previous_completion_code,
            true,
            self.endpoint_id,
            self.slot_id,
        );

        self.event_sender.send(event)?;
        self.control_transfer_state.edtla = 0;

        if event_data_trb_data.interrupt_on_completion {
            self.interrupt_on_completion(transfer_trb.address, CompletionCode::Success, false)?;
        }

        if !event_data_trb_data.chain {
            match self.control_transfer_state.state {
                ControlTransferStage::ConsumedSetupStageTd => {
                    self.control_transfer_state.state = ControlTransferStage::ConsumedDataStageTd;
                    self.control_transfer_state.edtla = 0;
                }
                ControlTransferStage::ConsumedDataStageTd => {
                    self.control_transfer_state.state = ControlTransferStage::Initial;
                    self.control_transfer_state.edtla = 0;
                }
                ControlTransferStage::ConsumedStatusStageTrb => {
                    self.control_transfer_state.state = ControlTransferStage::Initial;
                    self.control_transfer_state.edtla = 0;
                }
                _ => {
                    unreachable!("this should never be reached with spec compliancy");
                }
            }
        }

        self.submission_state = ControlSubmissionState::ParserConsumedTrb(transfer_trb);
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
    ParserConsumedTrb(TransferTrb),
    ParserError(RawTrb),
    AwaitingControlIn(TransferTrb),
    AwaitingControlOut(TransferTrb),
}

impl<RCEH: RealControlEndpointHandle> EndpointHandle for ControlEndpointHandle<RCEH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        // a parse error is handled in the match below as unrecognized trb
        let transfer_trb: TransferTrb = TransferTrb {
            address: trb.address,
            variant: TransferTrbVariant::parse(trb.buffer),
        };

        // passing the whole transfer trb to include the trb address and avoid multiple arguments but parsing twice always
        match transfer_trb.variant {
            TransferTrbVariant::SetupStage(_) => {
                self.handle_setup_stage_trb_pre_hardware(transfer_trb)?;
            }
            TransferTrbVariant::DataStage(_) => match &self.control_transfer_state.state {
                ControlTransferStage::ConsumedSetupStageTd => {
                    self.handle_data_stage_trb(transfer_trb)?;
                }
                other_state => {
                    error!(
                        "Data Stage Trb is not allowed in this state: {:?}",
                        other_state
                    );
                    self.control_transfer_state.state = ControlTransferStage::Initial;
                }
            },
            TransferTrbVariant::Normal(_) => {
                match &self.control_transfer_state.state {
                    ControlTransferStage::ConsumedSetupStageTd => {
                        todo!("Normal Trb in a Control Chain");
                        // This path is only Ok when not at the head of the DataStage TD.
                    }
                    other_state => {
                        error!("Normal Trb is not allowed in this state: {:?}", other_state);
                        self.control_transfer_state.state = ControlTransferStage::Initial;
                    }
                }
            }
            TransferTrbVariant::StatusStage(_) => match &self.control_transfer_state.state {
                ControlTransferStage::ConsumedSetupStageTd => {
                    self.handle_status_stage_trb_pre_hardware(transfer_trb)?;
                }
                ControlTransferStage::ConsumedDataStageTd => {
                    self.handle_status_stage_trb_pre_hardware(transfer_trb)?;
                }
                other_state => {
                    error!(
                        "Status Stage Trb is not allowed in this state: {:?}",
                        other_state
                    );
                    self.control_transfer_state.state = ControlTransferStage::Initial;
                }
            },
            TransferTrbVariant::EventData(_) => {
                self.handle_event_data_trb(transfer_trb)?;
            }
            TransferTrbVariant::Unrecognized(_, parse_error) => {
                error!("failed to parse trb on ControlEndpoint: {:?}", parse_error);
                self.submission_state = ControlSubmissionState::ParserError(trb);
            }
            _ => {
                // no action; skip until next setup stage
                warn!(
                    "Noop; unexpected trb in ControlTransfer: {:?}",
                    transfer_trb
                );
            }
        }

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let result = match &self.submission_state {
                ControlSubmissionState::ParserConsumedTrb(transfer_trb) => {
                    trace!("consumed trb at: {}", transfer_trb.address);
                    TrbProcessingResult::Ok
                }
                ControlSubmissionState::ParserError(raw_trb) => {
                    warn!("ControlSubmissionState::ParserError and reporting CompletionCode::TrbError");
                    let event = EventTrb::new_transfer_event_trb(
                        raw_trb.address,
                        0,
                        CompletionCode::TrbError,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(event)?;
                    TrbProcessingResult::TrbError
                }
                ControlSubmissionState::AwaitingControlIn(transfer_trb) => {
                    let processing_result = self.real_ep.next_completion().await?;
                    match processing_result {
                        ControlRequestProcessingResult::SuccessfulControlIn(mut data) => {
                            self.handle_setup_stage_trb_post_hardware(
                                transfer_trb.clone(),
                                &mut data,
                            )?;
                            TrbProcessingResult::Ok
                        }
                        ControlRequestProcessingResult::SuccessfulControlOut => unreachable!(),
                        processing_error => {
                            self.handle_processing_error(processing_error, transfer_trb.address)?
                        }
                    }
                }
                ControlSubmissionState::AwaitingControlOut(transfer_trb) => {
                    let processing_result = self.real_ep.next_completion().await?;
                    match processing_result {
                        ControlRequestProcessingResult::SuccessfulControlIn(_) => {
                            unreachable!()
                        }
                        ControlRequestProcessingResult::SuccessfulControlOut => {
                            self.handle_status_stage_trb_post_hardware(transfer_trb.clone())?;
                            TrbProcessingResult::Ok
                        }
                        processing_error => {
                            self.handle_processing_error(processing_error, transfer_trb.address)?
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

#[derive(Debug)]
pub struct ControlTransferState {
    pub state: ControlTransferStage, // upcoming or current stage of a control transfer to be handled
    pub direction: ControlTransferDirection, // holding the UsbRequest -> all things data
    pub edtla: u64, // transferred bytes counter necessary for event_data_trb handling
    pub previous_completion_code: CompletionCode, // needed for event_data_trb handling
}
impl ControlTransferState {
    // previous_completion_code should never be used as is, thus the error as a default value
    const fn new(direction: ControlTransferDirection) -> Self {
        Self {
            state: ControlTransferStage::Initial,
            direction,
            edtla: 0,
            previous_completion_code: CompletionCode::UndefinedError,
        }
    }
}

#[derive(Debug)]
pub struct NormalTransferParser {
    edtla: u64,
    previous_completion_code: CompletionCode,
    //current_trb_address: Option<u64>,
    //current_trb_data: Option<TransferTrbVariant>,
}
impl NormalTransferParser {
    const fn new() -> Self {
        Self {
            edtla: 0,
            previous_completion_code: CompletionCode::UndefinedError,
        }
    }
}

#[derive(Debug, Default)]
enum NormalSubmissionState {
    #[default]
    NoTrbSubmitted,
    UnsupportedTrbType(RawTrb),
    AwaitingRealTransfer(TransferTrb),
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
    trb_parser: NormalTransferParser,
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
            trb_parser: NormalTransferParser::new(),
        }
    }

    fn handle_normal_trb_pre_hardware(&mut self, transfer_trb: TransferTrb) -> anyhow::Result<()> {
        trace!("handle_normal_trb_pre_hardware Out");

        let normal_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::Normal(normal_trb_data) => normal_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        if !normal_trb_data.chain {
            self.trb_parser = NormalTransferParser::new();
        }

        let mut data = vec![0; normal_trb_data.transfer_length as usize];
        self.dma_bus
            .read_bulk(normal_trb_data.data_pointer, &mut data);
        self.real_ep.submit(data)?;

        self.submission_state = NormalSubmissionState::AwaitingRealTransfer(transfer_trb);
        self.trb_parser.previous_completion_code = CompletionCode::Success;

        Ok(())
    }

    fn handle_normal_trb_post_hardware(&mut self, transfer_trb: TransferTrb) -> anyhow::Result<()> {
        trace!("handle_normal_trb_post_hardware Out");

        let normal_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::Normal(normal_trb_data) => normal_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        self.trb_parser.edtla += normal_trb_data.transfer_length as u64;

        if normal_trb_data.interrupt_on_completion {
            self.interrupt_on_completion(transfer_trb.address, CompletionCode::Success, false)?;
        }

        self.trb_parser.previous_completion_code = CompletionCode::Success;
        Ok(())
    }

    fn handle_event_data_trb(&mut self, transfer_trb: TransferTrb) -> anyhow::Result<()> {
        trace!("EventData TRB");

        let event_data_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::EventData(event_data_trb_data) => event_data_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        // edlta is supposed to be a 24 bit value, it being larger is a spec violation we silently drop
        let masked_edtla = (MASK_24BIT & self.trb_parser.edtla) as u32;

        let event = EventTrb::new_transfer_event_trb(
            event_data_trb_data.event_data,
            masked_edtla,
            self.trb_parser.previous_completion_code,
            true,
            self.endpoint_id,
            self.slot_id,
        );

        self.event_sender.send(event)?;
        self.trb_parser.edtla = 0;

        if event_data_trb_data.interrupt_on_completion {
            self.interrupt_on_completion(transfer_trb.address, CompletionCode::Success, false)?;
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

        let transfer_trb_variant = TransferTrbVariant::parse(trb.buffer);
        let transfer_trb: TransferTrb = TransferTrb {
            address: trb.address,
            variant: transfer_trb_variant,
        };

        match TransferTrbVariant::parse(trb.buffer) {
            TransferTrbVariant::Normal(_) => {
                self.handle_normal_trb_pre_hardware(transfer_trb)?;
            }
            TransferTrbVariant::EventData(_) => {
                self.handle_event_data_trb(transfer_trb)?;
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
            let result = match &self.submission_state {
                NormalSubmissionState::ConsumedEventDataTrb => {
                    trace!(
                        "Slot {} Endpoint {} Consumed Event Data Trb",
                        self.slot_id,
                        self.endpoint_id
                    );
                    TrbProcessingResult::Ok
                }
                NormalSubmissionState::UnsupportedTrbType(ref trb) => {
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
                NormalSubmissionState::AwaitingRealTransfer(transfer_trb) => {
                    match &self.real_ep.next_completion().await? {
                        OutTrbProcessingResult::Disconnect => {
                            warn!("NormalSubmissionState::AwaitingRealTransfer OutTrbProcessingResult::Disconnect");
                            TrbProcessingResult::Disconnect
                        }
                        OutTrbProcessingResult::Stall => {
                            info!("OutTrbProcessingResult::Stall and reporting CompletionCode::StallError");
                            let event = EventTrb::new_transfer_event_trb(
                                transfer_trb.address,
                                0,
                                CompletionCode::StallError,
                                false,
                                self.endpoint_id,
                                self.slot_id,
                            );
                            self.event_sender.send(event)?;

                            TrbProcessingResult::Stall
                        }
                        OutTrbProcessingResult::TransactionError => {
                            warn!("NormalSubmissionState::AwaitingRealTransfer OutTrbProcessingResult::TransactionError");
                            TrbProcessingResult::TransactionError
                        }
                        OutTrbProcessingResult::Success => {
                            self.handle_normal_trb_post_hardware(transfer_trb.clone())?;
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
    real_ep: RIEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: NormalSubmissionState,
    trb_parser: NormalTransferParser,
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
            trb_parser: NormalTransferParser::new(),
        }
    }

    fn handle_normal_trb_pre_hardware(&mut self, transfer_trb: TransferTrb) -> anyhow::Result<()> {
        trace!("handle_normal_trb_pre_hardware In");

        let normal_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::Normal(normal_trb_data) => normal_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        if !normal_trb_data.chain {
            self.trb_parser = NormalTransferParser::new();
        }

        self.real_ep
            .submit(normal_trb_data.transfer_length as usize)?;

        self.submission_state = NormalSubmissionState::AwaitingRealTransfer(transfer_trb);
        self.trb_parser.previous_completion_code = CompletionCode::Success;

        Ok(())
    }

    fn handle_normal_trb_post_hardware(
        &mut self,
        transfer_trb: TransferTrb,
        hardware_data: Vec<u8>,
    ) -> anyhow::Result<()> {
        trace!("handle_normal_trb_post_hardware In");

        let normal_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::Normal(normal_trb_data) => normal_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        let completion_code: CompletionCode;

        let dma_length: usize = match hardware_data
            .len()
            .cmp(&(normal_trb_data.transfer_length as usize))
        {
            Ordering::Less => {
                debug!("received less than requested");
                completion_code = CompletionCode::ShortPacket;
                hardware_data.len()
            }
            Ordering::Equal => {
                debug!("received exactly as requested");
                completion_code = CompletionCode::Success;
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

        self.trb_parser.edtla += dma_length as u64;
        self.dma_bus
            .write_bulk(normal_trb_data.data_pointer, &hardware_data[..dma_length]);

        if normal_trb_data.interrupt_on_completion {
            self.interrupt_on_completion(transfer_trb.address, CompletionCode::Success, false)?;
        }

        self.trb_parser.previous_completion_code = completion_code;

        Ok(())
    }

    fn handle_event_data_trb(&mut self, transfer_trb: TransferTrb) -> anyhow::Result<()> {
        trace!("EventData TRB");

        let event_data_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::EventData(event_data_trb_data) => event_data_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        // edlta is supposed to be a 24 bit value, it being larger is a spec violation we silently drop
        let masked_edtla = MASK_24BIT & self.trb_parser.edtla;

        let event = EventTrb::new_transfer_event_trb(
            event_data_trb_data.event_data,
            masked_edtla as u32,
            self.trb_parser.previous_completion_code,
            true,
            self.endpoint_id,
            self.slot_id,
        );

        self.event_sender.send(event)?;
        self.trb_parser.edtla = 0;

        if event_data_trb_data.interrupt_on_completion {
            self.interrupt_on_completion(transfer_trb.address, CompletionCode::Success, false)?;
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

        let transfer_trb_variant = TransferTrbVariant::parse(trb.buffer);
        let transfer_trb: TransferTrb = TransferTrb {
            address: trb.address,
            variant: transfer_trb_variant,
        };

        match TransferTrbVariant::parse(trb.buffer) {
            TransferTrbVariant::Normal(_) => {
                self.handle_normal_trb_pre_hardware(transfer_trb)?;
            }
            TransferTrbVariant::EventData(_) => {
                self.handle_event_data_trb(transfer_trb)?;
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
            let result = match &self.submission_state {
                NormalSubmissionState::ConsumedEventDataTrb => {
                    trace!(
                        "Slot {} Endpoint {} Consumed Event Data Trb",
                        self.slot_id,
                        self.endpoint_id
                    );
                    TrbProcessingResult::Ok
                }

                NormalSubmissionState::UnsupportedTrbType(ref trb) => {
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
                NormalSubmissionState::AwaitingRealTransfer(transfer_trb) => {
                    debug!("NormalSubmissionState::AwaitingRealTransfer");
                    match self.real_ep.next_completion().await? {
                        InTrbProcessingResult::Disconnect => {
                            warn!("NormalSubmissionState::AwaitingRealTransfer InTrbProcessingResult::Disconnect");
                            TrbProcessingResult::Disconnect
                        }
                        InTrbProcessingResult::Stall => {
                            info!("InTrbProcessingResult::Stall and reporting CompletionCode::StallError");
                            let event = EventTrb::new_transfer_event_trb(
                                transfer_trb.address,
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
                            warn!("NormalSubmissionState::AwaitingRealTransfer InTrbProcessingResult::TransactionError");
                            TrbProcessingResult::TransactionError
                        }
                        InTrbProcessingResult::Success(data) => {
                            self.handle_normal_trb_post_hardware(transfer_trb.clone(), data)?;
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
