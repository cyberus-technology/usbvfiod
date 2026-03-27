use std::{fmt::Debug, future::Future, pin::Pin, sync::Arc};

use tokio::{runtime, select, sync::Mutex};
use tokio_util::sync::CancellationToken;

use crate::device::{
    pci::trb::{CompletionCode, EventTrb, RawTrb},
    xhci::{
        endpoint_handle::{DummyEndpointHandle, EndpointHandle, TrbProcessingResult},
        interrupter::EventSender,
    },
};

// same as TrbProcessingResult but without the Disconnect variant.
// The endpoint state machine should not now about real-device disconnects;
// TRBs to detached device should look like TransactionErrors to the endpoint
// state machine.
#[derive(Debug, Clone, Copy)]
pub enum HotplugTrbProcessingResult {
    Ok,
    Stall,
    TrbError,
    TransactionError,
}

impl HotplugTrbProcessingResult {
    fn map_result(value: TrbProcessingResult) -> Self {
        match value {
            TrbProcessingResult::Ok => HotplugTrbProcessingResult::Ok,
            TrbProcessingResult::Stall => HotplugTrbProcessingResult::Stall,
            TrbProcessingResult::TrbError => HotplugTrbProcessingResult::TrbError,
            TrbProcessingResult::TransactionError => HotplugTrbProcessingResult::TransactionError,
            // A device disconnect looks like a failed transaction for the endpoint state machine
            TrbProcessingResult::Disconnect => HotplugTrbProcessingResult::TransactionError,
        }
    }
}

// trait exists to hide the EndpointHandle generic parameter from the endpoint state machine
pub trait HotplugEndpointHandle: Debug + Send + 'static {
    type TrbCompletionFuture<'a>: Future<Output = anyhow::Result<HotplugTrbProcessingResult>>
        + Send
        + 'a;
    type CompletionFuture<'a>: Future<Output = anyhow::Result<()>> + Send + 'a;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()>;
    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_>;
    fn cancel(&mut self) -> Self::CompletionFuture<'_>;
    fn clear_halt(&mut self) -> Self::CompletionFuture<'_>;
}

#[derive(Debug)]
pub struct HotplugEndpointHandleImpl<EH: EndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    endpoint_handle: Arc<Mutex<Option<EH>>>,
    event_sender: EventSender,
    notify_detach: CancellationToken,
    submission_state: HotplugSubmissionState,
}

#[derive(Debug, Default)]
enum HotplugSubmissionState {
    #[default]
    NoTrbSubmitted,
    // TRB address
    TrbSubmitted(u64),
}

impl<EH: EndpointHandle> HotplugEndpointHandleImpl<EH> {
    pub fn new(
        slot_id: u8,
        endpoint_id: u8,
        endpoint_handle: EH,
        event_sender: EventSender,
        notify_detach: CancellationToken,
        async_runtime: &runtime::Handle,
    ) -> Self {
        let endpoint_handle = Arc::new(Mutex::new(Some(endpoint_handle)));

        async_runtime.spawn(Self::detach_handler(
            endpoint_handle.clone(),
            notify_detach.clone(),
        ));

        Self {
            slot_id,
            endpoint_id,
            endpoint_handle,
            event_sender,
            notify_detach,
            submission_state: HotplugSubmissionState::NoTrbSubmitted,
        }
    }

    // wait for signal of other endpoints or the central detach.
    // Drop device when receiving notification.
    async fn detach_handler(
        endpoint_handle: Arc<Mutex<Option<EH>>>,
        notify_detach: CancellationToken,
    ) {
        notify_detach.cancelled().await;
        let mut ep = endpoint_handle.lock().await;
        *ep = None;
    }
}

impl<EH: EndpointHandle> HotplugEndpointHandle for HotplugEndpointHandleImpl<EH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<HotplugTrbProcessingResult>> + Send + 'a>>;
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        if let Ok(mut guard) = self.endpoint_handle.try_lock() {
            assert!(
                matches!(
                    self.submission_state,
                    HotplugSubmissionState::NoTrbSubmitted
                ),
                "submit_trb called twice without calling next_completion"
            );

            self.submission_state = HotplugSubmissionState::TrbSubmitted(trb.address);
            if let Some(device) = guard.as_mut() {
                device.submit_trb(trb)?;
            }
        }

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let trb_addr = match self.submission_state {
                HotplugSubmissionState::TrbSubmitted(addr) => addr,
                HotplugSubmissionState::NoTrbSubmitted => {
                    panic!("next_completion called without prior submit_trb")
                }
            };

            // the event to send in case the real endpoint is/gets detached
            let event = || {
                EventTrb::new_transfer_event_trb(
                    trb_addr,
                    0,
                    CompletionCode::UsbTransactionError,
                    false,
                    self.endpoint_id,
                    self.slot_id,
                )
            };
            let result = match self.endpoint_handle.lock().await.as_mut() {
                Some(ep) => select! {
                    result = ep.next_completion() => match result? {
                        TrbProcessingResult::Disconnect => {
                            self.notify_detach.cancel();
                            HotplugTrbProcessingResult::TransactionError
                        },
                        result => HotplugTrbProcessingResult::map_result(result),
                    },
                    _ = self.notify_detach.cancelled() => {
                        self.event_sender.send(event())?;
                        HotplugTrbProcessingResult::TransactionError
                    },
                },
                None => {
                    self.event_sender.send(event())?;
                    HotplugTrbProcessingResult::TransactionError
                }
            };

            self.submission_state = HotplugSubmissionState::NoTrbSubmitted;
            Ok(result)
        })
    }

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async {
            if let Some(device) = self.endpoint_handle.lock().await.as_mut() {
                device.cancel().await?;
            }
            Ok(())
        })
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async {
            if let Some(device) = self.endpoint_handle.lock().await.as_mut() {
                device.clear_halt().await?;
            }
            Ok(())
        })
    }
}

impl HotplugEndpointHandleImpl<DummyEndpointHandle> {
    // Necessary because AddressDevice/ConfigureEndpoint might open an endpoint while the device just recently was removed.
    // It makes no sense to fail the command then. So we create a dummy endpoint handle that behaves the same as the
    // endpoint handle of a removed device.
    pub fn dummy(slot_id: u8, endpoint_id: u8, event_sender: EventSender) -> Self {
        Self {
            slot_id,
            endpoint_id,
            event_sender,
            endpoint_handle: Arc::new(Mutex::new(None)),
            submission_state: HotplugSubmissionState::NoTrbSubmitted,
            // just a dummy; nobody will notify, nobody listens
            notify_detach: CancellationToken::new(),
        }
    }
}
