//! Emulation of a USB3 Host (XHCI) controller.
//!
//! The specification is available
//! [here](https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf).

use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use crate::device::{
    bus::{BusDeviceRef, Request, SingleThreadedBusDevice},
    interrupt_line::{DummyInterruptLine, InterruptLine},
    pci::{
        config_space::{ConfigSpace, ConfigSpaceBuilder},
        constants::xhci::{
            capability, offset,
            operational::{crcr, portsc},
            runtime, MAX_INTRS, MAX_SLOTS, OP_BASE, RUN_BASE,
        },
        traits::PciDevice,
    },
};

use super::{config_space::BarInfo, rings::EventRing};

/// The emulation of a XHCI controller.
#[derive(Debug, Clone)]
pub struct XhciController {
    /// A reference to the VM memory to perform DMA on.
    #[allow(unused)]
    dma_bus: BusDeviceRef,

    /// The PCI Configuration Space of the controller.
    config_space: ConfigSpace,

    /// The current Run/Stop status of the controller.
    running: bool,

    /// The current Run/Stop status of the command ring.
    command_ring_running: bool,

    /// Internal Command Ring position.
    command_ring_dequeue_pointer: u64,

    /// The Event Ring of the single Interrupt Register Set.
    event_ring: EventRing,

    /// Internal Consumer Cycle State for the next TRB fetch.
    consumer_cycle_state: bool,

    /// Configured device slots.
    slots: Vec<()>,

    /// Device Context Array
    /// TODO: currently just the raw pointer configured by the OS
    device_contexts: Vec<u64>,

    /// Interrupt management register
    interrupt_management: u64,

    /// The minimum interval in 250ns increments between interrupts.
    interrupt_moderation_interval: u64,

    /// The interrupt line triggered to signal device events.
    interrupt_line: Arc<dyn InterruptLine>,
}

impl XhciController {
    /// Create a new XHCI controller with default settings.
    ///
    /// `dma_bus` is the device on which we will perform DMA
    /// operations. This is typically VM guest memory.
    #[must_use]
    pub fn new(dma_bus: BusDeviceRef) -> Self {
        use crate::device::pci::constants::config_space::*;

        Self {
            dma_bus,
            config_space: ConfigSpaceBuilder::new(vendor::REDHAT, device::REDHAT_XHCI)
                .class(class::SERIAL, subclass::SERIAL_USB, progif::USB_XHCI)
                // TODO Should be a 64-bit BAR.
                .mem32_nonprefetchable_bar(0, 4 * 0x1000)
                .mem32_nonprefetchable_bar(3, 2 * 0x1000)
                .msix_capability(MAX_INTRS.try_into().unwrap(), 3, 0, 3, 0x1000)
                .config_space(),
            running: false,
            command_ring_running: false,
            command_ring_dequeue_pointer: 0,
            consumer_cycle_state: false,
            event_ring: EventRing::default(),
            slots: vec![],
            device_contexts: vec![],
            interrupt_management: 0,
            interrupt_moderation_interval: runtime::IMOD_DEFAULT,
            interrupt_line: Arc::new(DummyInterruptLine::default()),
        }
    }

    /// Configure the interrupt line for the controller.
    ///
    /// The [`XhciController`] uses this to issue interrupts for events.
    pub fn connect_irq(&mut self, irq: Arc<dyn InterruptLine>) {
        self.interrupt_line = irq.clone();
    }

    /// Obtain the current host controller status as defined for the `USBSTS` register.
    #[must_use]
    pub fn status(&self) -> u64 {
        !u64::from(self.running) & 0x1u64
    }

    /// Obtain the current command ring status as defined for reading the `CRCR` register.
    #[must_use]
    pub fn command_ring_status(&self) -> u64 {
        // All fields except CRR (command ring running) read as zero.
        (u64::from(self.command_ring_running) << 3) & 0b100
    }

    /// Obtain the current host controller configuration as defined for the `CONFIG` register.
    #[must_use]
    pub fn config(&self) -> u64 {
        u64::try_from(self.slots.len()).unwrap() & 0x8u64
    }

    /// Enable device slots.
    pub fn enable_slots(&mut self, count: u64) {
        assert!(count <= MAX_SLOTS);

        self.slots = (0..count).map(|_| ()).collect();

        debug!("enabled {} device slots", self.slots.len());
    }

    /// Configure the device context array from the array base pointer.
    pub fn configure_device_contexts(&mut self, device_context_base_array_ptr: u64) {
        debug!(
            "configuring device contexts from pointer {:#x}",
            device_context_base_array_ptr
        );
        self.device_contexts.clear();
        self.device_contexts.push(device_context_base_array_ptr);
    }

    /// Start/Stop controller operation
    ///
    /// This is called for writes of the `USBCMD` register.
    pub fn run(&mut self, usbcmd: u64) {
        self.running = usbcmd & 0x1 == 0x1;
        if self.running {
            debug!("controller started with cmd {usbcmd:#x}");

            // XXX: This is just a test to see if we can generate interrupts.
            // This will be removed once we generate interrupts in the right
            // place, (e.g. generate a Port Connect Status Event) and test it.
            self.interrupt_line.interrupt();
            debug!("signalled a bogus interrupt");
        } else {
            debug!("controller stopped with cmd {usbcmd:#x}");
        }
    }

