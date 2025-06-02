//! Emulation of a USB3 Host (XHCI) controller.
//!
//! The specification is available
//! [here](https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf).

use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use crate::device::{
    bus::{BusDeviceRef, Request, RequestSize, SingleThreadedBusDevice},
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

use super::config_space::BarInfo;

/// A Basic Event Ring.
#[derive(Debug, Default, Clone)]
pub struct EventRing {
    // public XHCI registers
    base_address: u64,
    dequeue_pointer: u64,
    // internal variables
    enqueue_pointer: u64,
    trb_count: u32,
    cycle_state: u8,
}

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

    portsc: u64,
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
            portsc: 0x00260203,
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
        debug!("usbsts read");
        (!u64::from(self.running) & 0x1u64) | 0x8 | 0x10
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

    /// Configure the Event Ring Segment Table from the base address.
    pub fn configure_event_ring_segment_table(&mut self, erstba: u64) {
        assert_eq!(erstba & 0x3f, 0, "unaligned event ring base address");

        self.event_ring.base_address = erstba;
        debug!("event ring segment table at {:#x}", erstba);

        self.event_ring.enqueue_pointer =
            self.dma_bus.read(Request::new(erstba, RequestSize::Size8));
        debug!(
            "initializing event ring enqueue pointer with base address of the first (and only) segment {:#x}",
            self.event_ring.enqueue_pointer
        );

        self.event_ring.trb_count =
            self.dma_bus
                .read(Request::new(erstba + 8, RequestSize::Size4)) as u32;
        debug!(
            "retrieving TRB count of the first (and only) event ring segment {}",
            self.event_ring.trb_count
        );

        self.event_ring.cycle_state = 1;
        debug!(
            "initializing event ring producer cycle state with {}",
            self.event_ring.cycle_state
        );
    }

    /// Handle writes to the Event Ring Dequeue Pointer (ERDP).
    pub fn update_event_ring(&mut self, value: u64) {
        debug!("event ring dequeue pointer advanced to {:#x}", value);
        self.event_ring.dequeue_pointer = value;
    }

    /// Start/Stop controller operation
    ///
    /// This is called for writes of the `USBCMD` register.
    pub fn run(&mut self, usbcmd: u64) {
        self.running = usbcmd & 0x1 == 0x1;
        if self.running {
            debug!("controller started with cmd {usbcmd:#x}");
            self.check_if_commands_trb_available();
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

    /// enqueue port status change event TRB on event ring and signal an interrupt
    pub fn send_port_status_change(&mut self) {
        let port_status_change_event = Self::create_port_status_change_event_trb(1);
        self.enqueue_event_trb(port_status_change_event);
        self.interrupt_line.interrupt();
        debug!("signalled an interrupt");
    }

    fn create_port_status_change_event_trb(port_id: u8) -> [u8; 16] {
        let port_status_change_event_id = 34;
        let completion_code_success = 1;
        let mut trb = [0; 16];

        trb[3] = port_id;
        trb[11] = completion_code_success;
        trb[13] = port_status_change_event_id << 2;

        trb
    }

    fn create_command_completion_event_trb(command_trb_pointer: u64) -> [u8; 16] {
        let completion_code_success = 1;
        let _command_completion_parameter = 0;
        let _slot_id = 0;
        let _vf_id = 0;
        let port_command_completion_event_id = 33;
        let mut trb = [0; 16];
        let slot_id = 1;

        trb[0..8].copy_from_slice(&command_trb_pointer.to_le_bytes());
        trb[11] = completion_code_success;
        trb[13] = port_command_completion_event_id << 2;
        trb[15] = slot_id;

        trb
    }

    fn enqueue_event_trb(&mut self, trb: [u8; 16]) {
        if self.check_event_ring_full() {
            return;
        }

        assert!(
            self.event_ring.cycle_state <= 1,
            "event ring cycle state should only ever be 0 or 1"
        );
        let mut trb_with_cycle_bit = trb;
        trb_with_cycle_bit[12] = (trb_with_cycle_bit[12] & !0x1) | self.event_ring.cycle_state;

        self.dma_bus
            .write_bulk(self.event_ring.enqueue_pointer, &trb_with_cycle_bit);

        let enqueue_address = self.event_ring.enqueue_pointer;

        self.event_ring.enqueue_pointer += 16;
        self.event_ring.trb_count -= 1;

        debug!(
            "enqueued TRB in first segment of event ring at address {:#x}. Space for {} more TRBs left (TRB: {:?})",
            enqueue_address, self.event_ring.trb_count, trb_with_cycle_bit
        );
    }

    fn check_event_ring_full(&self) -> bool {
        self.event_ring.trb_count == 0
    }
    fn check_if_commands_trb_available(&self) {
        self.check_if_command_trb_available(0);
        self.check_if_command_trb_available(1);
        self.check_if_command_trb_available(2);
        self.check_if_command_trb_available(3);
    }
    fn check_if_command_trb_available(&self, offset: u64) {
        let mut data = [0; 16];
        self.dma_bus
            .read_bulk(self.command_ring_dequeue_pointer + offset * 16, &mut data);
        debug!("{}. command ring TRB: {:?}", offset, data);
    }
    fn doorbell(&mut self) {
        debug!("Ding Dong!");
        self.check_if_commands_trb_available();
        let command_completion_trb =
            Self::create_command_completion_event_trb(self.command_ring_dequeue_pointer);
        self.enqueue_event_trb(command_completion_trb);
        self.interrupt_line.interrupt();
        debug!("wrote command completion event for commandring[0] and signaled interrupt");
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

            offset::PORTSC => {
                debug!("portsc write: {:#x}", value);
                if (value & 0x00020000 != 0) {
                    debug!("found 1-to-clear on bit 17, unsetting bit");
                    self.lock().unwrap().portsc &= !0x00020000;
                }
                if (value & 0x00040000 != 0) {
                    debug!("found 1-to-clear on bit 18, unsetting bit");
                    self.lock().unwrap().portsc &= !0x00040000;
                }
                if (value & 0x00200000 != 0) {
                    debug!("found 1-to-clear on bit 21, unsetting bit");
                    self.lock().unwrap().portsc &= !0x00200000;
                }
            }
            // xHC Runtime Registers
            offset::IMAN => self.lock().unwrap().interrupt_management = value,
            offset::IMOD => self.lock().unwrap().interrupt_moderation_interval = value,
            offset::ERSTSZ => assert!(value <= 1, "only a single segment supported"),
            offset::ERSTBA => self
                .lock()
                .unwrap()
                .configure_event_ring_segment_table(value),
            offset::ERSTBA_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::ERDP => self.lock().unwrap().update_event_ring(value),
            offset::ERDP_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            0x2000 => self.lock().unwrap().doorbell(),
            _ => {
                debug! {"unknown write to {:#x} with value {:#x}", req.addr, value};
                //todo!()
            }
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

            offset::PORTSC => {
                let val = self.lock().unwrap().portsc;
                debug!("read PORTSC detected, supplying {:#x}", val);
                val
                //portsc::DEFAULT
            }
            offset::PORTLI => 0,

            // xHC Runtime Registers
            offset::IMAN => self.lock().unwrap().interrupt_management,
            offset::IMOD => self.lock().unwrap().interrupt_moderation_interval,
            offset::ERSTSZ => 1,
            offset::ERSTBA => self.lock().unwrap().event_ring.base_address,
            offset::ERSTBA_HI => 0,
            offset::ERDP => self.lock().unwrap().event_ring.dequeue_pointer,
            offset::ERDP_HI => 0,
            0x2000 => 0, // kernel reads the doorbell after write

            // Everything else is Reserved Zero
            _ => {
                debug! {"unknown read from {:#x}", req.addr};
                todo!()
            }
        }
    }

    fn bar(&self, bar_no: u8) -> Option<BarInfo> {
        self.lock().unwrap().config_space.bar(bar_no)
    }
}
