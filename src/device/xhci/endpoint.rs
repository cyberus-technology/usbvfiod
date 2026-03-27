use anyhow::anyhow;
use tokio::{
    runtime, select,
    sync::{mpsc, oneshot},
};
use tracing::{trace, warn};

use crate::{
    device::{
        bus::BusDeviceRef,
        pci::{constants::xhci::device_slots::endpoint_state, trb::CompletionCode},
        xhci::{
            hotplug_endpoint_handle::HotplugEndpointHandle,
            hotplug_endpoint_handle::HotplugTrbProcessingResult, linked_ring::LinkedRing,
            slot_manager::EndpointContext,
        },
    },
    oneshot_anyhow::SendWithAnyhowError,
};

#[derive(Debug)]
pub struct EndpointWorker<EH: HotplugEndpointHandle> {
    state: WorkerState,
    context: EndpointContext,
    transfer_ring: LinkedRing,
    recv: mpsc::UnboundedReceiver<EndpointMessage>,
    real_endpoint: EH,
}

#[derive(Debug)]
enum WorkerState {
    WaitForDoorbell,
    LookForTrb,
    WaitForTrbCompletion,
    Halted,
    Error,
    StoppedWithContinuableTrb,
    // contains the new pointer + cycle state
    SettingTrDequeuePointer(u64, bool, oneshot::Sender<CompletionCode>),
    Stopped,
    Terminating(oneshot::Sender<()>),
}

#[derive(Debug)]
pub enum EndpointMessage {
    Doorbell,
    Stop(oneshot::Sender<CompletionCode>),
    Reset(oneshot::Sender<CompletionCode>),
    // contains the new pointer
    SetTrDequeuePointer(u64, bool, oneshot::Sender<CompletionCode>),
    Terminate(oneshot::Sender<()>),
}

impl<EH: HotplugEndpointHandle> EndpointWorker<EH> {
    pub fn launch(
        async_runtime: &runtime::Handle,
        dma_bus: BusDeviceRef,
        trb_consumer: EH,
        context: EndpointContext,
    ) -> EndpointSender {
        let (sender, recv) = mpsc::unbounded_channel();

        context.set_state(endpoint_state::RUNNING);
        let (dequeue_pointer, cycle_state) = context.get_dequeue_pointer_and_cycle_state();
        let transfer_ring = LinkedRing::new(dma_bus, dequeue_pointer, cycle_state);

        let worker = EndpointWorker {
            state: WorkerState::WaitForDoorbell,
            context,
            recv,
            real_endpoint: trb_consumer,
            transfer_ring,
        };
        async_runtime.spawn(worker.run());

        EndpointSender { msg_sender: sender }
    }

    async fn run(self) {
        match self.run_loop().await {
            Ok(_) => {
                // endpoint terminated properly
            }
            Err(err) => warn!("endpoint stopped unexpectedly {err}"),
        }
    }

