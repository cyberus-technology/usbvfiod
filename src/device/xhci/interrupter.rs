use tokio::runtime;
use tokio::sync::{mpsc, Notify};

use crate::device::bus::BusDeviceRef;
use crate::device::interrupt_line::InterruptLine;
use crate::device::pci::constants::xhci::runtime::IMOD_DEFAULT;
use crate::device::pci::trb::EventTrb;
use crate::device::xhci::event_ring::EventRing;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64};
use std::sync::Arc;

#[derive(Debug)]
pub struct Interrupter {
    /// IMAN: Interrupt management register
    interrupt_management: AtomicU64,
    /// IMOD: Interrupt moderation interval
    ///
    /// The minimum interval in 250ns increments between interrupts.
    interrupt_moderation_interval: AtomicU64,
    /// ERSTBA: Event ring segment table base address
    erst_base_address: AtomicU64,
    notify_erstba_write: Notify,
    /// ERSTSZ: Event ring segment table size
    erst_size: AtomicU64,
    /// ERDP: Event ring dequeue pointer
    eventring_dequeue_pointer: AtomicU64,
    /// Transmits events to send to the worker
    event_enqueuer: mpsc::Sender<EventTrb>,
}

#[derive(Debug)]
struct EventWorker {
    interrupter: Arc<Interrupter>,
    events_to_send: mpsc::Receiver<EventTrb>,
    interrupt_line: Arc<dyn InterruptLine>,
    // INTE from USBCMD
    interrupt_enabled: AtomicBool,
    interrupt_enable_notifier: Notify,
    // for EINT from USBSTS
    interrupts_pending: AtomicU16,
    event_ring: EventRing,
}

#[derive(Debug)]
enum WorkerState {
    InterruptsDisabled,
}

impl Interrupter {
    fn new(
        dma_bus: BusDeviceRef,
        interrupt_line: Arc<dyn InterruptLine>,
        interrupt_enabled: AtomicBool,
        interrupt_enable_notifier: Notify,
        interrupts_pending: AtomicU16,
        async_runtime: runtime::Handle,
    ) -> Arc<Self> {
        let (send, recv) = mpsc::channel(10);
        let interrupter = Arc::new(Self {
            interrupt_management: AtomicU64::new(0),
            interrupt_moderation_interval: AtomicU64::new(IMOD_DEFAULT),
            event_enqueuer: send,
            erst_base_address: AtomicU64::new(0),
            erst_size: AtomicU64::new(0),
            eventring_dequeue_pointer: AtomicU64::new(0),
            notify_erstba_write: Notify::new(),
        });

        let event_ring = EventRing::new(dma_bus);
        let worker = EventWorker {
            interrupter: interrupter.clone(),
            events_to_send: recv,
            interrupt_line,
            interrupt_enabled,
            interrupt_enable_notifier,
            interrupts_pending,
            event_ring,
        };

        async_runtime.spawn(worker.run());

        interrupter
    }

    pub fn read_iman(&self) -> u64 {
        self.interrupt_management
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn write_iman(&self, value: u64) {
        self.interrupt_management
            .store(value, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn read_imod(&self) -> u64 {
        self.interrupt_moderation_interval
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn write_imod(&self, value: u64) {
        self.interrupt_moderation_interval
            .store(value, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn read_erstsz(&self) -> u32 {
        self.erst_size.load(std::sync::atomic::Ordering::Relaxed) as u32
    }

    pub fn write_erstsz(&self, value: u32) {
        self.erst_size
            .store(value as u64, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn read_erstba(&self) -> u64 {
        self.erst_base_address
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn write_erstba(&self, value: u64) {
        self.erst_base_address
            .store(value, std::sync::atomic::Ordering::Relaxed);
        self.notify_erstba_write.notify_one();
    }

    pub fn read_erdp(&self) -> u64 {
        self.eventring_dequeue_pointer
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn write_erdp(&self, value: u64) {
        self.eventring_dequeue_pointer
            .store(value, std::sync::atomic::Ordering::Relaxed);
    }
}

impl EventWorker {
    async fn run(mut self) -> ! {
        // first ERSTBA write starts the event ring
        self.interrupter.notify_erstba_write.notified().await;
        self.event_ring.configure(
            self.interrupter.read_erstba(),
            self.interrupter.read_erstsz(),
        );

        loop {
            // process TRB
            let event_trb = self
                .events_to_send
                .recv()
                .await
                .expect("The events channel should never close.");

            self.event_ring.enqueue(
                &event_trb,
                self.interrupter.read_erstba(),
                self.interrupter.read_erstsz(),
                self.interrupter.read_erdp(),
            );
            self.interrupt_line.interrupt();
        }
    }
}
