use tokio::{
    runtime, select,
    sync::{mpsc, oneshot},
};
use tracing::warn;

use crate::device::{
    bus::BusDeviceRef,
    pci::constants::xhci::device_slots::endpoint_state,
    xhci::{
        endpoint_handle::{HotplugEndpointHandle, TrbProcessingResult},
        linked_ring::LinkedRing,
        slot_manager::EndpointContext,
    },
};

#[derive(Debug)]
pub struct EndpointWorker {
    state: WorkerState,
    context: EndpointContext,
    transfer_ring: LinkedRing,
    recv: mpsc::UnboundedReceiver<EndpointMessage>,
    real_endpoint: HotplugEndpointHandle,
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
    SettingTrDequeuePointer(u64, bool, oneshot::Sender<()>),
    Stopped,
    Terminating(oneshot::Sender<()>),
}

#[derive(Debug)]
pub enum EndpointMessage {
    Doorbell,
    Stop(oneshot::Sender<()>),
    Reset,
    // contains the new pointer
    SetTrDequeuePointer(u64, bool, oneshot::Sender<()>),
    Terminate(oneshot::Sender<()>),
}

impl EndpointWorker {
    pub fn launch(
        async_runtime: &runtime::Handle,
        dma_bus: BusDeviceRef,
        trb_consumer: HotplugEndpointHandle,
        context: EndpointContext,
    ) -> mpsc::UnboundedSender<EndpointMessage> {
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

        sender
    }
}

impl EndpointWorker {
    async fn run(mut self) {
        loop {
            match self.state {
                WorkerState::WaitForDoorbell => match self.next_msg().await {
                    EndpointMessage::Doorbell => self.state = WorkerState::LookForTrb,
                    EndpointMessage::Terminate(sender) => {
                        self.state = WorkerState::Terminating(sender)
                    }
                    msg => warn!("invalid endpoint action: {msg:?} in state {:?}", self.state),
                },
                WorkerState::LookForTrb => {
                    if let Some(trb) = self.transfer_ring.next_trb() {
                        self.real_endpoint.submit_trb(trb);
                        self.state = WorkerState::WaitForTrbCompletion;
                    } else {
                        self.state = WorkerState::WaitForDoorbell;
                    }
                }
                WorkerState::WaitForTrbCompletion => select! {
                    result = self.real_endpoint.next_completion() => match result {
                        TrbProcessingResult::Ok => {
                            self.transfer_ring.advance();
                            self.state = WorkerState::LookForTrb;
                        },
                        TrbProcessingResult::Stall => {
                            self.context.set_state(endpoint_state::HALTED);
                            let (dequeue_pointer, cycle_state) = self.transfer_ring.get_dequeue_pointer();
                            self.context.set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
                            self.state = WorkerState::Halted;
                        },
                        TrbProcessingResult::TransactionError => {
                            self.context.set_state(endpoint_state::HALTED);
                            let (dequeue_pointer, cycle_state) = self.transfer_ring.get_dequeue_pointer();
                            self.context.set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
                            self.state = WorkerState::Halted;
                        },
                        TrbProcessingResult::TrbError => {
                            self.context.set_state(endpoint_state::ERROR);
                            let (dequeue_pointer, cycle_state) = self.transfer_ring.get_dequeue_pointer();
                            self.context.set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
                            self.state = WorkerState::Error;
                        },
                        TrbProcessingResult::Disconnect => {
                            unreachable!();
                        },
                    },
                    msg = self.recv.recv() => match msg.expect("Endpoint communication channel must never close during operation") {
                        EndpointMessage::Stop(completion) => {
                            self.state = WorkerState::StoppedWithContinuableTrb;
                            completion.send(());
                            },
                        msg => warn!("invalid endpoint action: {msg:?} in state {:?}", self.state),
                    }
                },
                WorkerState::Halted => match self.next_msg().await {
                    EndpointMessage::Reset => {
                        self.real_endpoint.clear_halt();
                        self.context.set_state(endpoint_state::STOPPED);
                        self.state = WorkerState::Stopped;
                    }
                    msg => warn!("invalid endpoint action: {msg:?} in state {:?}", self.state),
                },
                WorkerState::Error => match self.next_msg().await {
                    EndpointMessage::SetTrDequeuePointer(ptr, cs, completion) => {
                        self.state = WorkerState::SettingTrDequeuePointer(ptr, cs, completion);
                    }
                    msg => warn!("invalid endpoint action: {msg:?} in state {:?}", self.state),
                },
                WorkerState::StoppedWithContinuableTrb => match self.next_msg().await {
                    EndpointMessage::SetTrDequeuePointer(ptr, cs, completion) => {
                        self.real_endpoint.cancel();
                        self.state = WorkerState::SettingTrDequeuePointer(ptr, cs, completion)
                    }
                    EndpointMessage::Doorbell => {
                        self.context.set_state(endpoint_state::RUNNING);
                        self.state = WorkerState::WaitForTrbCompletion;
                    }
                    msg => warn!("invalid endpoint action: {msg:?} in state {:?}", self.state),
                },
                WorkerState::Stopped => match self.next_msg().await {
                    EndpointMessage::Doorbell => self.state = WorkerState::LookForTrb,
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
                    completion.send(());
                }
                WorkerState::Terminating(sender) => {
                    self.context.set_state(endpoint_state::DISABLED);
                    sender.send(());
                    break;
                }
            }
        }
    }

    async fn next_msg(&mut self) -> EndpointMessage {
        self.recv
            .recv()
            .await
            .expect("Endpoint communication channel must never close during operation")
    }
}
