//! Implements a XHCI command ring and a worker task that services th ring.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use tokio::{
    runtime,
    sync::{
        mpsc::{self, error::TryRecvError},
        oneshot,
    },
};
use tracing::{debug, warn};

use crate::device::{
    bus::BusDeviceRef,
    pci::{
        constants::xhci::operational::crcr,
        trb::{CommandTrb, CommandTrbVariant, CompletionCode, EventTrb},
    },
    xhci::{interrupter::EventSender, linked_ring::LinkedRing, slot_manager::SlotMessage},
};

#[derive(Debug)]
pub struct CommandRing {
    running: Arc<AtomicBool>,
    sender_to_worker: mpsc::UnboundedSender<WorkerMessage>,
}

#[derive(Debug)]
struct CommandWorker {
    state: WorkerState,
    receiver: mpsc::UnboundedReceiver<WorkerMessage>,
    running: Arc<AtomicBool>,
    event_sender: EventSender,
    ring: LinkedRing,
    slot_msg_sender: mpsc::UnboundedSender<SlotMessage>,
}

#[derive(Debug)]
enum WorkerState {
    Stopped,
    Idle,
    LookingForNewCommand,
    ProcessingCommand(CommandTrb),
    Stopping,
}

#[derive(Debug)]
enum WorkerMessage {
    SetDequeuePointerAndCS(u64, bool),
    Doorbell,
    Stop,
}

impl CommandRing {
    /// Create a new command ring.
    ///
    /// Additionally, a command worker starts running.
    ///
    /// # Parameters
    ///
    /// - dma_bus: access to guest memory
    /// - async_runtime: handle to the runtime that should start the command worker
    /// - event_ring_sender: interface to schedule command completion events onto the event ring
    pub fn new(
        dma_bus: BusDeviceRef,
        async_runtime: &runtime::Handle,
        event_sender: EventSender,
        slot_msg_sender: mpsc::UnboundedSender<SlotMessage>,
    ) -> Self {
        let (sender_to_worker, receiver) = mpsc::unbounded_channel();
        let running = Arc::new(AtomicBool::new(false));

        let ring = LinkedRing::new(dma_bus, 0, false);
        let worker = CommandWorker {
            state: WorkerState::Stopped,
            receiver,
            running: running.clone(),
            event_sender,
            ring,
            slot_msg_sender,
        };
        async_runtime.spawn(worker.run());

        Self {
            running,
            sender_to_worker,
        }
    }

    pub fn doorbell(&self) {
        debug!("Doorbell for the controller");
        self.send_to_worker(WorkerMessage::Doorbell);
    }

    /// Control the Command Ring.
    ///
    /// Call this function when the driver writes to the CRCR register.
    ///
    /// # Parameters
    ///
    /// - `value`: the value the driver wrote to the CRCR register
    pub fn control(&self, value: u64) {
        if self.running.load(Ordering::Relaxed) {
            match value {
                abort if abort & crcr::CA != 0 => self.send_to_worker(WorkerMessage::Stop),
                stop if stop & crcr::CS != 0 => self.send_to_worker(WorkerMessage::Stop),
                ignored => {
                    warn!(
                        "received useless write to CRCR while running {:#x}",
                        ignored
                    );
                }
            }
        } else {
            let dequeue_pointer = value & crcr::DEQUEUE_POINTER_MASK;
            let cycle_state = value & crcr::RCS != 0;
            self.send_to_worker(WorkerMessage::SetDequeuePointerAndCS(
                dequeue_pointer,
                cycle_state,
            ));
        }
    }

    /// Returns the current value of the `CRCR` register.
    ///
    /// All bits are zero except the CRR bit, which indicates whether the
    /// command ring is running.
    pub fn status(&self) -> u64 {
        if self.running.load(Ordering::Relaxed) {
            crcr::CRR
        } else {
            0
        }
    }

    fn send_to_worker(&self, msg: WorkerMessage) {
        match self.sender_to_worker.send(msg) {
            Ok(_) => {}
            Err(err) => {
                // The error contains the message
                warn!("Failed to send message to command worker (err: {err})");
            }
        }
    }
}