    /// Handle Command Ring Control Register (CRCR) updates.
    pub fn update_command_ring(&mut self, value: u64) {
        if self.command_ring_running {
            match value {
                abort if abort & crcr::CA != 0 => todo!(),
                stop if stop & crcr::CS != 0 => todo!(),
                ignored => {
                    warn!(
                        "received useless write to CRCR while running {:#x}",
                        ignored
                    )
                }
            }
        } else {
            let dequeue_ptr = value & crcr::DEQUEUE_POINTER_MASK;
            if self.command_ring_dequeue_pointer != dequeue_ptr {
                debug!(
                    "updating command ring dequeue ptr from {:#x} to {:#x}",
                    self.command_ring_dequeue_pointer, dequeue_ptr
                );
                self.command_ring_dequeue_pointer = dequeue_ptr;
            }
            // Update internal consumer cycle state for next TRB fetch.
            self.consumer_cycle_state = value & crcr::RCS != 0;
        }
    }
}

impl PciDevice for Mutex<XhciController> {
    fn write_cfg(&self, req: Request, value: u64) {
        self.lock().unwrap().config_space.write(req, value);
    }

    fn read_cfg(&self, req: Request) -> u64 {
        self.lock().unwrap().config_space.read(req)
    }

    fn write_io(&self, region: u32, req: Request, value: u64) {
        // The XHCI Controller has a single MMIO BAR.
        assert_eq!(region, 0);

        match req.addr {
            // xHC Operational Registers
            offset::USBCMD => self.lock().unwrap().run(value),
            offset::DNCTL => assert_eq!(value, 2, "debug notifications not supported"),
            offset::CRCR => self.lock().unwrap().update_command_ring(value),
            offset::CRCR_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::DCBAAP => self.lock().unwrap().configure_device_contexts(value),
            offset::DCBAAP_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::CONFIG => self.lock().unwrap().enable_slots(value),

            offset::PORTSC => assert_eq!(
                value & !portsc::WAKE_ON_EVENTS,
                portsc::DEFAULT,
                "port reconfiguration not yet supported"
            ),

            // xHC Runtime Registers
            offset::IMAN => self.lock().unwrap().interrupt_management = value,
            offset::IMOD => self.lock().unwrap().interrupt_moderation_interval = value,
            offset::ERSTSZ => assert_eq!(value, 1, "only a single segment supported"),
            offset::ERSTBA => {
                let mut xhci = self.lock().unwrap();
                let dma_bus = xhci.dma_bus.clone();
                xhci.event_ring
                    .configure_event_ring_segment_table(value, dma_bus)
            }
            offset::ERSTBA_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::ERDP => self
                .lock()
                .unwrap()
                .event_ring
                .update_dequeue_pointer(value),
            offset::ERDP_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            _ => todo!(),
        }
    }

    fn read_io(&self, region: u32, req: Request) -> u64 {
        // The XHCI Controller has a single MMIO BAR.
        assert_eq!(region, 0);

        match req.addr {
            // xHC Capability Registers
            offset::CAPLENGTH => OP_BASE,
            offset::HCIVERSION => capability::HCIVERSION,
            offset::HCSPARAMS1 => capability::HCSPARAMS1,
            offset::HCSPARAMS2 => 0, /* ERST Max size is a single segment */
            offset::HCSPARAMS3 => 0,
            offset::HCCPARAMS1 => capability::HCCPARAMS1,
            offset::DBOFF => 0x2000,
            offset::RTSOFF => RUN_BASE,
            offset::HCCPARAMS2 => 0,

            // xHC Extended Capability ("Supported Protocols Capability")
            offset::SUPPORTED_PROTOCOLS => capability::supported_protocols::CAP_INFO,
            offset::SUPPORTED_PROTOCOLS_CONFIG => capability::supported_protocols::CONFIG,

            // xHC Operational Registers
            offset::USBCMD => 0,
            offset::USBSTS => self.lock().unwrap().status(),
            offset::DNCTL => 2,
            offset::CRCR => self.lock().unwrap().command_ring_status(),
            offset::CRCR_HI => 0,
            offset::PAGESIZE => 0x1, /* 4k Pages */
            offset::CONFIG => self.lock().unwrap().config(),

            offset::PORTSC => portsc::DEFAULT,
            offset::PORTLI => 0,

            // xHC Runtime Registers
            offset::IMAN => self.lock().unwrap().interrupt_management,
            offset::IMOD => self.lock().unwrap().interrupt_moderation_interval,
            offset::ERSTSZ => 1,
            offset::ERSTBA => self.lock().unwrap().event_ring.read_base_address(),
            offset::ERSTBA_HI => 0,
            offset::ERDP => self.lock().unwrap().event_ring.read_dequeue_pointer(),
            offset::ERDP_HI => 0,

            // Everything else is Reserved Zero
            _ => todo!(),
        }
    }

    fn bar(&self, bar_no: u8) -> Option<BarInfo> {
        self.lock().unwrap().config_space.bar(bar_no)
    }
}
