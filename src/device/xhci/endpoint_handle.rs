use core::panic;
use std::{cmp::Ordering, fmt::Debug, future::Future, pin::Pin};

use tracing::{debug, error, info, trace, warn};

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

pub const MAX_VALUE_U24: u64 = 0xffffff;

pub trait EndpointHandle: BaseEndpointHandle {
    type TrbCompletionFuture<'a>: Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()>;
    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

fn interrupt_on_completion(
    address: u64,
    completion_code: CompletionCode,
    event_data: bool,
    endpoint_id: u8,
    slot_id: u8,
    event_sender: &EventSender,
) -> anyhow::Result<()> {
    trace!("interrupt_on_completion triggered for address {}", address);
    let event = EventTrb::new_transfer_event_trb(
        address,
        0,
        completion_code,
        event_data,
        endpoint_id,
        slot_id,
    );

    event_sender.send(event)?;
    Ok(())
}

// Track how far we are with parsing the Control Transfer (chain of TRB).
#[derive(Debug, PartialEq, Eq)]
pub enum ControlTransferStage {
    /// Nothing happened yet. Awaiting a Setup Stage Trb and silently dropping any other.
    Initial,
    /// Collected Information for the USB Control Request.
    ConsumedSetupStageTd,
    /// Finished all necessary dma operations.
    ConsumedDataStageTd,
    /// Status Stage TRB had a chain bit and there will be one Event Data Trb following to finish the Control Transfer.
    ConsumedStatusStageTrb,
}

/// The state machine provides the information partially as ControlSubmissionState::AwaitingControlIn(TransferTrb).
/// Track state between us and the guest for building the current control request.
#[derive(Debug, PartialEq, Eq)]
pub enum ControlTransferDirection {
    In(UsbRequest),
    Out(UsbRequest),
}

#[derive(Debug)]
pub struct ControlEndpointHandle<RCEH: RealControlEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    pcap_meta: EndpointPcapMeta,
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

