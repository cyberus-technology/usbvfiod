use std::{fmt::Debug, future::Future, mem, ops::ControlFlow, pin::Pin, sync::Arc};

use tokio::{runtime, select, sync::Mutex};
use tokio_util::sync::CancellationToken;

use crate::device::{
    bus::BusDeviceRef,
    pci::{
        trb::{CompletionCode, EventTrb, RawTrb, TransferTrb, TransferTrbVariant},
        usbrequest::UsbRequest,
    },
    xhci::{
        interrupter::EventSender,
        real_endpoint_handle::{
            ControlRequestProcessingResult, InTrbProcessingResult, OutTrbProcessingResult,
            RealControlEndpointHandle, RealInEndpointHandle, RealOutEndpointHandle,
        },
    },
};

pub trait EndpointHandle: Debug + Send + 'static {
    fn submit_trb(&mut self, trb: RawTrb);
    fn next_completion(&mut self)
        -> Pin<Box<dyn Future<Output = TrbProcessingResult> + Send + '_>>;
    fn cancel(&mut self);
    fn clear_halt(&mut self);
}

#[derive(Debug, Clone, Copy)]
pub enum TrbProcessingResult {
    Ok,
    Stall,
    TrbError,
    TransactionError,
    Disconnect,
}

#[derive(Debug)]
pub struct HotplugEndpointHandle {
    endpoint_handle: Arc<Mutex<Option<Box<dyn EndpointHandle>>>>,
    notify_detach: CancellationToken,
}

impl HotplugEndpointHandle {
    pub fn new(
        endpoint_handle: Box<dyn EndpointHandle>,
        notify_detach: CancellationToken,
        async_runtime: &runtime::Handle,
    ) -> Self {
        let endpoint_handle = Arc::new(Mutex::new(Some(endpoint_handle)));

        async_runtime.spawn(Self::detach_handler(
            endpoint_handle.clone(),
            notify_detach.clone(),
        ));

        Self {
            endpoint_handle,
            notify_detach,
        }
    }

    // Necessary because AddressDevice/ConfigureEndpoint might open an endpoint while the device just recently was removed.
    // It makes no sense to fail the command then. So we create a dummy endpoint handle that behaves the same as the
    // endpoint handle of a removed device.
    pub fn dummy() -> Self {
        Self {
            endpoint_handle: Arc::new(Mutex::new(None)),
            // just a dummy; nobody will notify, nobody listens
            notify_detach: CancellationToken::new(),
        }
    }

    pub fn submit_trb(&mut self, trb: RawTrb) {
        if let Ok(mut guard) = self.endpoint_handle.try_lock() {
            if let Some(device) = guard.as_mut() {
                device.submit_trb(trb);
            }
        }
    }

    pub fn next_completion(
        &self,
    ) -> Pin<Box<dyn Future<Output = TrbProcessingResult> + Send + '_>> {
        let ep_clone = self.endpoint_handle.clone();
        let detach_notify_clone = self.notify_detach.clone();

        Box::pin(async move {
            if let Some(ep) = ep_clone.lock().await.as_mut() {
                select! {
                    result = ep.next_completion() => match result {
                        TrbProcessingResult::Disconnect => {
                            detach_notify_clone.cancel();
                            TrbProcessingResult::TransactionError
                        },
                        result => result,
                    },
                    _ = detach_notify_clone.cancelled() => TrbProcessingResult::TransactionError,
                }
            } else {
                TrbProcessingResult::TransactionError
            }
        })
    }

    pub fn cancel(&self) {
        if let Ok(mut guard) = self.endpoint_handle.try_lock() {
            if let Some(device) = guard.as_mut() {
                device.cancel();
            }
        }
    }

    pub fn clear_halt(&self) {
        if let Ok(mut guard) = self.endpoint_handle.try_lock() {
            if let Some(device) = guard.as_mut() {
                device.cancel();
            }
        }
    }

    // wait for signal of other endpoints or the central detach.
    // Drop device when receiving notification.
    async fn detach_handler(
        endpoint_handle: Arc<Mutex<Option<Box<dyn EndpointHandle>>>>,
        notify_detach: CancellationToken,
    ) {
        notify_detach.cancelled().await;
        let mut ep = endpoint_handle.lock().await;
        *ep = None;
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
    last_request: Option<UsbRequest>,
    last_trb_address: Option<u64>,
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
            last_request: None,
            last_trb_address: None,
        }
    }
}

