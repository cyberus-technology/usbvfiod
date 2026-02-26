use std::{fmt::Debug, future::Future, mem, ops::ControlFlow, pin::Pin, sync::Arc};

use tokio::{
    runtime, select,
    sync::{mpsc, Mutex, Notify},
};

use crate::device::{
    bus::BusDeviceRef,
    pci::{
        trb::{CompletionCode, EventTrb, RawTrb, TransferTrb, TransferTrbVariant},
        usbrequest::UsbRequest,
    },
    xhci::real_endpoint_handle::{
        ControlRequestProcessingResult, InTrbProcessingResult, OutTrbProcessingResult,
        RealControlEndpointHandle, RealInEndpointHandle, RealOutEndpointHandle,
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
    notify_detach: Arc<Notify>,
}

impl HotplugEndpointHandle {
    pub fn new(
        endpoint_handle: Box<dyn EndpointHandle>,
        notify_detach: Arc<Notify>,
        async_runtime: runtime::Handle,
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
                            detach_notify_clone.notify_waiters();
                            TrbProcessingResult::TransactionError
                        },
                        result => result,
                    },
                    _ = detach_notify_clone.notified() => TrbProcessingResult::TransactionError,
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
        notify_detach: Arc<Notify>,
    ) {
        notify_detach.notified().await;
        let mut ep = endpoint_handle.lock().await;
        *ep = None;
    }
}

#[derive(Debug)]
struct ControlEndpointHandle<RCEH: RealControlEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    real_ep: RCEH,
    trb_parser: ControlRequestParser,
    dma_bus: BusDeviceRef,
    event_sender: mpsc::Sender<EventTrb>,
    submission_state: ControlSubmissionState,
    last_request: Option<UsbRequest>,
    last_trb_address: Option<u64>,
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
enum NextCompletion {
    None,
    Immediate(TrbProcessingResult),
    AwaitRealTransfer,
}

#[derive(Debug)]
struct OutEndpointHandle<ROEH: RealOutEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    real_ep: ROEH,
    dma_bus: BusDeviceRef,
    event_sender: mpsc::Sender<EventTrb>,
    trb_parser: ControlRequestParser,
    next_completion: NextCompletion,
    last_trb: Option<TransferTrb>,
}

impl<ROEH: RealOutEndpointHandle> EndpointHandle for OutEndpointHandle<ROEH> {
    fn submit_trb(&mut self, trb: RawTrb) {
        let transfer_trb = TransferTrbVariant::parse(trb.buffer);
        match &transfer_trb {
            TransferTrbVariant::Normal(normal_data) => {
                let mut data = vec![0; normal_data.transfer_length as usize];
                self.dma_bus.read_bulk(normal_data.data_pointer, &mut data);
                self.real_ep.submit(data);
                self.next_completion = NextCompletion::AwaitRealTransfer;
                self.last_trb = Some(TransferTrb {
                    address: trb.address,
                    variant: transfer_trb,
                });
            }
            _ => self.next_completion = NextCompletion::Immediate(TrbProcessingResult::TrbError),
        }
    }

