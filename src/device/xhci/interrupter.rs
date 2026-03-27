use anyhow::{anyhow, Context};
use tokio::sync::mpsc;
use tokio::{runtime, select};
use tracing_log::log::info;

use crate::device::bus::BusDeviceRef;
use crate::device::interrupt_line::{DummyInterruptLine, InterruptLine};
use crate::device::pci::constants::xhci::runtime::IMOD_DEFAULT;
use crate::device::xhci::event_ring::EventRing;
use crate::device::xhci::registers::{ErstbaRegister, GenericRwRegister};
use crate::device::xhci::trb::EventTrb;
use std::sync::Arc;

#[derive(Debug)]
pub struct Interrupter {
    pub registers: InterrupterRegisters,
    /// Transmits events to send to the worker
    msg_sender: mpsc::UnboundedSender<InterrupterMessage>,
}

#[derive(Debug, Clone)]
pub struct InterrupterRegisters {
    /// IMAN: Interrupt management register
    pub interrupt_management: GenericRwRegister,
    /// IMOD: Interrupt moderation interval
    ///
    /// The minimum interval in 250ns increments between interrupts.
    pub interrupt_moderation_interval: GenericRwRegister,
    /// ERSTBA: Event ring segment table base address
    pub erst_base_address: ErstbaRegister,
    /// ERSTSZ: Event ring segment table size
    pub erst_size: GenericRwRegister,
    /// ERDP: Event ring dequeue pointer
    pub eventring_dequeue_pointer: GenericRwRegister,
}

impl Default for InterrupterRegisters {
    fn default() -> Self {
        Self {
            interrupt_management: Default::default(),
            interrupt_moderation_interval: GenericRwRegister::new(IMOD_DEFAULT),
            erst_base_address: Default::default(),
            erst_size: Default::default(),
            eventring_dequeue_pointer: Default::default(),
        }
    }
}

#[derive(Debug)]
struct EventWorker {
    registers: InterrupterRegisters,
    msg_recv: mpsc::UnboundedReceiver<InterrupterMessage>,
    interrupt_line: Arc<dyn InterruptLine>,
    // // INTE from USBCMD
    // interrupt_enabled: AtomicBool,
    // interrupt_enable_notifier: Notify,
    // // for EINT from USBSTS
    // interrupts_pending: AtomicU16,
    event_ring: EventRing,
}

#[derive(Debug)]
enum InterrupterMessage {
    SendEvent(EventTrb),
    UpdateInterruptLine(Arc<dyn InterruptLine>),
}

#[derive(Debug, Clone)]
pub struct EventSender {
    sender: mpsc::UnboundedSender<InterrupterMessage>,
}

impl EventSender {
    pub fn send(&self, event: EventTrb) -> anyhow::Result<()> {
        let msg = InterrupterMessage::SendEvent(event);
        self.sender.send(msg).context("event channel closed")?;

        Ok(())
    }
}

impl Interrupter {
    pub fn new(
        dma_bus: BusDeviceRef,
        // interrupt_enabled: AtomicBool,
        // interrupt_enable_notifier: Notify,
        // interrupts_pending: AtomicU16,
        async_runtime: &runtime::Handle,
    ) -> Self {
        let (msg_sender, msg_recv) = mpsc::unbounded_channel();
        let registers = InterrupterRegisters::default();

        let interrupter = Self {
            registers: registers.clone(),
            msg_sender,
        };

        let event_ring = EventRing::new(dma_bus);
        let worker = EventWorker {
            registers,
            msg_recv,
            interrupt_line: Arc::new(DummyInterruptLine::default()),
            event_ring,
        };

        async_runtime.spawn(worker.run());

        interrupter
    }

    pub fn set_interrupt_line(&self, interrupt_line: Arc<dyn InterruptLine>) -> anyhow::Result<()> {
        let msg = InterrupterMessage::UpdateInterruptLine(interrupt_line);
        self.msg_sender.send(msg)?;

        Ok(())
    }

    pub fn create_event_sender(&self) -> EventSender {
        EventSender {
            sender: self.msg_sender.clone(),
        }
    }
}

impl EventWorker {
    async fn next_msg(&mut self) -> anyhow::Result<InterrupterMessage> {
        self.msg_recv
            .recv()
            .await
            .ok_or_else(|| anyhow!("event channel closed"))
    }

    async fn run(mut self) {
        match self.run_loop().await {
            Ok(_) => unreachable!(),
            Err(err) => {
                info!("EventWorker stopped {err}");
            }
        }
    }

    // function only returns on error, but cannot use ! in Result
    async fn run_loop(&mut self) -> anyhow::Result<()> {
        // first ERSTBA write starts the event ring.
        // drop all events that happen before.
        // interrupt line updates should be processed.
        loop {
            select! {
                _ = self.registers.erst_base_address.write_notification() => break,
                // we cannot use self.next_msg() here because it borrows self mutable, clashing
                // with the borrow of self.registers above
                msg = self.msg_recv.recv() => match msg.ok_or_else(|| anyhow!("event channel closed"))? {
                    InterrupterMessage::SendEvent(_) => {}
                    InterrupterMessage::UpdateInterruptLine(interrupt_line) => self.interrupt_line = interrupt_line,
                },
            }
        }
        self.event_ring.configure(
            self.registers.erst_base_address.erstba(),
            self.registers.erst_size.read() as u32,
        );

        loop {
            // process TRB
            match self.next_msg().await? {
                InterrupterMessage::SendEvent(event_trb) => {
                    self.event_ring.enqueue(
                        &event_trb,
                        self.registers.erst_base_address.erstba(),
                        self.registers.erst_size.read() as u32,
                        self.registers.eventring_dequeue_pointer.read(),
                    );
                    self.interrupt_line.interrupt();
                }
                InterrupterMessage::UpdateInterruptLine(interrupt_line) => {
                    self.interrupt_line = interrupt_line;
                }
            }
        }
    }
}
