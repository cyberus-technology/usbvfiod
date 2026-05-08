use std::{fmt::Debug, future::Future, pin::Pin, sync::Arc};

use tokio::{runtime, select, sync::Mutex};
use tokio_util::sync::CancellationToken;
use tracing::trace;

use crate::device::xhci::{
    endpoint_handle::{DummyEndpointHandle, EndpointHandle, TrbProcessingResult},
    interrupter::EventSender,
    trb::{CompletionCode, EventTrb, RawTrb},
};

// same as TrbProcessingResult but without the Disconnect variant.
// The endpoint state machine should not know about real-device disconnects;
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
    const fn map_result(value: TrbProcessingResult) -> Self {
        match value {
            TrbProcessingResult::Ok => Self::Ok,
            TrbProcessingResult::Stall => Self::Stall,
            TrbProcessingResult::TrbError => Self::TrbError,
            TrbProcessingResult::TransactionError => Self::TransactionError,
            // A device disconnect looks like a failed transaction for the endpoint state machine
            TrbProcessingResult::Disconnect => Self::TransactionError,
        }
    }
}

// all traits in the endpoint abstractions implement this trait
pub trait BaseEndpointHandle: Debug + Send + 'static {
    type CompletionFuture<'a>: Future<Output = anyhow::Result<()>> + Send + 'a;

    fn cancel(&mut self) -> Self::CompletionFuture<'_>;
    fn clear_halt(&mut self) -> Self::CompletionFuture<'_>;
}

// trait exists to hide the EndpointHandle generic parameter from the endpoint state machine
pub trait HotplugEndpointHandle: BaseEndpointHandle {
    type TrbCompletionFuture<'a>: Future<Output = anyhow::Result<HotplugTrbProcessingResult>>
        + Send
        + 'a;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()>;
    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_>;
}

#[derive(Debug)]
pub struct HotplugEndpointHandleImpl<EH: EndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    endpoint_handle: Arc<Mutex<Option<EH>>>,
    event_sender: EventSender,
    notify_detach: CancellationToken,
    notify_drop: CancellationToken,
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
        let notify_drop = CancellationToken::new();

        async_runtime.spawn(Self::detach_handler(
            endpoint_handle.clone(),
            notify_detach.clone(),
            notify_drop.clone(),
        ));

        Self {
            slot_id,
            endpoint_id,
            endpoint_handle,
            event_sender,
            notify_detach,
            notify_drop,
            submission_state: HotplugSubmissionState::NoTrbSubmitted,
        }
    }

    // wait for signal of other endpoints or the central detach.
    // Drop device when receiving notification.
    async fn detach_handler(
        endpoint_handle: Arc<Mutex<Option<EH>>>,
        notify_detach: CancellationToken,
        notify_drop: CancellationToken,
    ) {
        select! {
            _ = notify_drop.cancelled() => {}
            _ = notify_detach.cancelled() => {
                let mut ep = endpoint_handle.lock().await;
                *ep = None;
            }
        }
    }
}

impl<EH: EndpointHandle> Drop for HotplugEndpointHandleImpl<EH> {
    fn drop(&mut self) {
        // make sure that detach_handler also stops
        self.notify_drop.cancel();
    }
}

impl<EH: EndpointHandle> HotplugEndpointHandle for HotplugEndpointHandleImpl<EH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<HotplugTrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        assert!(
            matches!(
                self.submission_state,
                HotplugSubmissionState::NoTrbSubmitted
            ),
            "submit_trb called twice without calling next_completion"
        );

        // independently of whether we forward the TRB to the endpoint handle, we need
        // to track that we submitted the TRB. We have to send transaction error event
        // with TRB address even if device is gone.
        self.submission_state = HotplugSubmissionState::TrbSubmitted(trb.address);

        if let Ok(mut guard) = self.endpoint_handle.try_lock() {
            if let Some(device) = guard.as_mut() {
                device.submit_trb(trb)?;
            } else {
                trace!("TRB submission for detached device");
            }
        } else {
            trace!("try_lock failed during submit_trb: detach_handler currently has the lock");
            // After the lock release, the device will be gone, so no need to actually acquire the lock.
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
}

impl<EH: EndpointHandle> BaseEndpointHandle for HotplugEndpointHandleImpl<EH> {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async {
            if let Ok(mut guard) = self.endpoint_handle.try_lock() {
                if let Some(device) = guard.as_mut() {
                    device.cancel().await?;
                } else {
                    trace!("cancel for detached device");
                }
            } else {
                trace!("try_lock failed during cancel: detach_handler currently has the lock");
                // After the lock release, the device will be gone, so no need to actually acquire the lock.
            }

            Ok(())
        })
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async {
            if let Ok(mut guard) = self.endpoint_handle.try_lock() {
                if let Some(device) = guard.as_mut() {
                    device.clear_halt().await?;
                } else {
                    trace!("clear_halt for detached device");
                }
            } else {
                trace!("try_lock failed during clear_halt: detach_handler currently has the lock");
                // After the lock release, the device will be gone, so no need to actually acquire the lock.
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
            // just a dummy; the drop implementation will notify, but nobody listens
            notify_drop: CancellationToken::new(),
        }
    }
}
