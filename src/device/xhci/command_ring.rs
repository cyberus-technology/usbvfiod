//! Implements a XHCI command ring and a worker task that services th ring.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use tokio::{
    runtime,
    sync::mpsc::{self, error::TryRecvError},
};
use tracing::{debug, warn};

use crate::device::{
    bus::BusDeviceRef,
    pci::{
        constants::xhci::{operational::crcr, rings::TRB_SIZE},
        trb::{zeroed_trb_buffer, CommandTrb, CommandTrbVariant, EventTrb, RawTrbBuffer},
    },
};

#[derive(Debug)]
pub struct CommandRing {
    running: Arc<AtomicBool>,
    sender_to_worker: mpsc::Sender<WorkerMessage>,
    async_runtime: runtime::Handle,
}

#[derive(Debug)]
struct CommandWorker {
    dma_bus: BusDeviceRef,
    state: WorkerState,
    receiver: mpsc::Receiver<WorkerMessage>,
    running: Arc<AtomicBool>,
    event_ring_sender: mpsc::Sender<EventTrb>,
    dequeue_pointer: u64,
    cycle_state: bool,
}

#[derive(Debug)]
enum WorkerState {
    Stopped,
    JustStarted,
    WaitingForDoorbell,
    LookingForNewCommand,
    ProcessingCommand(CommandTrb),
    Stopping,
}

#[derive(Debug)]
enum WorkerMessage {
    SetDequeuePointerAndCS(u64, bool),
    Start,
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
        async_runtime: runtime::Handle,
        event_ring_sender: mpsc::Sender<EventTrb>,
    ) -> Self {
        let (sender_to_worker, receiver) = mpsc::channel(10);
        let running = Arc::new(AtomicBool::new(false));

        let worker = CommandWorker {
            dma_bus,
            state: WorkerState::Stopped,
            receiver,
            running: running.clone(),
            event_ring_sender,
            dequeue_pointer: 0,
            cycle_state: false,
        };
        async_runtime.spawn(worker.run());

        Self {
            running,
            sender_to_worker,
            async_runtime,
        }
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

            if value & crcr::CRR != 0 {
                self.send_to_worker(WorkerMessage::Start);
            }
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
        match self.sender_to_worker.try_send(msg) {
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
                        self.dequeue_pointer = dp;
                        self.cycle_state = cs;
                    }
                    WorkerMessage::Start => todo!(),
                    msg => warn!("Unexpected message: msg={msg:?}, state={:?}", self.state),
                },
                WorkerState::JustStarted => match self.recv().await {
                    WorkerMessage::Doorbell => {
                        self.running.store(true, Ordering::Relaxed);
                        self.state = WorkerState::LookingForNewCommand;
                    }
                    WorkerMessage::Stop => self.state = WorkerState::Stopped,
                    msg => warn!("Unexpected message: msg={msg:?}, state={:?}", self.state),
                },
                WorkerState::WaitingForDoorbell => match self.recv().await {
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
                            WorkerMessage::Doorbell => {}
                            WorkerMessage::Stop => {
                                self.state = WorkerState::Stopping;
                                break;
                            }
                            msg => warn!("Unexpected message: msg={msg:?}, state={:?}", self.state),
                        }
                    }

                    // check for TRB
                    self.state = match self.next_command_trb() {
                        Some(trb) => WorkerState::ProcessingCommand(trb),
                        None => WorkerState::WaitingForDoorbell,
                    };
                }
                WorkerState::ProcessingCommand(_) => {
                    self.process_command();
                    self.state = WorkerState::LookingForNewCommand;
                }
                WorkerState::Stopping => {
                    self.running.store(true, Ordering::Relaxed);
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

    fn process_command(&self) {
        assert!(
            matches!(self.state, WorkerState::ProcessingCommand(_)),
            "process_command called in state {:?}",
            self.state
        );

        if let WorkerState::ProcessingCommand(trb) = &self.state {
            todo!("process commands {trb:?}");
        }
    }

    /// Try to retrieve a new command from the command ring.
    ///
    /// This function only returns `CommandTrb`s that represent commands,
    /// i.e., it will not return Link TRBs. Instead, Link TRBs are handled
    /// correctly, which is the reason why the function might read two TRBs to
    /// return a single one.
    pub fn next_command_trb(&mut self) -> Option<CommandTrb> {
        // retrieve TRB at dequeue pointer and return None if there is no fresh
        // TRB
        let first_trb_buffer = self.next_trb_buffer()?;
        let first_trb = CommandTrbVariant::parse(first_trb_buffer);

        let final_trb = match first_trb {
            CommandTrbVariant::Link(link_data) => {
                // encountered Link TRB
                // update command ring status
                self.dequeue_pointer = link_data.ring_segment_pointer;
                if link_data.toggle_cycle {
                    self.cycle_state = !self.cycle_state;
                }
                // lookup first TRB in the new memory segment
                let second_trb_buffer = self.next_trb_buffer()?;
                let second_trb = CommandTrbVariant::parse(second_trb_buffer);
                if matches!(second_trb, CommandTrbVariant::Link(_)) {
                    panic!("Link TRB should not follow directly after another Link TRB");
                }
                second_trb
            }
            _ => first_trb,
        };

        let address = self.dequeue_pointer;

        // advance to next TRB
        self.dequeue_pointer = self.dequeue_pointer.wrapping_add(TRB_SIZE as u64);

        // return parsed result
        Some(CommandTrb {
            address,
            variant: final_trb,
        })
    }

    /// Try to retrieve a fresh command TRB buffer from the command ring.
    fn next_trb_buffer(&self) -> Option<RawTrbBuffer> {
        // retrieve TRB at current dequeue_pointer
        let mut trb_buffer = zeroed_trb_buffer();
        self.dma_bus
            .read_bulk(self.dequeue_pointer, &mut trb_buffer);

        debug!(
            "interpreting TRB at dequeue pointer; cycle state = {}, TRB = {:?}",
            self.cycle_state as u8, trb_buffer
        );

        // check if the TRB is fresh
        let cycle_bit = trb_buffer[12] & 0x1 != 0;
        if cycle_bit != self.cycle_state {
            // cycle-bit mismatch: no new command TRB available
            return None;
        }

        // TRB is fresh; return it
        Some(trb_buffer)
    }
}

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
