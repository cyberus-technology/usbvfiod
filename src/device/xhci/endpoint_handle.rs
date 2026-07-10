use std::{
    fmt::Debug,
    future::Future,
    mem::{self},
    pin::Pin,
};

use anyhow::anyhow;
use replace_with::replace_with_or_abort;
use tracing::{debug, info, trace, warn};

use crate::device::{
    bus::BusDeviceRef,
    pcap::{self, EndpointPcapMeta},
    xhci::{
        hotplug_endpoint_handle::BaseEndpointHandle,
        interrupter::EventSender,
        real_endpoint_handle::{
            ControlRequestProcessingResult, InTrbProcessingResult, InTrbProcessingStatus,
            OutTrbProcessingResult, RealControlEndpointHandle, RealInEndpointHandle,
            RealOutEndpointHandle,
        },
        trb::{
            CompletionCode, DataStageTrb, EventDataTrb, EventTrb, NormalTrb, RawTrb, SetupStageTrb,
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

/// Possible result cases for processing of a TRB.
///
/// Stall and TransactionError carry an Option to support TD-aggregation-based approaches.
/// - None indicates that the stall/error happened on the current TRB; the endpoint state machine
///   then reports the current dequeue pointer through the endpoint context.
/// - Some((addr, cs)) indicates that the stall/error happened on an earlier TRB but we notice it only
///   now because we aggregated all TRBs of a TD before talking to the real device; the endpoint
///   state machine should wind the dequeue pointer (and associated cycle state) back to this TRB.
#[derive(Debug, Clone, Copy)]
pub enum TrbProcessingResult {
    Ok,
    Stall(Option<(u64, bool)>),
    TrbError,
    TransactionError(Option<(u64, bool)>),
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

                    usb_request.data.append(
                        &mut data_pointer.to_le_bytes()[..transfer_length as usize].to_vec(),
                    );
                } else {
                    let mut tmp = vec![0u8; transfer_length as usize];
                    self.dma_bus.read_bulk(data_pointer, &mut tmp);

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

    fn handle_status_stage_hardware_response(
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
                        "invalid control transfer sequence; expected Setup, Data or Status Stage Trb, got: {:?}",
                        other_trb
                    );
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
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
                        "invalid control transfer sequence; expected Setup Stage, Normal or Event Data Trb, got: {:?}",
                        other_trb
                    );
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
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
                        "invalid control transfer sequence; expected Setup or Status Stage Trb, got: {:?}",
                        other_trb
                    );
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
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
                            self.handle_status_stage_hardware_response(address, trb)?;
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
                TrbProcessingResult::Stall(None)
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
                TrbProcessingResult::TransactionError(None)
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
                let data = if normal_data.immediate_data {
                    if normal_data.transfer_length > 8 {
                        todo!("using IDT with length > 8");
                    }
                    normal_data.data_pointer.to_le_bytes()[..normal_data.transfer_length as usize]
                        .to_vec()
                } else {
                    let mut data = vec![0; normal_data.transfer_length as usize];
                    self.dma_bus.read_bulk(normal_data.data_pointer, &mut data);
                    data
                };

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
                                (
                                    Some(CompletionCode::StallError),
                                    TrbProcessingResult::Stall(None),
                                )
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
                                    TrbProcessingResult::TransactionError(None),
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
struct SupportedInEndpointTrb {
    variant: SupportedInEndpointTrbVariant,
    addr: u64,
    cycle_bit: bool,
}

#[derive(Debug)]
enum SupportedInEndpointTrbVariant {
    Normal(NormalTrb),
    EventData(EventDataTrb),
}

impl TryFrom<TransferTrbVariant> for SupportedInEndpointTrbVariant {
    type Error = TransferTrbVariant;

    fn try_from(value: TransferTrbVariant) -> Result<Self, Self::Error> {
        match value {
            TransferTrbVariant::Normal(data) => Ok(Self::Normal(data)),
            TransferTrbVariant::EventData(data) => Ok(Self::EventData(data)),
            variant => Err(variant),
        }
    }
}

impl SupportedInEndpointTrb {
    const fn chain(&self) -> bool {
        match &self.variant {
            SupportedInEndpointTrbVariant::Normal(normal_trb_data) => normal_trb_data.chain,
            SupportedInEndpointTrbVariant::EventData(event_data_trb_data) => {
                event_data_trb_data.chain
            }
        }
    }

    const fn transfer_length(&self) -> usize {
        match &self.variant {
            SupportedInEndpointTrbVariant::Normal(data) => data.transfer_length as usize,
            SupportedInEndpointTrbVariant::EventData(_) => 0,
        }
    }
}

#[derive(Debug)]
enum TdBasedNormalSubmissionState {
    CollectingTd(Vec<SupportedInEndpointTrb>),
    AwaitingRealTransfer(Vec<SupportedInEndpointTrb>),
    UnsupportedTrb,
}

impl Default for TdBasedNormalSubmissionState {
    fn default() -> Self {
        Self::CollectingTd(vec![])
    }
}

#[derive(Debug)]
pub struct TdBasedInEndpointHandle<RIEH: RealInEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    pcap_meta: EndpointPcapMeta,
    real_ep: RIEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: TdBasedNormalSubmissionState,
}

impl<RIEH: RealInEndpointHandle> TdBasedInEndpointHandle<RIEH> {
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
            submission_state: TdBasedNormalSubmissionState::default(),
        }
    }
}