impl CommandWorker {
    async fn run(mut self) -> ! {
        loop {
            match &mut self.state {
                WorkerState::Stopped => match self.recv().await {
                    WorkerMessage::SetDequeuePointerAndCS(dp, cs) => {
                        debug!("Updating command ring parameters: dp={dp:#x}, cs={cs}");
                        self.ring.set_dequeue_pointer(dp, cs);
                    }
                    WorkerMessage::Doorbell => {
                        // TODO check R/S == 1
                        self.running.store(true, Ordering::Relaxed);
                        self.state = WorkerState::LookingForNewCommand;
                    }
                    msg => warn!("Unexpected message: msg={msg:?}, state={:?}", self.state),
                },
                WorkerState::Idle => match self.recv().await {
                    WorkerMessage::Doorbell => {
                        self.state = WorkerState::LookingForNewCommand;
                    }
                    WorkerMessage::Stop => self.state = WorkerState::Stopping,
                    msg => warn!("Unexpected message: msg={msg:?}, state={:?}", self.state),
                },
                WorkerState::LookingForNewCommand => {
                    // consume potential messages
                    loop {
                        let msg = match self.receiver.try_recv() {
                            Ok(msg) => msg,
                            Err(TryRecvError::Disconnected) => {
                                panic!("The command worker channel should never close.")
                            }
                            Err(TryRecvError::Empty) => break,
                        };

                        match msg {
                            WorkerMessage::Doorbell => {
                                // we are already active and running, silently consume
                            }
                            WorkerMessage::Stop => {
                                self.state = WorkerState::Stopping;
                                break;
                            }
                            msg => warn!("Unexpected message: msg={msg:?}, state={:?}", self.state),
                        }
                    }
                    // TODO check R/S and stop if equals 0

                    // check for TRB
                    self.state = match self.ring.next_trb() {
                        Some(trb) => {
                            let trb_data = CommandTrbVariant::parse(trb.buffer);
                            let command_trb = CommandTrb {
                                address: trb.address,
                                variant: trb_data,
                            };
                            WorkerState::ProcessingCommand(command_trb)
                        }
                        None => WorkerState::Idle,
                    };
                }
                WorkerState::ProcessingCommand(_) => {
                    self.process_command().await;
                    self.ring.advance();
                    self.state = WorkerState::LookingForNewCommand;
                }
                WorkerState::Stopping => {
                    self.running.store(false, Ordering::Relaxed);
                    self.state = WorkerState::Stopped;
                }
            }
        }
    }

    async fn recv(&mut self) -> WorkerMessage {
        self.receiver
            .recv()
            .await
            .expect("The command worker channel should never close.")
    }

    async fn process_command(&self) {
        assert!(
            matches!(self.state, WorkerState::ProcessingCommand(_)),
            "process_command called in state {:?}",
            self.state
        );

        if let WorkerState::ProcessingCommand(trb) = &self.state {
            debug!("Processing command {:?}", trb);
            let completion_event = match &trb.variant {
                CommandTrbVariant::EnableSlot => {
                    let (send, recv) = oneshot::channel();
                    let msg = SlotMessage::EnableSlot(send);
                    self.slot_msg_sender.send(msg);
                    let response = recv.await.expect("channel should never close");
                    let (slot_id, completion_code) = match response {
                        Ok(slot_id) => (slot_id, CompletionCode::Success),
                        Err(completion_error_code) => (0, completion_error_code),
                    };
                    EventTrb::new_command_completion_event_trb(
                        trb.address,
                        0,
                        completion_code,
                        slot_id,
                    )
                }
                CommandTrbVariant::DisableSlot(data) => {
                    let (send, recv) = oneshot::channel();
                    let msg = SlotMessage::DisableSlot(data.slot_id, send);
                    self.slot_msg_sender.send(msg);
                    let completion_code = recv.await.expect("channel should never close");
                    EventTrb::new_command_completion_event_trb(
                        trb.address,
                        0,
                        completion_code,
                        data.slot_id,
                    )
                }
                CommandTrbVariant::AddressDevice(data) => {
                    let (send, recv) = oneshot::channel();
                    let msg = SlotMessage::AddressDevice(*data, send);
                    self.slot_msg_sender.send(msg);
                    let completion_code = recv.await.expect("channel should never close");
                    EventTrb::new_command_completion_event_trb(
                        trb.address,
                        0,
                        completion_code,
                        data.slot_id,
                    )
                }
                CommandTrbVariant::ConfigureEndpoint(data) => {
                    let (send, recv) = oneshot::channel();
                    let msg = SlotMessage::ConfigureEndpoint(*data, send);
                    self.slot_msg_sender.send(msg);
                    let completion_code = recv.await.expect("channel should never close");
                    EventTrb::new_command_completion_event_trb(
                        trb.address,
                        0,
                        completion_code,
                        data.slot_id,
                    )
                }
                CommandTrbVariant::EvaluateContext(data) => {
                    EventTrb::new_command_completion_event_trb(
                        trb.address,
                        0,
                        CompletionCode::Success,
                        data.slot_id,
                    )
                }
                CommandTrbVariant::ResetEndpoint => todo!(),
                CommandTrbVariant::StopEndpoint(data) => {
                    EventTrb::new_command_completion_event_trb(
                        trb.address,
                        0,
                        CompletionCode::Success,
                        data.slot_id,
                    )
                }
                CommandTrbVariant::SetTrDequeuePointer(data) => {
                    EventTrb::new_command_completion_event_trb(
                        trb.address,
                        0,
                        CompletionCode::Success,
                        data.slot_id,
                    )
                }
                CommandTrbVariant::ResetDevice(data) => EventTrb::new_command_completion_event_trb(
                    trb.address,
                    0,
                    CompletionCode::Success,
                    data.slot_id,
                ),
                CommandTrbVariant::ForceHeader => todo!(),
                CommandTrbVariant::NoOp => todo!(),
                CommandTrbVariant::Unrecognized(_, trb_parse_error) => {
                    warn!("Failed to parse command TRB {trb_parse_error:?}");
                    EventTrb::new_command_completion_event_trb(
                        trb.address,
                        0,
                        CompletionCode::TrbError,
                        0,
                    )
                }
            };
            debug!("command {} finished: {completion_event:?}", trb.address);
            self.event_sender.send(completion_event);
        }
    }
}