    async fn run_loop(mut self) -> anyhow::Result<()> {
        loop {
            match self.state {
                WorkerState::WaitForDoorbell => match self.next_msg().await? {
                    EndpointMessage::Doorbell => self.state = WorkerState::LookForTrb,
                    EndpointMessage::Stop(sender) => {
                        self.context.set_state(endpoint_state::STOPPED);
                        self.state = WorkerState::Stopped;
                        sender.send_anyhow(CompletionCode::Success)?;
                    }
                    EndpointMessage::Terminate(sender) => {
                        self.state = WorkerState::Terminating(sender)
                    }
                    msg => warn!("invalid endpoint action: {msg:?} in state {:?}", self.state),
                },
                WorkerState::LookForTrb => {
                    if let Some(trb) = self.transfer_ring.next_trb() {
                        self.real_endpoint.submit_trb(trb)?;
                        self.state = WorkerState::WaitForTrbCompletion;
                    } else {
                        self.state = WorkerState::WaitForDoorbell;
                    }
                }
                WorkerState::WaitForTrbCompletion => select! {
                    result = self.real_endpoint.next_completion() => match result? {
                        HotplugTrbProcessingResult::Ok => {
                            self.transfer_ring.advance();
                            self.state = WorkerState::LookForTrb;
                        },
                        HotplugTrbProcessingResult::Stall => {
                            self.context.set_state(endpoint_state::HALTED);
                            let (dequeue_pointer, cycle_state) = self.transfer_ring.get_dequeue_pointer();
                            self.context.set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
                            self.state = WorkerState::Halted;
                        },
                        HotplugTrbProcessingResult::TransactionError => {
                            self.context.set_state(endpoint_state::HALTED);
                            let (dequeue_pointer, cycle_state) = self.transfer_ring.get_dequeue_pointer();
                            self.context.set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
                            self.state = WorkerState::Halted;
                        },
                        HotplugTrbProcessingResult::TrbError => {
                            self.context.set_state(endpoint_state::ERROR);
                            let (dequeue_pointer, cycle_state) = self.transfer_ring.get_dequeue_pointer();
                            self.context.set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
                            self.state = WorkerState::Error;
                        },
                    },
                    // cannot use self.next_msg() because the &mut it takes clashes with the self.real_endpoint above
                    msg = self.recv.recv() => match msg.ok_or_else(|| anyhow!(""))? {
                        EndpointMessage::Stop(completion) => {
                            self.context.set_state(endpoint_state::STOPPED);
                            self.state = WorkerState::StoppedWithContinuableTrb;
                            completion.send_anyhow(CompletionCode::Success)?;
                            },
                        msg => warn!("invalid endpoint action: {msg:?} in state {:?}", self.state),
                    }
                },
                WorkerState::Halted => match self.next_msg().await? {
                    EndpointMessage::Reset(completion) => {
                        self.real_endpoint.clear_halt().await?;
                        self.context.set_state(endpoint_state::STOPPED);
                        self.state = WorkerState::Stopped;
                        completion.send_anyhow(CompletionCode::Success)?;
                    }
                    EndpointMessage::Stop(completion) => {
                        // XHCI spec 4.8.3:
                        // A Stop Endpoint Command received while an endpoint is in the Halted
                        // state shall have no effect and shall generate a Command Completion Event with
                        // the Completion Code set to Context State Error.
                        completion.send_anyhow(CompletionCode::ContextStateError)?;
                    }
                    msg => warn!("invalid endpoint action: {msg:?} in state {:?}", self.state),
                },
                WorkerState::Error => match self.next_msg().await? {
                    EndpointMessage::SetTrDequeuePointer(ptr, cs, completion) => {
                        self.state = WorkerState::SettingTrDequeuePointer(ptr, cs, completion);
                    }
                    msg => warn!("invalid endpoint action: {msg:?} in state {:?}", self.state),
                },
                WorkerState::StoppedWithContinuableTrb => match self.next_msg().await? {
                    EndpointMessage::SetTrDequeuePointer(ptr, cs, completion) => {
                        self.real_endpoint.cancel().await?;
                        self.state = WorkerState::SettingTrDequeuePointer(ptr, cs, completion)
                    }
                    EndpointMessage::Doorbell => {
                        self.context.set_state(endpoint_state::RUNNING);
                        self.state = WorkerState::WaitForTrbCompletion;
                    }
                    EndpointMessage::Terminate(sender) => {
                        self.state = WorkerState::Terminating(sender);
                    }
                    msg => warn!("invalid endpoint action: {msg:?} in state {:?}", self.state),
                },
                WorkerState::Stopped => match self.next_msg().await? {
                    EndpointMessage::Doorbell => {
                        self.context.set_state(endpoint_state::RUNNING);
                        self.state = WorkerState::LookForTrb;
                    }
                    EndpointMessage::SetTrDequeuePointer(ptr, cs, completion) => {
                        self.state = WorkerState::SettingTrDequeuePointer(ptr, cs, completion)
                    }
                    EndpointMessage::Terminate(sender) => {
                        self.state = WorkerState::Terminating(sender)
                    }
                    msg => warn!("invalid endpoint action: {msg:?} in state {:?}", self.state),
                },
                WorkerState::SettingTrDequeuePointer(ptr, cs, completion) => {
                    // we might be transitioning from Error/Halted;
                    // we could have been in Stopped* state before, then
                    // writing state is unnecessary but also not wrong.
                    self.context.set_state(endpoint_state::STOPPED);
                    self.transfer_ring.set_dequeue_pointer(ptr, cs);
                    self.state = WorkerState::Stopped;
                    completion.send_anyhow(CompletionCode::Success)?;
                }
                WorkerState::Terminating(sender) => {
                    self.context.set_state(endpoint_state::DISABLED);
                    sender.send_anyhow(())?;
                    break;
                }
            }
        }

        Ok(())
    }

    async fn next_msg(&mut self) -> anyhow::Result<EndpointMessage> {
        let msg = self
            .recv
            .recv()
            .await
            .ok_or_else(|| anyhow!("endpoint channel closed"))?;

        trace!("endpoint received: {msg:?}");

        Ok(msg)
    }
}
// Doorbell,
// Stop(oneshot::Sender<()>),
// Reset,
// // contains the new pointer
// SetTrDequeuePointer(u64, bool, oneshot::Sender<()>),
// Terminate(oneshot::Sender<()>),

#[derive(Debug, Clone)]
pub struct EndpointSender {
    msg_sender: mpsc::UnboundedSender<EndpointMessage>,
}

impl EndpointSender {
    pub fn doorbell(&self) -> anyhow::Result<()> {
        self.msg_sender.send(EndpointMessage::Doorbell)?;

        Ok(())
    }

    pub fn stop(&self, completion: oneshot::Sender<CompletionCode>) -> anyhow::Result<()> {
        self.msg_sender.send(EndpointMessage::Stop(completion))?;

        Ok(())
    }

    pub fn reset(&self, completion: oneshot::Sender<CompletionCode>) -> anyhow::Result<()> {
        self.msg_sender.send(EndpointMessage::Reset(completion))?;

        Ok(())
    }

    pub fn set_tr_dequeue_pointer(
        &self,
        dequeue_pointer: u64,
        cycle_state: bool,
        completion: oneshot::Sender<CompletionCode>,
    ) -> anyhow::Result<()> {
        self.msg_sender.send(EndpointMessage::SetTrDequeuePointer(
            dequeue_pointer,
            cycle_state,
            completion,
        ))?;

        Ok(())
    }

    pub async fn terminate(&self) -> anyhow::Result<()> {
        let (send, recv) = oneshot::channel();
        self.msg_sender.send(EndpointMessage::Terminate(send))?;
        recv.await?;

        Ok(())
    }
}