#[derive(Debug)]
enum ControlSubmissionState {
    NoTrbSubmitted,
    ParserConsumedTrb,
    // store address of trb that failed to parse.
    // needs to be specified inside the transfer event indicating the error.
    ParserError(u64),
    AwaitingControlIn(UsbRequest),
    AwaitingControlOut(UsbRequest),
}

impl<RCEH: RealControlEndpointHandle> EndpointHandle for ControlEndpointHandle<RCEH> {
    fn submit_trb(&mut self, trb: RawTrb) {
        assert!(
            matches!(
                self.submission_state,
                ControlSubmissionState::NoTrbSubmitted
            ),
            "submit_trb called twice without calling next_completion"
        );

        let trb_address = trb.address;
        if let ControlFlow::Break(res) = self.trb_parser.trb(trb) {
            match res {
                Ok(request) => {
                    let request_copy = request.clone_without_data();
                    let is_out_request = request.request_type & 0x80 == 0;

                    self.real_ep.submit_control_request(request);

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
    }

    fn next_completion(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = TrbProcessingResult> + Send + '_>> {
        assert!(
            !matches!(
                self.submission_state,
                ControlSubmissionState::NoTrbSubmitted
            ),
            "next_completion called without prior submit_trb"
        );

        Box::pin(async {
            match self.submission_state {
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
                    self.event_sender.send(event);
                    TrbProcessingResult::TrbError
                }
                ControlSubmissionState::AwaitingControlIn(ref usb_request) => {
                    let processing_result = self.real_ep.next_completion().await;
                    match processing_result {
                        ControlRequestProcessingResult::SuccessfulControlIn(data) => {
                            if let Some(data_pointer) = usb_request.data_pointer {
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
                            self.event_sender.send(event);

                            TrbProcessingResult::Ok
                        }
                        ControlRequestProcessingResult::SuccessfulControlOut => unreachable!(),
                        processing_error => {
                            self.handle_processing_error(processing_error, usb_request.address)
                        }
                    }
                }
                ControlSubmissionState::AwaitingControlOut(ref usb_request) => {
                    let processing_result = self.real_ep.next_completion().await;
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
                            self.event_sender.send(event);

                            TrbProcessingResult::Ok
                        }
                        processing_error => {
                            self.handle_processing_error(processing_error, usb_request.address)
                        }
                    }
                }
                ControlSubmissionState::NoTrbSubmitted => unreachable!(),
            }
        })
    }

    fn cancel(&mut self) {
        self.real_ep.cancel();
    }

    fn clear_halt(&mut self) {
        self.real_ep.clear_halt();
    }
}

impl<RCEH: RealControlEndpointHandle> ControlEndpointHandle<RCEH> {
    fn handle_processing_error(
        &mut self,
        error: ControlRequestProcessingResult,
        request_address: u64,
    ) -> TrbProcessingResult {
        match error {
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
                self.event_sender.send(event);
                TrbProcessingResult::TrbError
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
                self.event_sender.send(event);
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
                self.event_sender.send(event);
                TrbProcessingResult::TransactionError
            }
            ControlRequestProcessingResult::SuccessfulControlIn(_) => {
                panic!("SuccessfulControlIn should be handled elsewhere")
            }
            ControlRequestProcessingResult::SuccessfulControlOut => {
                panic!("SuccessfulControlOut should be handled elsewhere")
            }
        }
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
            dma_bus: dma_bus,
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
                    TransferTrbVariant::StatusStage => {
                        self.state = ControlRequestParserState::DataStageConsumed;
                        continue;
                    }
                    _ => return ControlFlow::Break(Err(())),
                },
                ControlRequestParserState::DataStageConsumed => match transfer_trb {
                    TransferTrbVariant::StatusStage => {
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

#[derive(Debug, Default)]
enum NormalSubmissionState {
    #[default]
    NoTrbSubmitted,
    UnsupportedTrbType(RawTrb),
    AwaitingRealTransfer(TransferTrb),
}

impl<ROEH: RealOutEndpointHandle> EndpointHandle for OutEndpointHandle<ROEH> {
    fn submit_trb(&mut self, trb: RawTrb) {
        assert!(
            matches!(self.submission_state, NormalSubmissionState::NoTrbSubmitted),
            "submit_trb called twice without calling next_completion"
        );

        let transfer_trb = TransferTrbVariant::parse(trb.buffer);
        match &transfer_trb {
            TransferTrbVariant::Normal(normal_data) => {
                let mut data = vec![0; normal_data.transfer_length as usize];
                self.dma_bus.read_bulk(normal_data.data_pointer, &mut data);
                self.real_ep.submit(data);
                self.submission_state = NormalSubmissionState::AwaitingRealTransfer(TransferTrb {
                    address: trb.address,
                    variant: transfer_trb,
                });
            }
            _ => self.submission_state = NormalSubmissionState::UnsupportedTrbType(trb),
        }
    }

    fn next_completion(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = TrbProcessingResult> + Send + '_>> {
        assert!(
            !matches!(self.submission_state, NormalSubmissionState::NoTrbSubmitted),
            "next_completion called without prior submit_trb"
        );

        Box::pin(async {
            match mem::take(&mut self.submission_state) {
                NormalSubmissionState::UnsupportedTrbType(trb) => {
                    let transfer_event = EventTrb::new_transfer_event_trb(
                        trb.address,
                        0,
                        CompletionCode::TrbError,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(transfer_event);

                    TrbProcessingResult::TrbError
                }
                NormalSubmissionState::AwaitingRealTransfer(transfer_trb) => {
                    let (completion_code, processing_result) =
                        match self.real_ep.next_completion().await {
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
                                    if let TransferTrbVariant::Normal(normal_data) =
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
                        self.event_sender.send(transfer_event);
                    }

                    processing_result
                }
                NormalSubmissionState::NoTrbSubmitted => unreachable!(),
            }
        })
    }

    fn cancel(&mut self) {
        self.real_ep.cancel();
    }

    fn clear_halt(&mut self) {
        self.real_ep.clear_halt();
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

impl<RIEH: RealInEndpointHandle> EndpointHandle for InEndpointHandle<RIEH> {
    fn submit_trb(&mut self, trb: RawTrb) {
        assert!(
            matches!(self.submission_state, NormalSubmissionState::NoTrbSubmitted),
            "submit_trb called twice without calling next_completion"
        );

        let transfer_trb = TransferTrbVariant::parse(trb.buffer);
        match &transfer_trb {
            TransferTrbVariant::Normal(normal_data) => {
                self.real_ep.submit(normal_data.transfer_length as usize);
                self.submission_state = NormalSubmissionState::AwaitingRealTransfer(TransferTrb {
                    address: trb.address,
                    variant: transfer_trb,
                });
            }
            _ => self.submission_state = NormalSubmissionState::UnsupportedTrbType(trb),
        }
    }

    fn next_completion(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = TrbProcessingResult> + Send + '_>> {
        assert!(
            !matches!(self.submission_state, NormalSubmissionState::NoTrbSubmitted),
            "next_completion called without prior submit_trb"
        );

        Box::pin(async {
            match mem::take(&mut self.submission_state) {
                NormalSubmissionState::UnsupportedTrbType(trb) => {
                    let transfer_event = EventTrb::new_transfer_event_trb(
                        trb.address,
                        0,
                        CompletionCode::TrbError,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(transfer_event);

                    TrbProcessingResult::TrbError
                }
                NormalSubmissionState::AwaitingRealTransfer(transfer_trb) => {
                    let (completion_code, processing_result) =
                        match self.real_ep.next_completion().await {
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
                                let completion_code =
                                    if let TransferTrbVariant::Normal(normal_data) =
                                        transfer_trb.variant
                                    {
                                        // needs more checks.
                                        // - we should ensure we didn't receive more data than requested
                                        // - if we got less data, we need to do short-packet handling
                                        self.dma_bus
                                            .write_bulk(normal_data.data_pointer, &data[..]);

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

                        self.event_sender.send(transfer_event);
                    }

                    processing_result
                }
                NormalSubmissionState::NoTrbSubmitted => unreachable!(),
            }
        })
    }

    fn cancel(&mut self) {
        self.real_ep.cancel();
    }

    fn clear_halt(&mut self) {
        self.real_ep.clear_halt();
    }
}
