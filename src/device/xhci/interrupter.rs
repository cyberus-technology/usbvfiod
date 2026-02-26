use tokio::sync::mpsc;
use tokio::{runtime, select};

use crate::device::bus::BusDeviceRef;
use crate::device::interrupt_line::{DummyInterruptLine, InterruptLine};
use crate::device::pci::constants::xhci::runtime::IMOD_DEFAULT;
use crate::device::pci::registers::{ErstbaRegister, GenericRwRegister};
use crate::device::pci::trb::EventTrb;
use crate::device::xhci::event_ring::EventRing;
use std::sync::Arc;

#[derive(Debug)]
pub struct Interrupter {
    pub registers: InterrupterRegisters,
    /// Transmits events to send to the worker
    msg_sender: mpsc::Sender<InterrupterMessage>,
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
    msg_recv: mpsc::Receiver<InterrupterMessage>,
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
    sender: mpsc::Sender<InterrupterMessage>,
}

impl EventSender {
    pub fn send(&self, event: EventTrb) {
        let msg = InterrupterMessage::SendEvent(event);
        self.sender.send(msg);
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
        let (msg_sender, msg_recv) = mpsc::channel(10);
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

    pub fn set_interrupt_line(&self, interrupt_line: Arc<dyn InterruptLine>) {
        let msg = InterrupterMessage::UpdateInterruptLine(interrupt_line);
        self.msg_sender.send(msg);
    }

    pub fn create_event_sender(&self) -> EventSender {
        EventSender {
            sender: self.msg_sender.clone(),
        }
    }
}

impl EventWorker {
    async fn run(mut self) -> ! {
        // first ERSTBA write starts the event ring.
        // drop all events that happen before.
        // interrupt line updates should be processed.
        loop {
            select! {
                _ = self.registers.erst_base_address.write_notification() => break,
                msg = self.msg_recv.recv() => match msg.expect("channel should never close") {
                    InterrupterMessage::SendEvent(_) => {}
                    InterrupterMessage::UpdateInterruptLine(interrupt_line) => self.interrupt_line = interrupt_line,
                },
            }
        }
        self.event_ring.configure(
            self.registers.erst_base_address.read(),
            self.registers.erst_size.read() as u32,
        );

        loop {
            // process TRB
            match self
                .msg_recv
                .recv()
                .await
                .expect("The channel should never close.")
            {
                InterrupterMessage::SendEvent(event_trb) => {
                    self.event_ring.enqueue(
                        &event_trb,
                        self.registers.erst_base_address.read(),
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
