use std::{fmt::Debug, future::Future, pin::Pin};

use tracing::{debug, info, trace};

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
        trb::{
            CompletionCode, DataStageTrb, EventTrb, NormalTrb, RawTrb, SetupStageTrb,
            StatusStageTrb, TransferTrb, TransferTrbVariant, TrbDmaInfo,
        },
        usbrequest::UsbRequest,
    },
};

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

#[derive(Debug, PartialEq, Eq)]
pub struct ControlTransferState {
    /// upcoming or current stage/TD of a control transfer to be handled
    pub state: ControlTransferStage,
    /// holding the UsbRequest and all associated data
    pub data: ControlTransferData,
}
impl ControlTransferState {
    // previous_completion_code should never be used as is, thus the error as a default value
    const fn new(data: ControlTransferData) -> Self {
        Self {
            state: ControlTransferStage::ExpectSetupStageTrb,
            data,
        }
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
    /// Nothing happened yet. Awaiting a Setup Stage Trb and dropping any other
    /// Trb (they will not reach the hardware device).
    ExpectSetupStageTrb,
    /// Either collect data if a Data Stage Trb is received or skip the Data
    /// Stage TD altogether if a Status Stage Trb is received.
    MaybeDataStageTrb,
    MoreData,
    /// Finished processing the Data Stage if there was one.
    ExpectStatusStageTrb,
}
/// The state machine provides the information partially as ControlSubmissionState::AwaitingControlIn(TransferTrb).
/// Track state between us and the guest for building the current control request.
#[derive(Debug, PartialEq, Eq)]
pub enum ControlTransferData {
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
    /// referring to usbvfiod to hardware communication
    submission_state: ControlSubmissionState,
    /// referring to usbvfiod to guest communication
    transfer_state: ControlTransferState,
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
            transfer_state: ControlTransferState::new(ControlTransferData::In(
                UsbRequest::default(),
            )),
        }
    }

    fn handle_setup_stage_trb(&mut self, address: u64, trb: SetupStageTrb) -> anyhow::Result<()> {
        let usb_request = UsbRequest {
            address,
            request_type: trb.request_type,
            request: trb.request,
            value: trb.value,
            index: trb.index,
            length: trb.length,
            data_pointer: None,
            data: vec![],
        };

        if trb.request_type & 0x80 != 0 {
            trace!("SetupStage TRB with ControlIn");

            self.transfer_state =
                ControlTransferState::new(ControlTransferData::In(usb_request.clone()));

            self.real_ep.submit_control_request(usb_request.clone())?;
            pcap::control_submission(self.pcap_meta, &usb_request);

            self.submission_state = ControlSubmissionState::AwaitingControlIn(
                address,
                TransferTrbVariant::SetupStage(trb),
            );
        } else {
            trace!("SetupStage TRB with ControlOut");

            self.transfer_state = ControlTransferState::new(ControlTransferData::Out(usb_request));

            // actual hardware request happens in status stage after consuming the data stage td

            if trb.interrupt_on_completion {
                interrupt_on_completion(
                    address,
                    CompletionCode::Success,
                    false,
                    self.endpoint_id,
                    self.slot_id,
                    &self.event_sender,
                )?;
            }

            self.transfer_state.state = ControlTransferStage::MaybeDataStageTrb;
            self.submission_state = ControlSubmissionState::ParserConsumedTrb(
                address,
                TransferTrbVariant::SetupStage(trb),
            );
        }

        Ok(())
    }

    fn handle_setup_stage_hardware_response(
        &mut self,
        address: u64,
        trb: SetupStageTrb,
        hardware_data: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        match &mut self.transfer_state.data {
            // collect hardware data
            ControlTransferData::In(request) => {
                trace!("control in data {:?}", hardware_data);

                pcap::control_completion_in(self.pcap_meta, request.address, hardware_data);

                request.data.append(hardware_data);

                request.data.resize(trb.length as usize, 0);

                if trb.interrupt_on_completion {
                    interrupt_on_completion(
                        address,
                        CompletionCode::Success,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                        &self.event_sender,
                    )?;
                }

                self.transfer_state.state = ControlTransferStage::MaybeDataStageTrb;
            }
            ControlTransferData::Out(_) => {
                unreachable!("internal error: ControlOut SetupTrb have insufficient information to do the Hardware request; a submission state to arrive here should never be used");
            }
        }
        Ok(())
    }

    fn data_slices<T: TrbDmaInfo>(&mut self, address: u64, trb: &T) {
        let data_pointer = trb.data_pointer();
        let transfer_length = trb.transfer_length();
        let immediate_data = trb.has_immediate_data();

        match &mut self.transfer_state.data {
            ControlTransferData::In(usb_request) => {
                trace!("DMA for ControlIn");

                // From xhci specification chapter 4.11.2.2:
                //
                // System software is responsible for ensuring that the total data length defined by a
                // Data Stage TD (i.e. the sum of the Length fields of the Data Stage TRB and all Normal
                // TRBs) is equal to wLength. Note that communicating with some non-compliant
                // devices may require violating this rule.
                if usb_request.data.len() < transfer_length as usize {
                    self.transfer_state.state = ControlTransferStage::ExpectStatusStageTrb;
                    self.submission_state = ControlSubmissionState::ParserError(address); // TODO maybe protocol error?
                    return;
                }

                let byte_slice: Vec<u8> = usb_request
                    .data
                    .drain(0..transfer_length as usize)
                    .collect();

                trace!(
                    "DataStage TRB len: {} slice: {:?}",
                    byte_slice.len(),
                    byte_slice
                );
                self.dma_bus.write_bulk(data_pointer, &byte_slice);
            }
            ControlTransferData::Out(usb_request) => {
                trace!("DMA for ControlOut");

                if immediate_data {
                    // Only event data should follow when immediate data is used here
                    // but we do not check for that and allow multiple immediate data
                    // TRB in the data stage TD.

                    // SAFETY: is always set in the preceding setup stage
                    usb_request.data.append(
                        &mut data_pointer.to_le_bytes()[..transfer_length as usize].to_vec(),
                    );
                } else {
                    let mut tmp = vec![0u8; transfer_length as usize];
                    self.dma_bus.read_bulk(data_pointer, &mut tmp);

                    // SAFETY: is always set in the preceding setup stage
                    usb_request.data.append(&mut tmp);
                }
            }
        }
    }

    fn handle_data_stage_trb(&mut self, address: u64, trb: DataStageTrb) -> anyhow::Result<()> {
        trace!("DataStage TRB");

        self.data_slices(address, &trb);

        if trb.interrupt_on_completion {
            interrupt_on_completion(
                address,
                CompletionCode::Success,
                false,
                self.endpoint_id,
                self.slot_id,
                &self.event_sender,
            )?;
        }

        if trb.chain {
            self.transfer_state.state = ControlTransferStage::MoreData;
        } else {
            self.transfer_state.state = ControlTransferStage::ExpectStatusStageTrb;
        }

        self.submission_state =
            ControlSubmissionState::ParserConsumedTrb(address, TransferTrbVariant::DataStage(trb));
        Ok(())
    }

    fn handle_normal_trb(&mut self, address: u64, trb: NormalTrb) -> anyhow::Result<()> {
        trace!("Normal TRB");

        self.data_slices(address, &trb);

        if trb.interrupt_on_completion {
            interrupt_on_completion(
                address,
                CompletionCode::Success,
                false,
                self.endpoint_id,
                self.slot_id,
                &self.event_sender,
            )?;
        }

        if !trb.chain {
            self.transfer_state.state = ControlTransferStage::ExpectStatusStageTrb;
        }

        self.submission_state =
            ControlSubmissionState::ParserConsumedTrb(address, TransferTrbVariant::Normal(trb));
        Ok(())
    }

    fn handle_status_stage_trb(&mut self, address: u64, trb: StatusStageTrb) -> anyhow::Result<()> {
        match &mut self.transfer_state.data {
            ControlTransferData::In(_) => {
                trace!("StatusStage TRB with ControlIn");

                if trb.interrupt_on_completion {
                    interrupt_on_completion(
                        address,
                        CompletionCode::Success,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                        &self.event_sender,
                    )?;
                }

                if !trb.chain {
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
                }
                self.submission_state = ControlSubmissionState::ParserConsumedTrb(
                    address,
                    TransferTrbVariant::StatusStage(trb),
                );
            }
            ControlTransferData::Out(usb_request_out) => {
                trace!("StatusStage TRB with ControlOut");

                self.real_ep
                    .submit_control_request(usb_request_out.clone())?;
                pcap::control_submission(self.pcap_meta, usb_request_out);

                self.submission_state = ControlSubmissionState::AwaitingControlOut(
                    address,
                    TransferTrbVariant::StatusStage(trb),
                );
            }
        }
        Ok(())
    }

    fn handle_status_stage_trb_hardware_response(
        &mut self,
        address: u64,
        trb: StatusStageTrb,
    ) -> anyhow::Result<()> {
        match &mut self.transfer_state.data {
            ControlTransferData::In(_) => {
                unreachable!("internal error: ControlIn requests do the Hardware request in the SetupStage; a submission state to arrive here should never be used");
            }
            ControlTransferData::Out(usb_request) => {
                trace!("StatusStage TRB with ControlOut");

                pcap::control_completion_out(
                    self.pcap_meta,
                    usb_request.address,
                    u32::from(usb_request.length),
                );

                if !trb.chain {
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
                }

                if trb.interrupt_on_completion {
                    interrupt_on_completion(
                        address,
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
}

/// Track communication between us and the host hardware.
#[derive(Debug, Default, Clone)]
enum ControlSubmissionState {
    #[default]
    NoTrbSubmitted,
    ParserConsumedTrb(u64, TransferTrbVariant),
    ParserError(u64),
    AwaitingControlIn(u64, TransferTrbVariant),
    AwaitingControlOut(u64, TransferTrbVariant),
}

impl<RCEH: RealControlEndpointHandle> EndpointHandle for ControlEndpointHandle<RCEH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        let variant = TransferTrbVariant::parse(trb.buffer);

        if let TransferTrbVariant::Unrecognized(_, _) = &variant {
            // logging this is happening in next_completion()
            self.submission_state = ControlSubmissionState::ParserError(trb.address);
        }

        match &self.transfer_state.state {
            ControlTransferStage::ExpectSetupStageTrb => match variant {
                TransferTrbVariant::SetupStage(setup_stage) => {
                    self.handle_setup_stage_trb(trb.address, setup_stage)?;
                }
                other_trb => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        other_trb
                    );
                    // TODO maybe new state as dropped a invalid trb?
                    self.submission_state =
                        ControlSubmissionState::ParserConsumedTrb(trb.address, other_trb);
                }
            },
            ControlTransferStage::MaybeDataStageTrb => match variant {
                TransferTrbVariant::SetupStage(setup_stage) => {
                    info!(
                        "received Setup Stage TRB abort ongoing control transfer in favour of this new one"
                    );
                    self.handle_setup_stage_trb(trb.address, setup_stage)?;
                }
                TransferTrbVariant::DataStage(data_stage) => {
                    self.handle_data_stage_trb(trb.address, data_stage)?;
                }
                TransferTrbVariant::StatusStage(status_stage) => {
                    self.handle_status_stage_trb(trb.address, status_stage)?;
                }
                other_trb => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        other_trb
                    );
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
                    // TODO maybe new state as dropped a invalid trb?
                    self.submission_state =
                        ControlSubmissionState::ParserConsumedTrb(trb.address, other_trb);
                }
            },
            ControlTransferStage::MoreData => match variant {
                TransferTrbVariant::SetupStage(setup_stage) => {
                    info!(
                        "received Setup Stage TRB abort ongoing control transfer in favour of this new one"
                    );
                    self.handle_setup_stage_trb(trb.address, setup_stage)?;
                }
                TransferTrbVariant::Normal(normal) => {
                    self.handle_normal_trb(trb.address, normal)?;
                }
                other_trb => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        other_trb
                    );
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
                    // TODO maybe new state as dropped a invalid trb?
                    self.submission_state =
                        ControlSubmissionState::ParserConsumedTrb(trb.address, other_trb);
                }
            },

            ControlTransferStage::ExpectStatusStageTrb => match variant {
                TransferTrbVariant::SetupStage(setup_stage) => {
                    info!(
                        "received Setup Stage TRB abort ongoing control transfer in favour of this new one"
                    );
                    self.handle_setup_stage_trb(trb.address, setup_stage)?;
                }
                TransferTrbVariant::StatusStage(status_stage) => {
                    self.handle_status_stage_trb(trb.address, status_stage)?;
                }
                other_trb => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        other_trb
                    );
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
                    // TODO maybe new state as dropped a invalid trb?
                    self.submission_state =
                        ControlSubmissionState::ParserConsumedTrb(trb.address, other_trb);
                }
            },
        }

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let result = match self.submission_state.clone() {
                ControlSubmissionState::ParserConsumedTrb(address, variant) => {
                    trace!("consumed trb from address: {} as: {:?}", address, variant);
                    TrbProcessingResult::Ok
                }
                ControlSubmissionState::ParserError(address) => {
                    info!(
                        "Failed to parse Transfer Trb on Control Endpoint. slot {}",
                        self.slot_id
                    );
                    pcap::trb_error(self.pcap_meta, address);
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
                ControlSubmissionState::AwaitingControlIn(address, variant) => {
                    let processing_result = self.real_ep.next_completion().await?;
                    match processing_result {
                        ControlRequestProcessingResult::SuccessfulControlIn(mut data) => {
                            let trb = match variant {
                                TransferTrbVariant::SetupStage(setup_stage) => setup_stage,
                                _ => unreachable!("internal error: never set this ControlSubmissionState besides with a SetupStage."),
                            };
                            self.handle_setup_stage_hardware_response(address, trb, &mut data)?;
                            TrbProcessingResult::Ok
                        }
                        ControlRequestProcessingResult::SuccessfulControlOut => unreachable!(
                            "internal error: never set AwaitingControlIn and received a SuccessfulControlOut."
                        ),
                        processing_error => {
                            let usb_request = match &self.transfer_state.data {
                                ControlTransferData::In(usb_request) => usb_request,
                                _ => unreachable!("internal error: never set AwaitingControlIn without a UsbRequest containing a ControlIn"),
                            };
                            pcap::control_in_error(self.pcap_meta, usb_request, &processing_error);
                            self.handle_processing_error(processing_error, address)?
                        }
                    }
                }
                ControlSubmissionState::AwaitingControlOut(address, variant) => {
                    let processing_result = self.real_ep.next_completion().await?;
                    match processing_result {
                        ControlRequestProcessingResult::SuccessfulControlIn(_) => {
                            unreachable!("internal error: never set AwaitingControlOut and receive a SuccessfulControlIn.")
                        }
                        ControlRequestProcessingResult::SuccessfulControlOut => {
                            let trb = match variant {
                                TransferTrbVariant::StatusStage(status_stage) => status_stage,
                                _ => unreachable!("internal error: never set this ControlSubmissionState besides with a StatusStage."),
                            };
                            self.handle_status_stage_trb_hardware_response(address, trb)?;
                            TrbProcessingResult::Ok
                        }
                        processing_error => {
                            let usb_request = match &self.transfer_state.data {
                                ControlTransferData::Out(usb_request) => usb_request,
                                _ => unreachable!("internal error: never set AwaitingControlOut without a UsbRequest containing a ControlOut"),
                            };
                            pcap::control_out_error(self.pcap_meta, usb_request, &processing_error);
                            self.handle_processing_error(processing_error, address)?
                        }
                    }
                }
                ControlSubmissionState::NoTrbSubmitted => {
                    unreachable!("internal error: Always set a different ControlSubmissionState in submit_trb().")
                }
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
                unreachable!(
                    "internal error: Don't try processing an error with a successful ControlRequestProcessingResult."
                )
            }
            ControlRequestProcessingResult::SuccessfulControlOut => {
                panic!("SuccessfulControlOut should be handled elsewhere")
            }
        };
        Ok(mapped)
    }
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
        }
    }
}

