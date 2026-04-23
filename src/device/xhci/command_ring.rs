//! Implements a XHCI command ring and a worker task that services the ring.

use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};

use anyhow::anyhow;
use tokio::{
    runtime,
    sync::mpsc::{self, error::TryRecvError},
};
use tracing::{debug, info, trace, warn};

use crate::device::{
    bus::BusDeviceRef,
    pci::constants::xhci::operational::crcr,
    xhci::{
        interrupter::EventSender,
        linked_ring::LinkedRing,
        slot_manager::SlotWorkerHandle,
        trb::{CommandTrb, CommandTrbVariant, CompletionCode, EventTrb},
    },
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
    commandring_running: Arc<AtomicBool>,
    usbcmd: Arc<AtomicU32>,
    event_sender: EventSender,
    ring: LinkedRing,
    slot_handle: SlotWorkerHandle,
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
        slot_handle: SlotWorkerHandle,
        usbcmd: Arc<AtomicU32>,
    ) -> Self {
        let (sender_to_worker, receiver) = mpsc::unbounded_channel();
        let running = Arc::new(AtomicBool::new(false));

        let ring = LinkedRing::new(dma_bus, 0, false);
        let worker = CommandWorker {
            state: WorkerState::Stopped,
            receiver,
            commandring_running: running.clone(),
            usbcmd,
            event_sender,
            ring,
            slot_handle,
        };
        async_runtime.spawn(worker.run());

        Self {
            running,
            sender_to_worker,
        }
    }

    pub fn doorbell(&self) -> anyhow::Result<()> {
        debug!("Doorbell for the controller");
        self.send_to_worker(WorkerMessage::Doorbell)?;

        Ok(())
    }

    /// Control the Command Ring.
    ///
    /// Call this function when the driver writes to the CRCR register.
    ///
    /// # Parameters
    ///
    /// - `value`: the value the driver wrote to the CRCR register
    pub fn control(&self, value: u64) -> anyhow::Result<()> {
        if self.running.load(Ordering::Relaxed) {
            match value {
                abort if abort & crcr::CA != 0 => self.send_to_worker(WorkerMessage::Stop)?,
                stop if stop & crcr::CS != 0 => self.send_to_worker(WorkerMessage::Stop)?,
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
            ))?;
        }

        Ok(())
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

    fn send_to_worker(&self, msg: WorkerMessage) -> anyhow::Result<()> {
        self.sender_to_worker.send(msg)?;

        Ok(())
    }
}

impl CommandWorker {
    async fn run(mut self) {
        match self.run_loop().await {
            Ok(_) => unreachable!(),
            Err(err) => {
                info!("CommandWorker stopped {err}");
            }
        }
    }