impl<RIEH: RealInEndpointHandle> EndpointHandle for TdBasedInEndpointHandle<RIEH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        let trbs = match &mut self.submission_state {
            TdBasedNormalSubmissionState::CollectingTd(trbs) => trbs,
            state => {
                return Err(anyhow!(
                    "TdBasedInEndpointHandle called while in state {state:?}; there is a logic error somewhere"
                ));
            }
        };

        let transfer_trb_variant = TransferTrbVariant::parse(trb.buffer);
        let supported_trb_variant =
            match SupportedInEndpointTrbVariant::try_from(transfer_trb_variant) {
                Ok(supported_trb) => supported_trb,
                Err(transfer_trb) => {
                    warn!(
                    "Encountered unsupported TRB on In Endpoint (slot {}, ep {}): {transfer_trb:?}",
                    self.slot_id, self.endpoint_id
                );
                    self.submission_state = TdBasedNormalSubmissionState::UnsupportedTrb;
                    return Ok(());
                }
            };
        let cycle_bit = trb.buffer[12] & 0x1 != 0;
        let supported_trb = SupportedInEndpointTrb {
            variant: supported_trb_variant,
            addr: trb.address,
            cycle_bit,
        };
        let end_of_td = !supported_trb.chain();
        trbs.push(supported_trb);
        if end_of_td {
            let td_request_length = trbs
                .iter()
                .map(SupportedInEndpointTrb::transfer_length)
                .sum::<usize>();
            debug!(
                "Submitting on ep {} a real request for {td_request_length} bytes",
                self.endpoint_id
            );
            self.real_ep.submit(td_request_length)?;

            replace_with_or_abort(&mut self.submission_state, |old_state| {
                let TdBasedNormalSubmissionState::CollectingTd(trbs) = old_state else {
                    unreachable!("verified the state is CollectingTd at the start of the function");
                };
                TdBasedNormalSubmissionState::AwaitingRealTransfer(trbs)
            });
        }

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            match &mut self.submission_state {
                TdBasedNormalSubmissionState::CollectingTd(_) => Ok(TrbProcessingResult::Ok),
                TdBasedNormalSubmissionState::UnsupportedTrb => Ok(TrbProcessingResult::TrbError),
                TdBasedNormalSubmissionState::AwaitingRealTransfer(trbs) => {
                    let completion = self.real_ep.next_completion().await?;
                    let trbs = mem::take(trbs);
                    self.submission_state = TdBasedNormalSubmissionState::default();
                    let processing_result = process_real_transfer_response(
                        self.endpoint_id,
                        self.slot_id,
                        completion,
                        trbs,
                        &self.event_sender,
                        &self.dma_bus,
                        self.pcap_meta,
                    )?;
                    Ok(processing_result)
                }
            }
        })
    }
}

fn process_real_transfer_response(
    endpoint_id: u8,
    slot_id: u8,
    completion: InTrbProcessingResult,
    trbs: Vec<SupportedInEndpointTrb>,
    event_sender: &EventSender,
    dma_bus: &BusDeviceRef,
    pcap_meta: EndpointPcapMeta,
) -> anyhow::Result<TrbProcessingResult> {
    debug!(
        "received device response with {} bytes",
        completion.data.len(),
    );

    let mut td_info = TdProcessingInfo {
        event_sender,
        dma_bus,
        pcap_meta,
        status: completion.status,
        state: TdProcessingState::Default,
        data: &completion.data,
        endpoint_id,
        slot_id,
    };

    for trb in trbs {
        if let Some(early_return_result) = td_info.process_trb(trb)? {
            return Ok(early_return_result);
        }
    }

    if !td_info.data.is_empty() {
        warn!(
            "leftover data on IN TD (received: {} bytes, leftover: {}",
            completion.data.len(),
            td_info.data.len(),
        );
    }

    Ok(TrbProcessingResult::Ok)
}