    fn next_completion(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = TrbProcessingResult> + Send + '_>> {
        Box::pin(async {
            let (completion_code, trb_processing_result) = match self.next_completion {
                NextCompletion::None => panic!("next_completion called without prior submit_trb"),
                NextCompletion::Immediate(TrbProcessingResult::TrbError) => {
                    (CompletionCode::TrbError, TrbProcessingResult::TrbError)
                }
                NextCompletion::Immediate(_) => unreachable!(),
                NextCompletion::AwaitRealTransfer => match self.real_ep.next_completion().await {
                    OutTrbProcessingResult::Disconnect => (
                        CompletionCode::UsbTransactionError,
                        TrbProcessingResult::Disconnect,
                    ),
                    OutTrbProcessingResult::Stall => {
                        (CompletionCode::StallError, TrbProcessingResult::Stall)
                    }
                    OutTrbProcessingResult::TransactionError => (
                        CompletionCode::UsbTransactionError,
                        TrbProcessingResult::TransactionError,
                    ),
                    OutTrbProcessingResult::Success => {
                        (CompletionCode::Success, TrbProcessingResult::Ok)
                    }
                },
            };

            let last_trb = self
                .last_trb
                .take()
                .expect("next_completion called without prior submit_trb");

            let send_event = match last_trb.variant {
                TransferTrbVariant::Normal(normal_data) => normal_data.interrupt_on_completion,
                _ => true, //send event with TRB error about unknown TRB type
            };
            if send_event {
                let transfer_event = EventTrb::new_transfer_event_trb(
                    last_trb.address,
                    0,
                    completion_code,
                    false,
                    self.endpoint_id,
                    self.slot_id,
                );
                self.event_sender.send(transfer_event);
            }

            trb_processing_result
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
struct InEndpointHandle<RIEH: RealInEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    real_ep: RIEH,
    dma_bus: BusDeviceRef,
    event_sender: mpsc::Sender<EventTrb>,
    submission_state: NormalSubmissionState,
}

#[derive(Debug)]
enum NormalSubmissionState {
    NoTrbSubmitted,
    UnsupportedTrbType(RawTrb),
    AwaitingRealTransfer(TransferTrb),
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
            match self.submission_state {
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
                                CompletionCode::UsbTransactionError,
                                TrbProcessingResult::Disconnect,
                            ),
                            InTrbProcessingResult::Stall => {
                                (CompletionCode::StallError, TrbProcessingResult::Stall)
                            }
                            InTrbProcessingResult::TransactionError => (
                                CompletionCode::UsbTransactionError,
                                TrbProcessingResult::TransactionError,
                            ),
                            InTrbProcessingResult::Success(data) => {
                                if let TransferTrbVariant::Normal(normal_data) =
                                    transfer_trb.variant
                                {
                                    // needs more checks.
                                    // - we should ensure we didn't receive more data than requested
                                    // - if we got less data, we need to do short-packet handling
                                    self.dma_bus.write_bulk(normal_data.data_pointer, &data[..]);
                                }
                                (CompletionCode::Success, TrbProcessingResult::Ok)
                            }
                        };

                    let transfer_event = EventTrb::new_transfer_event_trb(
                        transfer_trb.address,
                        0,
                        completion_code,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(transfer_event);

                    processing_result
                }
                NormalSubmissionState::NoTrbSubmitted => unreachable!(),
            }
        });

        Box::pin(async {
            let last_trb = self
                .last_trb
                .take()
                .expect("next_completion called without prior submit_trb");

            let (completion_code, trb_processing_result) = match self.next_completion {
                NextCompletion::None => panic!("next_completion called without prior submit_trb"),
                NextCompletion::Immediate(TrbProcessingResult::TrbError) => {
                    (CompletionCode::TrbError, TrbProcessingResult::TrbError)
                }
                NextCompletion::Immediate(_) => unreachable!(),
                NextCompletion::AwaitRealTransfer => match self.real_ep.next_completion().await {
                    InTrbProcessingResult::Disconnect => (
                        CompletionCode::UsbTransactionError,
                        TrbProcessingResult::Disconnect,
                    ),
                    InTrbProcessingResult::Stall => {
                        (CompletionCode::StallError, TrbProcessingResult::Stall)
                    }
                    InTrbProcessingResult::TransactionError => (
                        CompletionCode::UsbTransactionError,
                        TrbProcessingResult::TransactionError,
                    ),
                    InTrbProcessingResult::Success(data) => {
                        if let TransferTrbVariant::Normal(normal_data) = last_trb.variant {
                            // needs more checks.
                            // - we should ensure we didn't receive more data than requested
                            // - if we got less data, we need to do short-packet handling
                            self.dma_bus.write_bulk(normal_data.data_pointer, &data[..]);
                        }
                        (CompletionCode::Success, TrbProcessingResult::Ok)
                    }
                },
            };

            let transfer_event = EventTrb::new_transfer_event_trb(
                last_trb.address,
                0,
                completion_code,
                false,
                self.endpoint_id,
                self.slot_id,
            );
            self.event_sender.send(transfer_event);

            trb_processing_result
        })
    }

    fn cancel(&mut self) {
        self.real_ep.cancel();
    }

    fn clear_halt(&mut self) {
        self.real_ep.clear_halt();
    }
}
