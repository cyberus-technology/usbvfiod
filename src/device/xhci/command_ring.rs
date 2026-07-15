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
    pci::constants::xhci::operational::{crcr, usbcmd},
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
        'run_loop: loop {
            match &self.state {
                WorkerState::Stopped => match self.next_msg().await? {
                    WorkerMessage::SetDequeuePointerAndCS(dp, cs) => {
                        debug!("Updating command ring parameters: dp={dp:#x}, cs={cs}");
                        self.ring.set_dequeue_pointer(dp, cs);
                    }
                    WorkerMessage::Doorbell => {
                        let controller_running =
                            (self.usbcmd.load(Ordering::Relaxed) as u64 & usbcmd::RS) == usbcmd::RS;
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
                                continue 'run_loop;
                            }
                            msg => warn!("Unexpected message: msg={msg:?}, state={:?}", self.state),
                        }
                    }
                    let controller_stopped =
                        (self.usbcmd.load(Ordering::Relaxed) & usbcmd::RS as u32) == 0;
                    if controller_stopped {
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
                    let (dequeue_pointer, _) = self.ring.get_dequeue_pointer();
                    let event = EventTrb::new_command_completion_event_trb(
                        dequeue_pointer,
                        0,
                        CompletionCode::CommandRingStopped,
                        0,
                    );
                    self.event_sender.send(event)?;
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
            CommandTrbVariant::NoOp => EventTrb::new_command_completion_event_trb(
                trb.address,
                0,
                CompletionCode::Success,
                0,
            ),
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

#[cfg(test)]
mod tests {

    use tokio::{runtime::Handle, sync::mpsc::UnboundedReceiver};

    use crate::{
        device::{
            bus::{testutils::TestBusDevice, BusDevice},
            pci::constants::xhci::{
                operational::crcr::{self},
                rings::trb_types,
            },
            xhci::{
                interrupter::tests::testutils::MockInterrupter,
                registers::UsbcmdRegister,
                slot_manager::{test::testutils::MockSlotManager, SlotMessage},
                trb::testutils::RawTrbBuilder,
            },
        },
        dynamic_bus::DynamicBus,
    };

    use super::*;

    // We can never set a 64 bit Dequeu Pointers lowest 6 bits:
    // "This field defines high order bits of the initial value of the 64-bit Command Ring Dequeue Pointer."
    const FIRST_ADDRESS: u64 = 0x10 << 6;
    const SECOND_ADDRESS: u64 = FIRST_ADDRESS + 0x10;
    const THIRD_ADDRESS: u64 = SECOND_ADDRESS + 0x10;
    const FOURTH_ADDRESS: u64 = THIRD_ADDRESS + 0x10;

    const SLOT_ID: u8 = 0;

    /// the ring is not running
    /// dequeue_pointer points to FIRST_ADDRESS
    fn init_test() -> (
        CommandRing,
        MockInterrupter,
        UnboundedReceiver<SlotMessage>,
        Arc<DynamicBus>,
        UsbcmdRegister,
    ) {
        let dma_bus = Arc::new(DynamicBus::new());
        let dma_backing = vec![99; 16384];
        dma_bus
            .add(0x0, Arc::new(TestBusDevice::new(&dma_backing[..])))
            .expect("");
        let async_runtime = Handle::current();

        let (event_sender, interrupter) = MockInterrupter::new();

        let (slot_manager, receiver) = MockSlotManager::new();

        let usbcmd = UsbcmdRegister::new();

        let command_ring = CommandRing::new(
            dma_bus.clone(),
            &async_runtime,
            event_sender,
            slot_manager.create_slot_worker_handle(),
            usbcmd.value_reference(),
        );

        assert!(interrupter.is_empty());
        assert!(!command_ring.running.load(Ordering::Relaxed));

        // write a dequeue pointer value so the first command trb is next in line
        command_ring.control(FIRST_ADDRESS).expect("");

        (command_ring, interrupter, receiver, dma_bus, usbcmd)
    }

    #[tokio::test]
    async fn process_many_command_trb_with_one_doorbell() {
        let (command_ring, mut interrupter, mut receiver, dma_bus, usbcmd) = init_test();

        // place command trb on a ring segment
        let command_1 = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_type(trb_types::NO_OP_COMMAND)
            .build();
        let command_2 = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_type(trb_types::ENABLE_SLOT_COMMAND)
            .build();
        let command_3 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_type(trb_types::DISABLE_SLOT_COMMAND)
            .with_byte(15, SLOT_ID)
            .build();
        let command_4 = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_field(0x1 << 4)
            .with_type(trb_types::ADDRESS_DEVICE_COMMAND)
            .with_byte(15, SLOT_ID)
            .build();

        let commands = vec![command_1, command_2, command_3, command_4];

        for command in &commands {
            dma_bus.write_bulk(command.address, &command.buffer);
        }

        // start the ring through usbcmd and doorbell
        usbcmd.write(usbcmd::RS);
        command_ring.doorbell().expect("");

        // expected outcome of the command chain

        // command_1
        let event = interrupter.await_event().await.unwrap();
        let expected_event = EventTrb::new_command_completion_event_trb(
            FIRST_ADDRESS,
            0,
            CompletionCode::Success,
            SLOT_ID,
        );
        assert_eq!(event, expected_event);

        // command ring running can be checked here without race condition since
        // await above acted as "wait until succeeded"
        assert_eq!(command_ring.status(), crcr::CRR);

        // command_2
        assert!(!receiver.is_empty());
        if let SlotMessage::EnableSlot(sender) = receiver.recv().await.unwrap() {
            sender.send(Ok(SLOT_ID)).expect("");
        } else {
            panic!()
        }
        let event = interrupter.await_event().await.unwrap();
        let expected_event = EventTrb::new_command_completion_event_trb(
            SECOND_ADDRESS,
            0,
            CompletionCode::Success,
            SLOT_ID,
        );
        assert_eq!(event, expected_event);

        // command_3
        assert!(!receiver.is_empty());
        if let SlotMessage::DisableSlot(id, sender) = receiver.recv().await.unwrap() {
            assert_eq!(id, SLOT_ID);
            sender.send(CompletionCode::Success).expect("");
        } else {
            panic!()
        }
        let event = interrupter.await_event().await.unwrap();
        let expected_event = EventTrb::new_command_completion_event_trb(
            THIRD_ADDRESS,
            0,
            CompletionCode::Success,
            SLOT_ID,
        );
        assert_eq!(event, expected_event);

        // command_4
        assert!(!receiver.is_empty());
        if let SlotMessage::AddressDevice(data, sender) = receiver.recv().await.unwrap() {
            assert_eq!(data.input_context_pointer, 0x1 << 4);
            assert!(!data.block_set_address_request);
            assert_eq!(data.slot_id, SLOT_ID);
            sender.send(CompletionCode::Success).expect("");
        } else {
            panic!()
        }
        let event = interrupter.await_event().await.unwrap();
        let expected_event = EventTrb::new_command_completion_event_trb(
            FOURTH_ADDRESS,
            0,
            CompletionCode::Success,
            SLOT_ID,
        );
        assert_eq!(event, expected_event);

        assert!(interrupter.is_empty());
        assert!(receiver.is_empty());
    }

    #[tokio::test]
    async fn write_to_command_stop_bit_and_restart_with_initial_dequeue_pointer_value() {
        let (command_ring, mut interrupter, _receiver, dma_bus, usbcmd) = init_test();

        let command = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_type(trb_types::NO_OP_COMMAND)
            .build();

        dma_bus.write_bulk(command.address, &command.buffer);

        // start the ring through usbcmd and doorbell
        usbcmd.write(usbcmd::RS);
        command_ring.doorbell().expect("");

        // verify running state by waiting for the first command completion event
        if let EventTrb::CommandCompletion(event) = interrupter.await_event().await.unwrap() {
            assert!(event.get_completion_code() == CompletionCode::Success);
        } else {
            panic!()
        }

        assert_eq!(command_ring.status(), crcr::CRR);

        // we likely can expect idle state (without sleep(1) here) and transition to stopped
        command_ring.control(crcr::CS).expect("");
        if let EventTrb::CommandCompletion(event) = interrupter.await_event().await.unwrap() {
            assert!(event.get_completion_code() == CompletionCode::CommandRingStopped);
        } else {
            panic!()
        }

        // check it is stopped
        assert_eq!(command_ring.status() & crcr::CRR, 0);

        // write a dequeue pointer value so the first command trb is next in line
        command_ring.control(FIRST_ADDRESS).expect("");

        // restart
        command_ring.doorbell().expect("");

        // verify running state by waiting for the first command completion event
        if let EventTrb::CommandCompletion(event) = interrupter.await_event().await.unwrap() {
            assert!(event.get_completion_code() == CompletionCode::Success);
        } else {
            panic!()
        }

        assert_eq!(command_ring.status(), crcr::CRR);
    }

    #[tokio::test]
    async fn write_to_command_abort_bit() {
        let (command_ring, mut interrupter, _receiver, dma_bus, usbcmd) = init_test();

        let command = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_type(trb_types::NO_OP_COMMAND)
            .build();

        dma_bus.write_bulk(command.address, &command.buffer);
        dma_bus.write_bulk(SECOND_ADDRESS, &command.buffer);
        dma_bus.write_bulk(THIRD_ADDRESS, &command.buffer);
        dma_bus.write_bulk(FOURTH_ADDRESS, &command.buffer);

        // start the ring through usbcmd and doorbell
        usbcmd.write(usbcmd::RS);
        command_ring.doorbell().expect("");

        // verify running state by waiting for the first command completion event
        if let EventTrb::CommandCompletion(event) = interrupter.await_event().await.unwrap() {
            assert!(event.get_completion_code() == CompletionCode::Success);
        } else {
            panic!()
        }

        assert_eq!(command_ring.status(), crcr::CRR);

        // abort ring operations
        //
        // As time of writing I have not found a surefire way to stop in between
        // the No Op Command TRB's. So we can not be sure if we are still processing
        // or are already done and switched to idle.
        command_ring.control(crcr::CA).expect("");
        loop {
            let event = interrupter.await_event().await.unwrap();
            debug!("{:?}", event);
            match event {
                EventTrb::CommandCompletion(event_trb)
                    if event_trb.get_completion_code() == CompletionCode::Success =>
                {
                    assert_eq!(command_ring.status() & crcr::CRR, crcr::CRR);
                }
                EventTrb::CommandCompletion(event_trb)
                    if event_trb.get_completion_code() == CompletionCode::CommandRingStopped =>
                {
                    assert_eq!(command_ring.status() & crcr::CRR, 0);
                    break;
                }
                _ => panic!(),
            }
        }
    }
}