// needs adaptation to work again
//
// #[cfg(test)]
// mod tests {
//     use crate::device::bus::testutils::TestBusDevice;

//     use super::*;

//     #[test]
//     fn command_ring_single_segment_traversal() {
//         let noop_command = [
//             0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x5c, 0x0, 0x0,
//         ];
//         let link = [
//             0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0x18, 0x0, 0x0,
//         ];

//         // construct memory segment for a ring that can contain 4 TRBs
//         let ram = Arc::new(TestBusDevice::new(&[0; 16 * 4]));
//         let mut command_ring = CommandRing::new(ram.clone());
//         command_ring.control(0x1);

//         // the ring is still empty
//         let trb = command_ring.next_command_trb();
//         assert!(
//             trb.is_none(),
//             "When no fresh command is on the command ring, next_command_trb should return None, instead got: {trb:?}"
//         );

//         // place a noop command in the first TRB slot
//         ram.write_bulk(0, &noop_command);
//         // set cycle bit
//         ram.write_bulk(12, &[0x1]);

//         // ring abstraction should parse correctly
//         let expected = Some(CommandTrb {
//             address: 0,
//             variant: CommandTrbVariant::NoOp,
//         });
//         assert_eq!(command_ring.next_command_trb(), expected);

//         // no new command placed, should return no new command
//         let trb = command_ring.next_command_trb();
//         assert!(
//             trb.is_none(),
//             "When no fresh command is on the command ring, next_command_trb should return None, instead got: {trb:?}"
//         );

//         // place two noop commands
//         ram.write_bulk(16, &noop_command);
//         ram.write_bulk(16 + 12, &[0x1]);
//         ram.write_bulk(32, &noop_command);
//         ram.write_bulk(32 + 12, &[0x1]);

//         // parse first noop
//         let expected = Some(CommandTrb {
//             address: 16,
//             variant: CommandTrbVariant::NoOp,
//         });
//         assert_eq!(command_ring.next_command_trb(), expected);

//         // parse second noop
//         let expected = Some(CommandTrb {
//             address: 32,
//             variant: CommandTrbVariant::NoOp,
//         });
//         assert_eq!(command_ring.next_command_trb(), expected);

//         // no new command placed, should return no new command
//         let trb = command_ring.next_command_trb();
//         assert!(
//             trb.is_none(),
//             "When no fresh command is on the command ring, next_command_trb should return None, instead got: {trb:?}"
//         );

//         // place link TRB back to the start of the memory segment
//         ram.write_bulk(48, &link);
//         // set cycle bit without affecting the toggle_cycle bit
//         ram.write_bulk(48 + 12, &[0x1 | link[12]]);

//         // we cannot observe it, but the dequeue_pointer should now point to 0 again and the cycle
//         // state should have toggled to false. The dequeue_pointer now points at the first written
//         // noop command. Cycle bits don't match, so the command ring should not report a new
//         // command.
//         let trb = command_ring.next_command_trb();
//         assert!(
//             trb.is_none(),
//             "When no fresh command is on the command ring, next_command_trb should return None, instead got: {trb:?}"
//         );

//         // make noop command fresh by toggling the cycle bit
//         ram.write_bulk(12, &[0x0]);

//         // parse refreshed noop
//         let expected = Some(CommandTrb {
//             address: 0,
//             variant: CommandTrbVariant::NoOp,
//         });
//         assert_eq!(command_ring.next_command_trb(), expected);
//     }
// }