struct TdProcessingInfo<'a> {
    // always the same
    endpoint_id: u8,
    slot_id: u8,
    event_sender: &'a EventSender,
    dma_bus: &'a BusDeviceRef,
    pcap_meta: EndpointPcapMeta,
    // per TD data
    status: InTrbProcessingStatus,
    // updated every TRB
    state: TdProcessingState,
    data: &'a [u8],
}

enum TdProcessingState {
    Default,
    // no more data, skip forward to next TD
    ShortTransfer,
}

impl<'a> TdProcessingInfo<'a> {
    fn process_trb(
        &mut self,
        trb: SupportedInEndpointTrb,
    ) -> anyhow::Result<Option<TrbProcessingResult>> {
        // assumption: Only normal TRBs
        match trb.variant {
            SupportedInEndpointTrbVariant::Normal(data) => {
                self.process_normal_trb(trb.addr, trb.cycle_bit, data)
            }
            SupportedInEndpointTrbVariant::EventData(_data) => todo!(),
        }
    }

    fn process_normal_trb(
        &mut self,
        addr: u64,
        cs: bool,
        trb_data: NormalTrb,
    ) -> anyhow::Result<Option<TrbProcessingResult>> {
        match self.state {
            TdProcessingState::Default => {
                pcap::in_submission(self.pcap_meta, addr, trb_data.transfer_length);

                let bytes_requested = trb_data.transfer_length as usize;
                let bytes_available = self.data.len();
                let dma_byte_count = bytes_requested.min(bytes_available);
                let bytes = &self.data[..dma_byte_count];
                self.data = &self.data[dma_byte_count..];

                debug!(
                    "copying {dma_byte_count} bytes to {:#x}",
                    trb_data.data_pointer
                );
                self.dma_bus.write_bulk(trb_data.data_pointer, bytes);

                if bytes_available < bytes_requested {
                    let bytes_missing = bytes_requested - bytes_available;
                    match self.status {
                        InTrbProcessingStatus::Success => {
                            // short transfer
                            if trb_data.interrupt_on_completion || trb_data.interrupt_on_short {
                                let transfer_event = EventTrb::new_transfer_event_trb(
                                    addr,
                                    bytes_missing as u32,
                                    CompletionCode::ShortPacket,
                                    false,
                                    self.endpoint_id,
                                    self.slot_id,
                                );
                                self.event_sender.send(transfer_event)?;
                            }
                            self.state = TdProcessingState::ShortTransfer;
                            return Ok(None);
                        }
                        _ => {
                            let (completion_code, processing_result) = match self.status {
                                InTrbProcessingStatus::Disconnect => (
                                    CompletionCode::UsbTransactionError,
                                    TrbProcessingResult::Disconnect,
                                ),
                                InTrbProcessingStatus::Stall => (
                                    CompletionCode::StallError,
                                    TrbProcessingResult::Stall(Some((addr, cs))),
                                ),
                                InTrbProcessingStatus::TransactionError => (
                                    CompletionCode::UsbTransactionError,
                                    TrbProcessingResult::TransactionError(Some((addr, cs))),
                                ),
                                InTrbProcessingStatus::Success => {
                                    unreachable!("handled by outer match")
                                }
                            };
                            let transfer_event = EventTrb::new_transfer_event_trb(
                                addr,
                                bytes_missing as u32,
                                completion_code,
                                false,
                                self.endpoint_id,
                                self.slot_id,
                            );
                            self.event_sender.send(transfer_event)?;

                            pcap::in_error(self.pcap_meta, addr, &self.status);

                            return Ok(Some(processing_result));
                        }
                    }
                }

                pcap::in_completion(self.pcap_meta, addr, bytes);

                // event sending only when IOC is set
                if trb_data.interrupt_on_completion {
                    let transfer_event = EventTrb::new_transfer_event_trb(
                        addr,
                        0,
                        CompletionCode::Success,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(transfer_event)?;
                }

                Ok(None)
            }
            TdProcessingState::ShortTransfer => {
                // Skip all Normal TRBs.
                // We will need more handling here once we support EventData TRBs.

                // We do need to handle the case from xhci specification page 193 in chapter 4.10.1.1.2:
                // > If the Short Packet occurred while processing a Transfer TRB with only an ISP
                // > flag set, then two events shall be generated for the transfer; one for the Transfer
                // > TRB that the Short Packet occurred on, and a second for the last TRB with the
                // > IOC flag set.

                Ok(None)
            }
        }
    }
}

impl<RIEH: RealInEndpointHandle> BaseEndpointHandle for TdBasedInEndpointHandle<RIEH> {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async {
            self.submission_state = TdBasedNormalSubmissionState::default();
            self.real_ep.cancel().await
        })
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.clear_halt().await })
    }
}