        let mut usb_request = UsbRequest {
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
                ControlTransferState::new(ControlTransferDirection::In(usb_request.clone()));

            self.real_ep.submit_control_request(usb_request.clone())?;
            pcap::control_submission(self.pcap_meta, &usb_request);

            self.submission_state = ControlSubmissionState::AwaitingControlIn(transfer_trb);
        } else {
            trace!("SetupStage TRB with ControlOut");
            usb_request.data = Some(vec![]);

            self.control_transfer_state =
                ControlTransferState::new(ControlTransferDirection::Out(usb_request));

            // actual hardware request happens in status stage after consuming the data stage td

            if setup_stage_trb_data.interrupt_on_completion {
                interrupt_on_completion(
                    transfer_trb.address,
                    CompletionCode::Success,
                    false,
                    self.endpoint_id,
                    self.slot_id,
                    &self.event_sender,
                )?;
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

                pcap::control_completion_in(self.pcap_meta, request.address, hardware_data);

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
                    interrupt_on_completion(
                        transfer_trb.address,
                        CompletionCode::Success,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                        &self.event_sender,
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
                self.control_transfer_state.edtla = self
                    .control_transfer_state
                    .edtla
                    .wrapping_add(data_stage_trb_data.transfer_length as u64);

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
                self.control_transfer_state.edtla = self
                    .control_transfer_state
                    .edtla
                    .wrapping_add(data_stage_trb_data.transfer_length as u64);

                if data_stage_trb_data.immediate_data {
                    // Only event data should follow when immediate data is used here
                    // but there is currently no check for that.

                    // SAFETY: is always set in the preceding setup stage
                    control_out.data.as_mut().unwrap().append(
                        &mut data_stage_trb_data.data_pointer.to_le_bytes()
                            [..data_stage_trb_data.transfer_length as usize]
                            .to_vec(),
                    );
                } else {
                    let mut tmp = vec![0u8; data_stage_trb_data.transfer_length as usize];
                    self.dma_bus
                        .read_bulk(data_stage_trb_data.data_pointer, &mut tmp);

                    // SAFETY: is always set in the preceding setup stage
                    control_out.data.as_mut().unwrap().append(&mut tmp);
                }
            }
        }

        if data_stage_trb_data.interrupt_on_completion {
            interrupt_on_completion(
                transfer_trb.address,
                CompletionCode::Success,
                false,
                self.endpoint_id,
                self.slot_id,
                &self.event_sender,
            )?;
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
                    interrupt_on_completion(
                        transfer_trb.address,
                        CompletionCode::Success,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                        &self.event_sender,
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
            ControlTransferDirection::Out(usb_request_out) => {
                trace!("StatusStage TRB with ControlOut");

                self.real_ep
                    .submit_control_request(usb_request_out.clone())?;
                pcap::control_submission(self.pcap_meta, usb_request_out);

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
            ControlTransferDirection::Out(usb_request) => {
                trace!("StatusStage TRB with ControlOut");

                self.control_transfer_state.previous_completion_code = CompletionCode::Success;

                pcap::control_completion_out(
                    self.pcap_meta,
                    usb_request.address,
                    u32::from(usb_request.length),
                );

                if status_stage_trb_data.chain {
                    self.control_transfer_state.state =
                        ControlTransferStage::ConsumedStatusStageTrb;
                    // one more EventDataTrb until Control Transfer is done
                } else {
                    self.control_transfer_state.state = ControlTransferStage::Initial;
                    self.control_transfer_state.edtla = 0;
                }

                if status_stage_trb_data.interrupt_on_completion {
                    interrupt_on_completion(
                        transfer_trb.address,
                        CompletionCode::Success,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                        &self.event_sender,
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

        // edlta is supposed to be a 24 bit value, it being larger is a spec violation we silently drop
        let masked_edtla = (MAX_VALUE_U24 & self.control_transfer_state.edtla) as u32;

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

        // According to the spec Event Data shall always have the IOC bit set.
        // It was not clear from the specification alone if the IOC bit is
        // actually intended for the above event or for this separate/additional one.
        if event_data_trb_data.interrupt_on_completion {
            interrupt_on_completion(
                transfer_trb.address,
                CompletionCode::Success,
                false,
                self.endpoint_id,
                self.slot_id,
                &self.event_sender,
            )?;
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
}

/// Track communication between us and the host hardware.
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
                    pcap::trb_error(self.pcap_meta, raw_trb.address); // TODO should this reference the setup trb address?
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
                            let usb_request = match &self.control_transfer_state.direction {
                                ControlTransferDirection::In(usb_request) => usb_request,
                                _ => panic!("TODO write a message or check if it is unreachable"),
                            };
                            pcap::control_in_error(self.pcap_meta, usb_request, &processing_error);
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
                            let usb_request = match &self.control_transfer_state.direction {
                                ControlTransferDirection::Out(usb_request) => usb_request,
                                _ => panic!("TODO write a message or check if it is unreachable"),
                            };
                            pcap::control_out_error(self.pcap_meta, usb_request, &processing_error);
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

#[derive(Debug, PartialEq, Eq)]
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

fn handle_event_data_trb_normal_ep(
    transfer_trb: &TransferTrb,
    normal_transfer_state: &mut EventDataTrbMetadata,
    endpoint_id: u8,
    slot_id: u8,
    event_sender: &EventSender,
) -> anyhow::Result<()> {
    trace!("EventData TRB");

    let event_data_trb_data = match &transfer_trb.variant {
        TransferTrbVariant::EventData(event_data_trb_data) => event_data_trb_data,
        _ => unreachable!("checked variant before calling this handle"),
    };

    // edlta is supposed to be a 24 bit value, it being larger is a spec violation we silently drop
    let masked_edtla = (MAX_VALUE_U24 & normal_transfer_state.edtla) as u32;

    let event = EventTrb::new_transfer_event_trb(
        event_data_trb_data.event_data,
        masked_edtla,
        normal_transfer_state.previous_completion_code,
        true,
        endpoint_id,
        slot_id,
    );

    event_sender.send(event)?;
    normal_transfer_state.edtla = 0;

    // It was not clear from the specification alone if the IOC bit is
    // actually intended for the above event or as this separate one.
    if event_data_trb_data.interrupt_on_completion {
        interrupt_on_completion(
            transfer_trb.address,
            CompletionCode::Success,
            false,
            endpoint_id,
            slot_id,
            event_sender,
        )?;
    }

    Ok(())
}

/// When an Event Data is encountered two additional things are needed:
/// - the EDTLA, counting already transmitted bytes of the current TD
/// - the completion code of the previously handled TRB
#[derive(Debug)]
pub struct EventDataTrbMetadata {
    edtla: u64,
    previous_completion_code: CompletionCode,
}
impl EventDataTrbMetadata {
    const fn new() -> Self {
        Self {
            edtla: 0,
            previous_completion_code: CompletionCode::Success,
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
    pcap_meta: EndpointPcapMeta,
    real_ep: ROEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: NormalSubmissionState,
    event_data_trb_metadata: EventDataTrbMetadata,
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
            event_data_trb_metadata: EventDataTrbMetadata::new(),
        }
    }

    fn handle_normal_trb_pre_hardware(&mut self, transfer_trb: TransferTrb) -> anyhow::Result<()> {
        trace!("handle_normal_trb_pre_hardware Out");

        let normal_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::Normal(normal_trb_data) => normal_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        if normal_trb_data.immediate_data {
            todo!()
        }

        if !normal_trb_data.chain {
            self.event_data_trb_metadata = EventDataTrbMetadata::new();
        }

        let mut data = vec![0; normal_trb_data.transfer_length as usize];
        self.dma_bus
            .read_bulk(normal_trb_data.data_pointer, &mut data);

        self.real_ep.submit(data.clone())?;
        pcap::out_submission(
            self.pcap_meta,
            transfer_trb.address,
            &data,
            normal_trb_data.transfer_length,
        );

        self.submission_state = NormalSubmissionState::AwaitingRealTransfer(transfer_trb);
        self.event_data_trb_metadata.previous_completion_code = CompletionCode::Success;

        Ok(())
    }

    fn handle_normal_trb_post_hardware(&mut self, transfer_trb: TransferTrb) -> anyhow::Result<()> {
        trace!("handle_normal_trb_post_hardware Out");

        let normal_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::Normal(normal_trb_data) => normal_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        self.event_data_trb_metadata.edtla = self
            .event_data_trb_metadata
            .edtla
            .wrapping_add(normal_trb_data.transfer_length as u64);

        if normal_trb_data.interrupt_on_completion {
            interrupt_on_completion(
                transfer_trb.address,
                CompletionCode::Success,
                false,
                self.endpoint_id,
                self.slot_id,
                &self.event_sender,
            )?;
        }

        pcap::out_completion(
            self.pcap_meta,
            transfer_trb.address,
            normal_trb_data.transfer_length,
        );

        self.event_data_trb_metadata.previous_completion_code = CompletionCode::Success;
        Ok(())
    }

    fn handle_event_data_trb(&mut self, transfer_trb: TransferTrb) -> anyhow::Result<()> {
        handle_event_data_trb_normal_ep(
            &transfer_trb,
            &mut self.event_data_trb_metadata,
            self.endpoint_id,
            self.slot_id,
            &self.event_sender,
        )?;

        self.submission_state = NormalSubmissionState::ConsumedEventDataTrb;
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

        let transfer_trb: TransferTrb = TransferTrb {
            address: trb.address,
            variant: TransferTrbVariant::parse(trb.buffer),
        };

        match transfer_trb.variant {
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
                            pcap::out_error(
                                self.pcap_meta,
                                transfer_trb.address,
                                &OutTrbProcessingResult::Disconnect,
                                &[],
                            );
                            TrbProcessingResult::Disconnect
                        }
                        OutTrbProcessingResult::Stall => {
                            info!("OutTrbProcessingResult::Stall and reporting CompletionCode::StallError");
                            pcap::out_error(
                                self.pcap_meta,
                                transfer_trb.address,
                                &OutTrbProcessingResult::Stall,
                                &[],
                            );
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
                            pcap::out_error(
                                self.pcap_meta,
                                transfer_trb.address,
                                &OutTrbProcessingResult::TransactionError,
                                &[],
                            );
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
    pcap_meta: EndpointPcapMeta,
    real_ep: RIEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: NormalSubmissionState,
    event_data_trb_metadata: EventDataTrbMetadata,
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
            event_data_trb_metadata: EventDataTrbMetadata::new(),
        }
    }

    fn handle_normal_trb_pre_hardware(&mut self, transfer_trb: TransferTrb) -> anyhow::Result<()> {
        trace!("handle_normal_trb_pre_hardware In");

        let normal_trb_data = match &transfer_trb.variant {
            TransferTrbVariant::Normal(normal_trb_data) => normal_trb_data,
            _ => unreachable!("checked variant before calling this handle"),
        };

        if normal_trb_data.immediate_data {
            todo!()
        }

        if !normal_trb_data.chain {
            self.event_data_trb_metadata = EventDataTrbMetadata::new();
        }

        self.real_ep
            .submit(normal_trb_data.transfer_length as usize)?;
        pcap::in_submission(
            self.pcap_meta,
            transfer_trb.address,
            normal_trb_data.transfer_length,
        );

        self.submission_state = NormalSubmissionState::AwaitingRealTransfer(transfer_trb);
        self.event_data_trb_metadata.previous_completion_code = CompletionCode::Success;

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

        self.event_data_trb_metadata.edtla = self
            .event_data_trb_metadata
            .edtla
            .wrapping_add(dma_length as u64);
        self.dma_bus
            .write_bulk(normal_trb_data.data_pointer, &hardware_data[..dma_length]);

        if normal_trb_data.interrupt_on_completion {
            interrupt_on_completion(
                transfer_trb.address,
                CompletionCode::Success,
                false,
                self.endpoint_id,
                self.slot_id,
                &self.event_sender,
            )?;
        }

        pcap::in_completion(self.pcap_meta, transfer_trb.address, &hardware_data);
        self.event_data_trb_metadata.previous_completion_code = completion_code;

        Ok(())
    }

    fn handle_event_data_trb(&mut self, transfer_trb: TransferTrb) -> anyhow::Result<()> {
        handle_event_data_trb_normal_ep(
            &transfer_trb,
            &mut self.event_data_trb_metadata,
            self.endpoint_id,
            self.slot_id,
            &self.event_sender,
        )?;

        self.submission_state = NormalSubmissionState::ConsumedEventDataTrb;
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

        let transfer_trb: TransferTrb = TransferTrb {
            address: trb.address,
            variant: TransferTrbVariant::parse(trb.buffer),
        };

        match transfer_trb.variant {
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
                            pcap::in_error(
                                self.pcap_meta,
                                transfer_trb.address,
                                &InTrbProcessingResult::Disconnect,
                            );
                            TrbProcessingResult::Disconnect
                        }
                        InTrbProcessingResult::Stall => {
                            info!("InTrbProcessingResult::Stall and reporting CompletionCode::StallError");
                            pcap::in_error(
                                self.pcap_meta,
                                transfer_trb.address,
                                &InTrbProcessingResult::Stall,
                            );
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
                            pcap::in_error(
                                self.pcap_meta,
                                transfer_trb.address,
                                &InTrbProcessingResult::TransactionError,
                            );
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::device::xhci::interrupter::InterrupterMessage;
    use crate::device::{bus::testutils::TestBusDevice, xhci::trb::testutils::RawTrbBuilder};
    use crate::dynamic_bus::DynamicBus;

    use std::sync::Arc;

    use tokio::sync::mpsc::{self, UnboundedReceiver};

    const FIRST_ADDRESS: u64 = 0x10;
    const SECOND_ADDRESS: u64 = 0x20;
    const THIRD_ADDRESS: u64 = 0x30;
    const FOURTH_ADDRESS: u64 = 0x40;
    const FIFTH_ADDRESS: u64 = 0x50;
    const SIXTH_ADDRESS: u64 = 0x60;

    const DMA_POINTER_1: u64 = 0x200;
    const DMA_POINTER_2: u64 = 0x400;
    const DMA_POINTER_3: u64 = 0x600;
    const DMA_POINTER_4: u64 = 0x800;

    const TRANSFER_LENGTH: u16 = 512;
    const EVENT_DATA_FIELD: u64 = 0xda7a;

    const BM_REQUEST_TYPE_IN: u8 = 0x80;
    const BM_REQUEST_TYPE_OUT: u8 = 0;

    const TRB_TYPE_NORMAL: u8 = 0x1;
    const TRB_TYPE_SETUP_STAGE: u8 = 0x2;
    const TRB_TYPE_DATA_STAGE: u8 = 0x3;
    const TRB_TYPE_STATUS_STAGE: u8 = 0x4;
    const TRB_TYPE_EVENT_DATA: u8 = 0x7;

    const TRT_OUT_DATA: u8 = 0x2;
    const TRT_IN_DATA: u8 = 0x3;

    // will return  the requested length of bytes with a value of 42
    #[derive(Debug)]
    pub struct DummyRealControlEndpointReadStatic {
        data_length: u16,
        direction: bool,
    }
    impl DummyRealControlEndpointReadStatic {
        fn new() -> Self {
            Self {
                data_length: 0,
                direction: false,
            }
        }
    }

    impl RealControlEndpointHandle for DummyRealControlEndpointReadStatic {
        type TrbCompletionFuture<'a> = Pin<
            Box<dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a>,
        >;

        fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()> {
            // fake request is instantly submitted but we need to remember the direction for next_complete
            const IN: u8 = 0b10000000;
            self.direction = (request.request_type & IN) == IN;
            self.data_length = request.length;

            Ok(())
        }

        fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
            Box::pin(async {
                let result = match self.direction {
                    true => {
                        let data = vec![42; self.data_length as usize];
                        ControlRequestProcessingResult::SuccessfulControlIn(data)
                    }
                    false => ControlRequestProcessingResult::SuccessfulControlOut,
                };
                Ok(result)
            })
        }
    }

    impl BaseEndpointHandle for DummyRealControlEndpointReadStatic {
        type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

        fn cancel(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }

        fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }
    }

    // Initialize test environment using the DummyRealControlEndpointReadStatic
    //
    // Use the ControlEndpointHandle to submit some TransferTrb.
    // Use the UnboundedReceiver to directly check events meant for a EventRing.
    fn init_control_endpoint_handle_test() -> (
        UnboundedReceiver<InterrupterMessage>,
        ControlEndpointHandle<DummyRealControlEndpointReadStatic>,
    ) {
        const SLOT_ID: u8 = 1;
        const ENDPOINT_ID: u8 = 1;

        let pcap_usb_bus_number = 1;
        let pcap_meta = EndpointPcapMeta::control(pcap_usb_bus_number, SLOT_ID, ENDPOINT_ID);

        let real_ep = DummyRealControlEndpointReadStatic::new();

        let dma_bus = Arc::new(DynamicBus::new());
        let dma_backing = vec![99; 2048];
        dma_bus
            .add(0x0, Arc::new(TestBusDevice::new(&dma_backing[..])))
            .expect("");

        let (event_sender, event_receiver) = mpsc::unbounded_channel();
        let event_sender = EventSender::new_with_sender(event_sender);

        let control_endpoint = ControlEndpointHandle::new(
            SLOT_ID,
            ENDPOINT_ID,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
        );
        (event_receiver, control_endpoint)
    }

    #[tokio::test]
    async fn submit_shortest_possible_control_in_request() {
        let (mut event_receiver, mut control_endpoint) = init_control_endpoint_handle_test();

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_bm_request_type(BM_REQUEST_TYPE_IN)
            .with_idt()
            .with_ioc()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_ioc()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
            .build();

        control_endpoint
            .submit_trb(setup_stage)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .next_completion()
            .await
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(status_stage)
            .expect("this dummy hardware request should never fail");

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FIRST_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, SECOND_ADDRESS);
    }

    #[tokio::test]
    async fn submit_shortest_possible_control_in_request_with_data_stage() {
        let (mut event_receiver, mut control_endpoint) = init_control_endpoint_handle_test();

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_bm_request_type(BM_REQUEST_TYPE_IN)
            .with_w_length(TRANSFER_LENGTH)
            .with_idt()
            .with_ioc()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, TRT_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER_1)
            .with_transfer_length(TRANSFER_LENGTH)
            .with_ioc()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_dir()
            .build();
        let status_stage = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_ioc()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
            .build();

        control_endpoint
            .submit_trb(setup_stage)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .next_completion()
            .await
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(data_stage)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(status_stage)
            .expect("this dummy hardware request should never fail");

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FIRST_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, SECOND_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, THIRD_ADDRESS);
    }

    #[tokio::test]
    async fn submit_control_in_request_with_event_data_at_the_end_of_the_data_stage() {
        let (mut event_receiver, mut control_endpoint) = init_control_endpoint_handle_test();

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_bm_request_type(BM_REQUEST_TYPE_IN)
            .with_w_length(TRANSFER_LENGTH)
            .with_idt()
            .with_ioc()
            .with_trb_type(0x2)
            .with_byte(14, TRT_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER_1)
            .with_transfer_length(TRANSFER_LENGTH)
            .with_ch()
            .with_trb_type(0x3)
            .with_dir()
            .build();
        let event_data = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ioc()
            .with_trb_type(0x7)
            .with_dir()
            .build();
        let status_stage = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_ioc()
            .with_trb_type(0x4)
            .with_dir()
            .build();

        control_endpoint
            .submit_trb(setup_stage)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .next_completion()
            .await
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(data_stage)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(event_data)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(status_stage)
            .expect("this dummy hardware request should never fail");

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FIRST_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_ne!(trb_data.trb_pointer, SECOND_ADDRESS);
        assert_eq!(trb_data.trb_pointer, EVENT_DATA_FIELD);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, THIRD_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FOURTH_ADDRESS);
    }

    #[tokio::test]
    async fn submit_control_in_request_with_event_data_between_two_data_stage_trb() {
        let (mut event_receiver, mut control_endpoint) = init_control_endpoint_handle_test();

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_bm_request_type(BM_REQUEST_TYPE_IN)
            .with_w_length(TRANSFER_LENGTH * 2)
            .with_idt()
            .with_ioc()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, TRT_IN_DATA)
            .build();
        let data_stage_1 = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER_1)
            .with_transfer_length(TRANSFER_LENGTH)
            .with_ch()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_dir()
            .build();
        let event_data = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ch()
            .with_ioc()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_dir()
            .build();
        let data_stage_2 = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_field(DMA_POINTER_2)
            .with_transfer_length(TRANSFER_LENGTH)
            .with_ioc()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_dir()
            .build();
        let status_stage = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_ioc()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
            .build();

        control_endpoint
            .submit_trb(setup_stage)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .next_completion()
            .await
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(data_stage_1)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(event_data)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(data_stage_2)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(status_stage)
            .expect("this dummy hardware request should never fail");

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FIRST_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_ne!(trb_data.trb_pointer, SECOND_ADDRESS);
        assert_eq!(trb_data.trb_pointer, EVENT_DATA_FIELD);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, THIRD_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FOURTH_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FIFTH_ADDRESS);
    }

    #[tokio::test]
    async fn submit_control_in_request_with_event_data_after_status_stage_trb() {
        let (mut event_receiver, mut control_endpoint) = init_control_endpoint_handle_test();

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_bm_request_type(BM_REQUEST_TYPE_IN)
            .with_idt()
            .with_ioc()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_ch()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
            .build();
        let event_data = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ioc()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_dir()
            .build();

        control_endpoint
            .submit_trb(setup_stage)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .next_completion()
            .await
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(status_stage)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(event_data)
            .expect("this dummy hardware request should never fail");

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FIRST_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_ne!(trb_data.trb_pointer, SECOND_ADDRESS);
        assert_eq!(trb_data.trb_pointer, EVENT_DATA_FIELD);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, THIRD_ADDRESS);
    }

