use std::{
    fmt::Debug,
    future::Future,
    mem::{self},
    ops::ControlFlow,
    pin::Pin,
};

use anyhow::anyhow;
use replace_with::replace_with_or_abort;
use tracing::{debug, warn};

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
            CompletionCode, EventDataTrbData, EventTrb, NormalTrbData, RawTrb, TransferTrb,
            TransferTrbVariant,
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

#[derive(Debug)]
pub struct ControlEndpointHandle<RCEH: RealControlEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    pcap_meta: EndpointPcapMeta,
    real_ep: RCEH,
    trb_parser: ControlRequestParser,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: ControlSubmissionState,
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
            trb_parser: ControlRequestParser::new(dma_bus.clone()),
            dma_bus,
            event_sender,
            submission_state: ControlSubmissionState::NoTrbSubmitted,
        }
    }
}

#[derive(Debug, Default)]
enum ControlSubmissionState {
    #[default]
    NoTrbSubmitted,
    ParserConsumedTrb,
    // store address of trb that failed to parse.
    // needs to be specified inside the transfer event indicating the error.
    ParserError(u64),
    AwaitingControlIn(UsbRequest),
    AwaitingControlOut(UsbRequest),
}

impl<RCEH: RealControlEndpointHandle> EndpointHandle for ControlEndpointHandle<RCEH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        let trb_address = trb.address;
        if let ControlFlow::Break(res) = self.trb_parser.trb(trb) {
            match res {
                Ok(request) => {
                    let request_copy = request.clone_without_data();
                    let is_out_request = request.request_type & 0x80 == 0;

                    pcap::control_submission(self.pcap_meta, &request);

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

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let result = match self.submission_state {
                ControlSubmissionState::ParserConsumedTrb => TrbProcessingResult::Ok,
                ControlSubmissionState::ParserError(trb_address) => {
                    pcap::trb_error(self.pcap_meta, trb_address);
                    let event = EventTrb::new_transfer_event_trb(
                        trb_address,
                        0,
                        CompletionCode::TrbError,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(event)?;
                    TrbProcessingResult::TrbError
                }
                ControlSubmissionState::AwaitingControlIn(ref usb_request) => {
                    let processing_result = self.real_ep.next_completion().await?;
                    match processing_result {
                        ControlRequestProcessingResult::SuccessfulControlIn(data) => {
                            debug!("got data from control in: {data:?}");
                            pcap::control_completion_in(self.pcap_meta, usb_request.address, &data);
                            if let Some(data_pointer) = usb_request.data_pointer {
                                debug!("writing data to {data_pointer}");
                                self.dma_bus.write_bulk(data_pointer, &data);
                            }

                            let event = EventTrb::new_transfer_event_trb(
                                usb_request.address,
                                0,
                                CompletionCode::Success,
                                false,
                                self.endpoint_id,
                                self.slot_id,
                            );
                            self.event_sender.send(event)?;

                            TrbProcessingResult::Ok
                        }
                        ControlRequestProcessingResult::SuccessfulControlOut => unreachable!(),
                        processing_error => {
                            pcap::control_in_error(self.pcap_meta, usb_request, &processing_error);
                            self.handle_processing_error(processing_error, usb_request.address)?
                        }
                    }
                }
                ControlSubmissionState::AwaitingControlOut(ref usb_request) => {
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
                            let event = EventTrb::new_transfer_event_trb(
                                usb_request.address,
                                0,
                                CompletionCode::Success,
                                false,
                                self.endpoint_id,
                                self.slot_id,
                            );
                            self.event_sender.send(event)?;

                            TrbProcessingResult::Ok
                        }
                        processing_error => {
                            pcap::control_out_error(self.pcap_meta, usb_request, &processing_error);
                            self.handle_processing_error(processing_error, usb_request.address)?
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
struct ControlRequestParser {
    state: ControlRequestParserState,
    dma_bus: BusDeviceRef,
    request_builder: UsbRequest,
}

impl ControlRequestParser {
    fn new(dma_bus: BusDeviceRef) -> Self {
        Self {
            state: ControlRequestParserState::Initial,
            dma_bus,
            request_builder: Default::default(),
        }
    }
}

#[derive(Debug)]
enum ControlRequestParserState {
    Initial,
    SetupStageConsumed,
    DataStageConsumed,
}

impl ControlRequestParser {
    fn trb(&mut self, trb: RawTrb) -> ControlFlow<Result<UsbRequest, ()>> {
        let transfer_trb = TransferTrbVariant::parse(trb.buffer);

        loop {
            match &self.state {
                ControlRequestParserState::Initial => match transfer_trb {
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
                        self.state = ControlRequestParserState::SetupStageConsumed;
                        return ControlFlow::Continue(());
                    }
                    _ => return ControlFlow::Break(Err(())),
                },
                ControlRequestParserState::SetupStageConsumed => match transfer_trb {
                    TransferTrbVariant::DataStage(data_trb_data) => {
                        let data = if data_trb_data.immediate_data {
                            if self.request_builder.length > 8 {
                                todo!("using IDT with length > 8");
                            }
                            data_trb_data.data_pointer.to_le_bytes()
                                [..self.request_builder.length as usize]
                                .to_vec()
                        } else {
                            let mut data = vec![0; self.request_builder.length as usize];
                            self.dma_bus
                                .read_bulk(data_trb_data.data_pointer, &mut data);
                            data
                        };

                        self.request_builder.data = Some(data);
                        self.request_builder.data_pointer = Some(data_trb_data.data_pointer);
                        self.state = ControlRequestParserState::DataStageConsumed;
                        return ControlFlow::Continue(());
                    }
                    TransferTrbVariant::StatusStage(_) => {
                        self.state = ControlRequestParserState::DataStageConsumed;
                        continue;
                    }
                    _ => return ControlFlow::Break(Err(())),
                },
                ControlRequestParserState::DataStageConsumed => match transfer_trb {
                    TransferTrbVariant::StatusStage(_) => {
                        self.request_builder.address = trb.address;
                        let request = mem::take(&mut self.request_builder);
                        self.request_builder = UsbRequest::default();
                        self.state = ControlRequestParserState::Initial;
                        return ControlFlow::Break(Ok(request));
                    }
                    _ => return ControlFlow::Break(Err(())),
                },
            }
        }
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
struct SupportedInEndpointTrb {
    variant: SupportedInEndpointTrbVariant,
    addr: u64,
    cycle_bit: bool,
}

#[derive(Debug)]
enum SupportedInEndpointTrbVariant {
    Normal(NormalTrbData),
    EventData(EventDataTrbData),
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
                TdBasedNormalSubmissionState::UnsupportedTrb => {
                    self.submission_state = TdBasedNormalSubmissionState::default();
                    Ok(TrbProcessingResult::TrbError)
                }
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
    ShortTransfer(usize),
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
        trb_data: NormalTrbData,
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
                            self.state = TdProcessingState::ShortTransfer(bytes_missing);

                            pcap::in_completion(self.pcap_meta, addr, bytes);

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
            TdProcessingState::ShortTransfer(bytes_missing) => {
                // Skip all Normal TRBs.
                // We will need more handling here once we support EventData TRBs.

                if trb_data.interrupt_on_completion {
                    let transfer_event = EventTrb::new_transfer_event_trb(
                        addr,
                        bytes_missing as u32,
                        CompletionCode::Success,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(transfer_event)?;
                }

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

#[cfg(test)]
pub mod tests {
    use super::*;

    pub mod testutils {
        use super::*;

        // will return `vec![42; requested length]`
        #[derive(Debug)]
        pub struct MockRealControlEndpointReadStatic {
            data_length: u16,
            direction: bool,
        }
        impl MockRealControlEndpointReadStatic {
            pub fn new() -> Self {
                Self {
                    data_length: 0,
                    direction: false,
                }
            }
        }

        impl RealControlEndpointHandle for MockRealControlEndpointReadStatic {
            type TrbCompletionFuture<'a> = Pin<
                Box<
                    dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a,
                >,
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

        impl BaseEndpointHandle for MockRealControlEndpointReadStatic {
            type CompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

            fn cancel(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }

            fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }
        }

        // will return `vec![42; requested length]`
        #[derive(Debug)]
        pub struct MockRealInEndpoint {
            data_length: usize,
        }
        impl MockRealInEndpoint {
            pub fn new() -> Self {
                Self { data_length: 0 }
            }
        }
        impl RealInEndpointHandle for MockRealInEndpoint {
            type TrbCompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<InTrbProcessingResult>> + Send + 'a>>;

            fn submit(&mut self, data: usize) -> anyhow::Result<()> {
                self.data_length = data;
                Ok(())
            }

            fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
                Box::pin(async {
                    let data = vec![42; self.data_length];
                    let result = InTrbProcessingResult {
                        status: InTrbProcessingStatus::Success,
                        data,
                    };
                    Ok(result)
                })
            }
        }
        impl BaseEndpointHandle for MockRealInEndpoint {
            type CompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

            fn cancel(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }

            fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }
        }

        // mock for bulk out real endpoint returning success while discarding the data
        #[derive(Debug)]
        pub struct MockRealOutEndpoint {}
        impl MockRealOutEndpoint {
            pub fn new() -> Self {
                Self {}
            }
        }
        impl RealOutEndpointHandle for MockRealOutEndpoint {
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
        impl BaseEndpointHandle for MockRealOutEndpoint {
            type CompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

            fn cancel(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }

            fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }
        }
    }
}