#[derive(Debug, Default)]
enum NormalSubmissionState {
    #[default]
    NoTrbSubmitted,
    UnsupportedTrbType(RawTrb),
    AwaitingRealTransfer(TransferTrb),
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
        match &transfer_trb {
            TransferTrbVariant::Normal(normal_data) => {
                if normal_data.immediate_data {
                    todo!("immediate data on normal trb")
                }
                let mut data = vec![0; normal_data.transfer_length as usize];
                self.dma_bus.read_bulk(normal_data.data_pointer, &mut data);
                pcap::out_submission(
                    self.pcap_meta,
                    trb.address,
                    &data,
                    normal_data.transfer_length,
                );
                self.real_ep.submit(data)?;
                self.submission_state = NormalSubmissionState::AwaitingRealTransfer(TransferTrb {
                    address: trb.address,
                    variant: transfer_trb,
                });
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
                NormalSubmissionState::AwaitingRealTransfer(ref transfer_trb) => {
                    let (completion_code, processing_result) =
                        match self.real_ep.next_completion().await? {
                            OutTrbProcessingResult::Disconnect => {
                                pcap::out_error(
                                    self.pcap_meta,
                                    transfer_trb.address,
                                    &OutTrbProcessingResult::Disconnect,
                                    &[],
                                );
                                (
                                    Some(CompletionCode::UsbTransactionError),
                                    TrbProcessingResult::Disconnect,
                                )
                            }
                            OutTrbProcessingResult::Stall => {
                                pcap::out_error(
                                    self.pcap_meta,
                                    transfer_trb.address,
                                    &OutTrbProcessingResult::Stall,
                                    &[],
                                );
                                (Some(CompletionCode::StallError), TrbProcessingResult::Stall)
                            }
                            OutTrbProcessingResult::TransactionError => {
                                pcap::out_error(
                                    self.pcap_meta,
                                    transfer_trb.address,
                                    &OutTrbProcessingResult::TransactionError,
                                    &[],
                                );
                                (
                                    Some(CompletionCode::UsbTransactionError),
                                    TrbProcessingResult::TransactionError,
                                )
                            }
                            OutTrbProcessingResult::Success => {
                                let completion_code =
                                    if let TransferTrbVariant::Normal(ref normal_data) =
                                        transfer_trb.variant
                                    {
                                        pcap::out_completion(
                                            self.pcap_meta,
                                            transfer_trb.address,
                                            normal_data.transfer_length,
                                        );
                                        match normal_data.interrupt_on_completion {
                                            true => Some(CompletionCode::Success),
                                            false => None,
                                        }
                                    } else {
                                        unreachable!();
                                    };
                                (completion_code, TrbProcessingResult::Ok)
                            }
                        };

                    if let Some(completion_code) = completion_code {
                        let transfer_event = EventTrb::new_transfer_event_trb(
                            transfer_trb.address,
                            0,
                            completion_code,
                            false,
                            self.endpoint_id,
                            self.slot_id,
                        );
                        self.event_sender.send(transfer_event)?;
                    }

                    processing_result
                }
                NormalSubmissionState::NoTrbSubmitted => {
                    unreachable!("internal error: Always set a different ControlSubmissionState in submit_trb().")
                }
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
        }
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
        match &transfer_trb {
            TransferTrbVariant::Normal(normal_data) => {
                pcap::in_submission(self.pcap_meta, trb.address, normal_data.transfer_length);
                self.real_ep.submit(normal_data.transfer_length as usize)?;
                self.submission_state = NormalSubmissionState::AwaitingRealTransfer(TransferTrb {
                    address: trb.address,
                    variant: transfer_trb,
                });
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
                NormalSubmissionState::AwaitingRealTransfer(ref transfer_trb) => {
                    let (completion_code, processing_result) = match self
                        .real_ep
                        .next_completion()
                        .await?
                    {
                        InTrbProcessingResult::Disconnect => {
                            pcap::in_error(
                                self.pcap_meta,
                                transfer_trb.address,
                                &InTrbProcessingResult::Disconnect,
                            );
                            (
                                Some(CompletionCode::UsbTransactionError),
                                TrbProcessingResult::Disconnect,
                            )
                        }
                        InTrbProcessingResult::Stall => {
                            pcap::in_error(
                                self.pcap_meta,
                                transfer_trb.address,
                                &InTrbProcessingResult::Stall,
                            );
                            (Some(CompletionCode::StallError), TrbProcessingResult::Stall)
                        }
                        InTrbProcessingResult::TransactionError => {
                            pcap::in_error(
                                self.pcap_meta,
                                transfer_trb.address,
                                &InTrbProcessingResult::TransactionError,
                            );
                            (
                                Some(CompletionCode::UsbTransactionError),
                                TrbProcessingResult::TransactionError,
                            )
                        }
                        InTrbProcessingResult::Success(data) => {
                            pcap::in_completion(self.pcap_meta, transfer_trb.address, &data);
                            let completion_code = if let TransferTrbVariant::Normal(
                                ref normal_data,
                            ) = transfer_trb.variant
                            {
                                // needs more checks.
                                // - if we got less data, we need to do short-packet handling
                                let requested_len = normal_data.transfer_length as usize;
                                let received_len = data.len();
                                let dma_length = if received_len > requested_len {
                                    debug!("device delivered more data than requested. Requested {requested_len}, received {received_len}. Sending {:?}, dropping {:?}", &data[..requested_len], &data[requested_len..]);
                                    requested_len
                                } else {
                                    received_len
                                };
                                self.dma_bus
                                    .write_bulk(normal_data.data_pointer, &data[..dma_length]);

                                // event sending only when IOC is set
                                match normal_data.interrupt_on_completion {
                                    true => Some(CompletionCode::Success),
                                    false => None,
                                }
                            } else {
                                unreachable!();
                            };

                            (completion_code, TrbProcessingResult::Ok)
                        }
                    };

                    if let Some(completion_code) = completion_code {
                        let transfer_event = EventTrb::new_transfer_event_trb(
                            transfer_trb.address,
                            0,
                            completion_code,
                            false,
                            self.endpoint_id,
                            self.slot_id,
                        );

                        self.event_sender.send(transfer_event)?;
                    }

                    processing_result
                }
                NormalSubmissionState::NoTrbSubmitted => {
                    unreachable!("internal error: Always set a different ControlSubmissionState in submit_trb().")
                }
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