    // expecting to receive 0xda7a via an out request
    #[derive(Debug)]
    pub struct DummyRealControlEndpointExpectDataPattern {}
    impl DummyRealControlEndpointExpectDataPattern {
        fn new() -> Self {
            Self {}
        }
    }

    impl RealControlEndpointHandle for DummyRealControlEndpointExpectDataPattern {
        type TrbCompletionFuture<'a> = Pin<
            Box<dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a>,
        >;

        fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()> {
            let Some(data) = request.data else {
                self::panic!("expected SendEvent(Transfer(_)), got {:?}", request.data);
            };

            assert_eq!(data, 0xda7a_u64.to_le_bytes()[..2]);
            Ok(())
        }

        fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
            Box::pin(async {
                let result = ControlRequestProcessingResult::SuccessfulControlOut;
                Ok(result)
            })
        }
    }

    impl BaseEndpointHandle for DummyRealControlEndpointExpectDataPattern {
        type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

        fn cancel(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }

        fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn submit_control_out_request_with_data_stage_using_immediate_data() {
        const SLOT_ID: u8 = 1;
        const ENDPOINT_ID: u8 = 1;

        let pcap_usb_bus_number = 1;
        let pcap_meta = EndpointPcapMeta::control(pcap_usb_bus_number, SLOT_ID, ENDPOINT_ID);

        let real_ep = DummyRealControlEndpointExpectDataPattern::new();

        let dma_bus = Arc::new(DynamicBus::new());
        let dma_backing = vec![99; 2048];
        dma_bus
            .add(0x0, Arc::new(TestBusDevice::new(&dma_backing[..])))
            .expect("");

        let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
        let event_sender = EventSender::new_with_sender(event_sender);

        let mut control_endpoint = ControlEndpointHandle::new(
            SLOT_ID,
            ENDPOINT_ID,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
        );

        const DMA_POINTER: u64 = 0xeb8bda7a;
        const TRANSFER_LENGTH: u16 = 2;

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_bm_request_type(BM_REQUEST_TYPE_OUT)
            .with_w_length(TRANSFER_LENGTH)
            .with_idt()
            .with_ioc()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, TRT_OUT_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER)
            .with_transfer_length(TRANSFER_LENGTH)
            .with_ioc()
            .with_idt()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_ioc()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .build();

        control_endpoint
            .submit_trb(setup_stage)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(data_stage)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(status_stage)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .next_completion()
            .await
            .expect("this dummy hardware request should never fail");

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FIRST_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, SECOND_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, THIRD_ADDRESS);
    }

    #[tokio::test]
    async fn submitting_out_of_order_or_unfinished_sequence_does_not_prevent_the_following_valid_sequence_of_trb(
    ) {
        let (mut event_receiver, mut control_endpoint) = init_control_endpoint_handle_test();

        let status_stage_out_of_order = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_ioc()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
            .build();
        let setup_stage_incomplete_sequence = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_bm_request_type(BM_REQUEST_TYPE_IN)
            .with_idt()
            .with_ioc()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let setup_stage = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_bm_request_type(BM_REQUEST_TYPE_IN)
            .with_idt()
            .with_ioc()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_ioc()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
            .build();

        control_endpoint
            .submit_trb(status_stage_out_of_order)
            .expect("this dummy hardware request should never fail");

        // To initialize a Control Transfer a Setup Stage TRB is needed. We expect
        // this Status Stage TRB to be ignored.
        assert!(event_receiver.is_empty());

        control_endpoint
            .submit_trb(setup_stage_incomplete_sequence)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .next_completion()
            .await
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(setup_stage)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .next_completion()
            .await
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(status_stage)
            .expect("this dummy hardware request should never fail");

        // The incomplete sequence (the second trb; a lone setup stage) is valid
        // and we expect an event.
        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, SECOND_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, THIRD_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FOURTH_ADDRESS);
    }

    // dummy for bulk in real endpoint returning `vec![42; requested length]`
    #[derive(Debug)]
    struct DummyRealInEndpoint {
        data_length: usize,
    }
    impl DummyRealInEndpoint {
        fn new() -> Self {
            Self { data_length: 0 }
        }
    }
    impl RealInEndpointHandle for DummyRealInEndpoint {
        type TrbCompletionFuture<'a> =
            Pin<Box<dyn Future<Output = anyhow::Result<InTrbProcessingResult>> + Send + 'a>>;

        fn submit(&mut self, data: usize) -> anyhow::Result<()> {
            self.data_length = data;
            Ok(())
        }

        fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
            Box::pin(async {
                let data = vec![42; self.data_length];
                let result = InTrbProcessingResult::Success(data);
                Ok(result)
            })
        }
    }
    impl BaseEndpointHandle for DummyRealInEndpoint {
        type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

        fn cancel(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }

        fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn submit_multi_trb_bulk_in_transfer_with_event_data() {
        const SLOT_ID: u8 = 1;
        const ENDPOINT_ID: u8 = 1;

        let pcap_usb_bus_number = 1;
        let pcap_meta = EndpointPcapMeta::bulk(pcap_usb_bus_number, SLOT_ID, ENDPOINT_ID);

        let real_ep = DummyRealInEndpoint::new();

        let dma_bus = Arc::new(DynamicBus::new());
        let dma_backing = vec![99; 2048];
        dma_bus
            .add(0x0, Arc::new(TestBusDevice::new(&dma_backing[..])))
            .expect("");

        let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
        let event_sender = EventSender::new_with_sender(event_sender);

        let mut bulk_in_endpoint = InEndpointHandle::new(
            SLOT_ID,
            ENDPOINT_ID,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
        );

        let normal_1 = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_data_field(DMA_POINTER_1)
            .with_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x8) // remaining TD Size: 2048
            .with_ch()
            .with_ioc()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let normal_2 = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER_2)
            .with_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x6) // remaining TD Size: 1536
            .with_ch()
            .with_ioc()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let normal_3 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_field(DMA_POINTER_3)
            .with_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x4) // remaining TD Size: 1024
            .with_ch()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let event_data_1 = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ch()
            .with_ioc()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_dir()
            .build();
        let normal_4 = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_data_field(DMA_POINTER_4)
            .with_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x2) // remaining TD Size: 512
            .with_ch()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let event_data_2 = RawTrbBuilder::new(SIXTH_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ioc()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_dir()
            .build();

        let input_trb = vec![
            normal_1,
            normal_2,
            normal_3,
            event_data_1,
            normal_4,
            event_data_2,
        ];

        for trb in input_trb {
            bulk_in_endpoint
                .submit_trb(trb)
                .expect("this dummy hardware request should never fail");
            assert_eq!(
                bulk_in_endpoint
                    .next_completion()
                    .await
                    .expect("this dummy hardware request should never fail"),
                TrbProcessingResult::Ok
            );
        }

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FIRST_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, SECOND_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_ne!(trb_data.trb_pointer, THIRD_ADDRESS);
        assert_eq!(trb_data.trb_pointer, EVENT_DATA_FIELD);
        assert_eq!(trb_data.trb_transfer_length, 1536);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FOURTH_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_ne!(trb_data.trb_pointer, FIFTH_ADDRESS);
        assert_eq!(trb_data.trb_pointer, EVENT_DATA_FIELD);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, SIXTH_ADDRESS);
    }

    // dummy for bulk out real endpoint returning success while discarding the data
    #[derive(Debug)]
    struct DummyRealOutEndpoint {}
    impl DummyRealOutEndpoint {
        fn new() -> Self {
            Self {}
        }
    }
    impl RealOutEndpointHandle for DummyRealOutEndpoint {
        type TrbCompletionFuture<'a> =
            Pin<Box<dyn Future<Output = anyhow::Result<OutTrbProcessingResult>> + Send + 'a>>;

        fn submit(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
            println!("consumed data of length: {}", data.len());
            Ok(())
        }

        fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
            Box::pin(async {
                let result = OutTrbProcessingResult::Success;
                Ok(result)
            })
        }
    }
    impl BaseEndpointHandle for DummyRealOutEndpoint {
        type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

        fn cancel(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }

        fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn submit_multi_trb_bulk_out_transfer_with_event_data() {
        const SLOT_ID: u8 = 1;
        const ENDPOINT_ID: u8 = 1;

        let pcap_usb_bus_number = 1;
        let pcap_meta = EndpointPcapMeta::bulk(pcap_usb_bus_number, SLOT_ID, ENDPOINT_ID);

        let real_ep = DummyRealOutEndpoint::new();

        let dma_bus = Arc::new(DynamicBus::new());
        let dma_backing = vec![99; 2048];
        dma_bus
            .add(0x0, Arc::new(TestBusDevice::new(&dma_backing[..])))
            .expect("");

        let (event_sender, mut event_receiver) = mpsc::unbounded_channel();
        let event_sender = EventSender::new_with_sender(event_sender);

        let mut bulk_out_endpoint = OutEndpointHandle::new(
            SLOT_ID,
            ENDPOINT_ID,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
        );

        let normal_1 = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_data_field(DMA_POINTER_1)
            .with_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x8) // remaining TD Size: 2048
            .with_ch()
            .with_ioc()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let normal_2 = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER_2)
            .with_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x6) // remaining TD Size: 1536
            .with_ch()
            .with_ioc()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let normal_3 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_field(DMA_POINTER_3)
            .with_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x4) // remaining TD Size: 1024
            .with_ch()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let event_data_1 = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ch()
            .with_ioc()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .build();
        let normal_4 = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_data_field(DMA_POINTER_4)
            .with_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x2) // remaining TD Size: 512
            .with_ch()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let event_data_2 = RawTrbBuilder::new(SIXTH_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ioc()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .build();

        let input_trb = vec![
            normal_1,
            normal_2,
            normal_3,
            event_data_1,
            normal_4,
            event_data_2,
        ];

        for trb in input_trb {
            bulk_out_endpoint
                .submit_trb(trb)
                .expect("this dummy hardware request should never fail");
            assert_eq!(
                bulk_out_endpoint
                    .next_completion()
                    .await
                    .expect("this dummy hardware request should never fail"),
                TrbProcessingResult::Ok
            );
        }

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FIRST_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, SECOND_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_ne!(trb_data.trb_pointer, THIRD_ADDRESS);
        assert_eq!(trb_data.trb_pointer, EVENT_DATA_FIELD);
        assert_eq!(trb_data.trb_transfer_length, 1536);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, FOURTH_ADDRESS);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_ne!(trb_data.trb_pointer, FIFTH_ADDRESS);
        assert_eq!(trb_data.trb_pointer, EVENT_DATA_FIELD);

        assert!(!event_receiver.is_empty());
        let message = event_receiver.recv().await;
        let Some(InterrupterMessage::SendEvent(EventTrb::Transfer(trb_data))) = &message else {
            self::panic!("expected SendEvent(Transfer(_)), got {:?}", message);
        };
        assert_eq!(trb_data.trb_pointer, SIXTH_ADDRESS);
    }
}
