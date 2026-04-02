use std::{fmt::Debug, future::Future, mem, ops::ControlFlow, pin::Pin};

use tracing::debug;

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
pub struct ControlEndpointHandle<RCEH: RealControlEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
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
        real_ep: RCEH,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) -> Self {
        Self {
            slot_id,
            endpoint_id,
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
                        let mut data = vec![0; self.request_builder.length as usize];
                        self.dma_bus
                            .read_bulk(data_trb_data.data_pointer, &mut data);

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
    real_ep: ROEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: NormalSubmissionState,
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
                let mut data = vec![0; normal_data.transfer_length as usize];
                self.dma_bus.read_bulk(normal_data.data_pointer, &mut data);
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
                            OutTrbProcessingResult::Disconnect => (
                                Some(CompletionCode::UsbTransactionError),
                                TrbProcessingResult::Disconnect,
                            ),
                            OutTrbProcessingResult::Stall => {
                                (Some(CompletionCode::StallError), TrbProcessingResult::Stall)
                            }
                            OutTrbProcessingResult::TransactionError => (
                                Some(CompletionCode::UsbTransactionError),
                                TrbProcessingResult::TransactionError,
                            ),
                            OutTrbProcessingResult::Success => {
                                let completion_code =
                                    if let TransferTrbVariant::Normal(ref normal_data) =
                                        transfer_trb.variant
                                    {
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
pub struct InEndpointHandle<RIEH: RealInEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    real_ep: RIEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: NormalSubmissionState,
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
                        InTrbProcessingResult::Disconnect => (
                            Some(CompletionCode::UsbTransactionError),
                            TrbProcessingResult::Disconnect,
                        ),
                        InTrbProcessingResult::Stall => {
                            (Some(CompletionCode::StallError), TrbProcessingResult::Stall)
                        }
                        InTrbProcessingResult::TransactionError => (
                            Some(CompletionCode::UsbTransactionError),
                            TrbProcessingResult::TransactionError,
                        ),
                        InTrbProcessingResult::Success(data) => {
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