    // function only returns on error, but cannot use ! in Result
    async fn run_loop(&mut self) -> anyhow::Result<()> {
        loop {
            match &self.state {
                WorkerState::Stopped => match self.next_msg().await? {
                    WorkerMessage::SetDequeuePointerAndCS(dp, cs) => {
                        debug!("Updating command ring parameters: dp={dp:#x}, cs={cs}");
                        self.ring.set_dequeue_pointer(dp, cs);
                    }
                    WorkerMessage::Doorbell => {
                        let controller_running = (self.usbcmd.load(Ordering::Relaxed) & 0x1) == 1;
                        if controller_running {
                            self.commandring_running.store(true, Ordering::Relaxed);
                            self.state = WorkerState::LookingForNewCommand;
                        } else {
                            warn!("received doorbell while controller is not running. Ignoring");
                        }
                    }
                    msg => warn!("Unexpected message: msg={msg:?}, state={:?}", self.state),
                },
                WorkerState::Idle => match self.next_msg().await? {
                    WorkerMessage::Doorbell => {
                        self.state = WorkerState::LookingForNewCommand;
                    }
                    WorkerMessage::Stop => self.state = WorkerState::Stopping,
                    msg => warn!("Unexpected message: msg={msg:?}, state={:?}", self.state),
                },
                WorkerState::LookingForNewCommand => {
                    // consume potential messages
                    loop {
                        let msg = match self.try_next_msg()? {
                            Some(msg) => msg,
                            None => break,
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
                    let controller_running = (self.usbcmd.load(Ordering::Relaxed) & 0x1) == 1;
                    if !controller_running {
                        trace!("Detected controller is not running; moving command ring to stopped state");
                        self.state = WorkerState::Stopped;
                        continue;
                    }

                    // check for TRB
                    self.state = self.ring.next_trb().map_or(WorkerState::Idle, |trb| {
                        let trb_data = CommandTrbVariant::parse(trb.buffer);
                        let command_trb = CommandTrb {
                            address: trb.address,
                            variant: trb_data,
                        };
                        WorkerState::ProcessingCommand(command_trb)
                    });
                }
                WorkerState::ProcessingCommand(command_trb) => {
                    self.process_command(command_trb).await?;
                    self.ring.advance();
                    self.state = WorkerState::LookingForNewCommand;
                }
                WorkerState::Stopping => {
                    self.commandring_running.store(false, Ordering::Relaxed);
                    self.state = WorkerState::Stopped;
                }
            }
        }
    }

    async fn next_msg(&mut self) -> anyhow::Result<WorkerMessage> {
        self.receiver
            .recv()
            .await
            .ok_or_else(|| anyhow!("command channel closed"))
    }

    fn try_next_msg(&mut self) -> anyhow::Result<Option<WorkerMessage>> {
        match self.receiver.try_recv() {
            Ok(msg) => Ok(Some(msg)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(anyhow!("command channel closed")),
        }
    }

    async fn process_command(&self, trb: &CommandTrb) -> anyhow::Result<()> {
        debug!("Processing command {:?}", trb);
        let completion_event = match &trb.variant {
            CommandTrbVariant::EnableSlot => {
                let (slot_id, completion_code) = match self.slot_handle.enable_slot().await? {
                    Ok(slot_id) => (slot_id, CompletionCode::Success),
                    Err(completion_error_code) => (0, completion_error_code),
                };
                EventTrb::new_command_completion_event_trb(trb.address, 0, completion_code, slot_id)
            }
            CommandTrbVariant::DisableSlot(data) => {
                let completion_code = self.slot_handle.disable_slot(data.slot_id).await?;
                EventTrb::new_command_completion_event_trb(
                    trb.address,
                    0,
                    completion_code,
                    data.slot_id,
                )
            }
            CommandTrbVariant::AddressDevice(data) => {
                let completion_code = self.slot_handle.address_device(*data).await?;
                EventTrb::new_command_completion_event_trb(
                    trb.address,
                    0,
                    completion_code,
                    data.slot_id,
                )
            }
            CommandTrbVariant::ConfigureEndpoint(data) => {
                let completion_code = self.slot_handle.configure_endpoint(*data).await?;
                EventTrb::new_command_completion_event_trb(
                    trb.address,
                    0,
                    completion_code,
                    data.slot_id,
                )
            }
            CommandTrbVariant::EvaluateContext(data) => {
                let completion_code = self.slot_handle.evaluate_context(*data).await?;
                EventTrb::new_command_completion_event_trb(
                    trb.address,
                    0,
                    completion_code,
                    data.slot_id,
                )
            }
            CommandTrbVariant::ResetEndpoint(data) => {
                let completion_code = self
                    .slot_handle
                    .reset_endpoint(data.slot_id, data.endpoint_id)
                    .await?;
                EventTrb::new_command_completion_event_trb(
                    trb.address,
                    0,
                    completion_code,
                    data.slot_id,
                )
            }
            CommandTrbVariant::StopEndpoint(data) => {
                let completion_code = self
                    .slot_handle
                    .stop_endpoint(data.slot_id, data.endpoint_id)
                    .await?;
                EventTrb::new_command_completion_event_trb(
                    trb.address,
                    0,
                    completion_code,
                    data.slot_id,
                )
            }
            CommandTrbVariant::SetTrDequeuePointer(data) => {
                let completion_code = self.slot_handle.set_tr_dequeue_pointer(*data).await?;
                EventTrb::new_command_completion_event_trb(
                    trb.address,
                    0,
                    completion_code,
                    data.slot_id,
                )
            }
            CommandTrbVariant::ResetDevice(data) => {
                let completion_code = self.slot_handle.reset_device(data.slot_id).await?;
                EventTrb::new_command_completion_event_trb(
                    trb.address,
                    0,
                    completion_code,
                    data.slot_id,
                )
            }
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
        self.event_sender.send(completion_event)?;

        Ok(())
    }
}
