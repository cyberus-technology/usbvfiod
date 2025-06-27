//! Emulation of a USB3 Host (XHCI) controller.
//!
//! The specification is available
//! [here](https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf).

use std::sync::{Arc, Mutex};
use tracing::debug;

use crate::device::{
    bus::{BusDeviceRef, Request, SingleThreadedBusDevice},
    interrupt_line::{DummyInterruptLine, InterruptLine},
    pci::{
        config_space::{ConfigSpace, ConfigSpaceBuilder},
        constants::xhci::{capability, offset, runtime, MAX_INTRS, MAX_SLOTS, OP_BASE, RUN_BASE},
        traits::PciDevice,
        trb::{CompletionCode, EventTrb},
    },
};

use super::{
    config_space::BarInfo,
    device_slots::DeviceSlotManager,
    registers::SimpleRegister,
    rings::{CommandRing, EventRing},
    trb::CommandTrb,
};

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

    /// The Command Ring.
    command_ring: CommandRing,

    /// The Event Ring of the single Interrupt Register Set.
    event_ring: EventRing,

    /// Configured device slots.
    slots: Vec<()>,

    /// Device Slot Management
    device_slot_manager: DeviceSlotManager,

    /// Interrupt management register
    interrupt_management: u64,

    /// The minimum interval in 250ns increments between interrupts.
    interrupt_moderation_interval: u64,

    /// The interrupt line triggered to signal device events.
    interrupt_line: Arc<dyn InterruptLine>,

    /// State of the PORTSC register
    portsc: SimpleRegister,
}

impl XhciController {
    /// Create a new XHCI controller with default settings.
    ///
    /// `dma_bus` is the device on which we will perform DMA
    /// operations. This is typically VM guest memory.
    #[must_use]
    pub fn new(dma_bus: BusDeviceRef) -> Self {
        use crate::device::pci::constants::config_space::*;

        let dma_bus_for_command_ring = dma_bus.clone();
        let dma_bus_for_event_ring = dma_bus.clone();
        let dma_bus_for_device_slot_manager = dma_bus.clone();

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
            command_ring: CommandRing::new(dma_bus_for_command_ring),
            event_ring: EventRing::new(dma_bus_for_event_ring),
            slots: vec![],
            device_slot_manager: DeviceSlotManager::new(1, dma_bus_for_device_slot_manager),
            interrupt_management: 0,
            interrupt_moderation_interval: runtime::IMOD_DEFAULT,
            interrupt_line: Arc::new(DummyInterruptLine::default()),
            portsc: SimpleRegister::new(0x00260203, 0x00260000),
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
        !u64::from(self.running) & 0x1u64 | 0x8 | 0x10
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
        self.device_slot_manager
            .set_dcbaap(device_context_base_array_ptr);
    }

    /// Start/Stop controller operation
    ///
    /// This is called for writes of the `USBCMD` register.
    pub fn run(&mut self, usbcmd: u64) {
        self.running = usbcmd & 0x1 == 0x1;
        if self.running {
            debug!("controller started with cmd {usbcmd:#x}");

            // Send a port status change event, which signals the driver to
            // inspect the PORTSC status register.
            let trb = EventTrb::new_port_status_change_event_trb(0);
            self.event_ring.enqueue(&trb);

            // XXX: This is just a test to see if we can generate interrupts.
            // This will be removed once we generate interrupts in the right
            // place, (e.g. generate a Port Connect Status Event) and test it.
            self.interrupt_line.interrupt();
            debug!("signalled a bogus interrupt");
        } else {
            debug!("controller stopped with cmd {usbcmd:#x}");
        }
    }

    fn doorbell_controller(&mut self) {
        debug!("Ding Dong!");
        // check command available
        let next = self.command_ring.next_command_trb();
        if let Some((address, Ok(cmd_trb))) = next {
            self.handle_command(address, cmd_trb);
        } else {
            debug!(
                "Doorbell was rang, but no (valid) command found on the command ring ({:?})",
                next
            );
        }
    }

    fn handle_command(&mut self, address: u64, cmd: CommandTrb) {
        debug!("handling command {:?} at {:#x}", cmd, address);
        match cmd {
            CommandTrb::EnableSlotCommand => {
                debug!("Handling Enable Slot Command");
                // try to reserve a device slot
                let reservation = self.device_slot_manager.reserve_slot();
                let completion_event = if let Some(slot_id) = reservation {
                    debug!("Answering driver to use Slot ID {}", slot_id);
                    EventTrb::new_command_completion_event_trb(
                        address,
                        0,
                        CompletionCode::Success,
                        slot_id as u8,
                    )
                } else {
                    // no free slot
                    debug!("Answering driver that no free slot is available");
                    EventTrb::new_command_completion_event_trb(
                        address,
                        0,
                        CompletionCode::NoSlotsAvailableError,
                        0,
                    )
                };
                self.event_ring.enqueue(&completion_event);
                self.interrupt_line.interrupt();
            }
            CommandTrb::DisableSlotCommand => todo!(),
            CommandTrb::AddressDeviceCommand(data) => {
                debug!("Handling Address Device Command (input context pointer: {:#x}, BSR: {}, slot id: {})", data.input_context_pointer, data.block_set_address_request, data.slot_id);

                let device_context = self.device_slot_manager.get_device_context(data.slot_id);
                device_context.initialize(data.input_context_pointer);

                let completion_event = EventTrb::new_command_completion_event_trb(
                    address,
                    0,
                    CompletionCode::Success,
                    data.slot_id,
                );
                self.event_ring.enqueue(&completion_event);
                self.interrupt_line.interrupt();
            }
            CommandTrb::ConfigureEndpointCommand => todo!(),
            CommandTrb::EvaluateContextCommand => todo!(),
            CommandTrb::ResetEndpointCommand => todo!(),
            CommandTrb::StopEndpointCommand => todo!(),
            CommandTrb::SetTrDequeuePointerCommand => todo!(),
            CommandTrb::ResetDeviceCommand => todo!(),
            CommandTrb::ForceHeaderCommand => todo!(),
            CommandTrb::NoOpCommand => todo!(),
            CommandTrb::Link(_) => unreachable!(),
        }
    }

    fn doorbell_device(&mut self, value: u32) {
        debug!("Ding Dong Device with value {}!", value);
        // TODO inspect value
        // currently we assume it is 1, which indicates a request on the control transfer ring
        assert_eq!(1, value, "currently only implemented doorbell rings that indicate requests on the control transfer ring");

        // check request available
        let transfer_ring = self
            .device_slot_manager
            .get_device_context(1)
            .get_control_transfer_ring();

        let (address, request) = match transfer_ring.next_request() {
            None => panic!(
                "Device doorbell was rang, but there is no request on the control transfer ring"
            ),
            Some(Err(err)) => panic!(
                "Failed to retrieve request from control transfer ring: {:?}",
                err
            ),
            Some(Ok(res)) => res,
        };

        debug!("got request: {:?}", request);
        // TODO forward request to device

        // send transfer event
        let trb =
            EventTrb::new_transfer_event_trb(address, 0, CompletionCode::Success, false, 1, 1);
        self.event_ring.enqueue(&trb);
        self.interrupt_line.interrupt();
        debug!("sent Transfer Event and signaled interrupt");
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
            offset::CRCR => self.lock().unwrap().command_ring.control(value),
            offset::CRCR_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::DCBAAP => self.lock().unwrap().configure_device_contexts(value),
            offset::DCBAAP_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::CONFIG => self.lock().unwrap().enable_slots(value),
            offset::USBSTS => {}
            offset::PORTSC => self.lock().unwrap().portsc.write(value),

            // xHC Runtime Registers
            offset::IMAN => self.lock().unwrap().interrupt_management = value,
            offset::IMOD => self.lock().unwrap().interrupt_moderation_interval = value,
            offset::ERSTSZ => assert_eq!(value, 1, "only a single segment supported"),
            offset::ERSTBA => self.lock().unwrap().event_ring.configure(value),
            offset::ERSTBA_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::ERDP => self
                .lock()
                .unwrap()
                .event_ring
                .update_dequeue_pointer(value),
            offset::ERDP_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::DOORBELL_CONTROLLER => self.lock().unwrap().doorbell_controller(),
            offset::DOORBELL_DEVICE => self.lock().unwrap().doorbell_device(value as u32),
            addr => {
                panic!("unknown write {}", addr);
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
            offset::DBOFF => offset::DOORBELL_CONTROLLER,
            offset::RTSOFF => RUN_BASE,
            offset::HCCPARAMS2 => 0,

            // xHC Extended Capability ("Supported Protocols Capability")
            offset::SUPPORTED_PROTOCOLS => capability::supported_protocols::CAP_INFO,
            offset::SUPPORTED_PROTOCOLS_CONFIG => capability::supported_protocols::CONFIG,

            // xHC Operational Registers
            offset::USBCMD => 0,
            offset::USBSTS => self.lock().unwrap().status(),
            offset::DNCTL => 2,
            offset::CRCR => self.lock().unwrap().command_ring.status(),
            offset::CRCR_HI => 0,
            offset::DCBAAP => self.lock().unwrap().device_slot_manager.get_dcbaap(),
            offset::DCBAAP_HI => 0,
            offset::PAGESIZE => 0x1, /* 4k Pages */
            offset::CONFIG => self.lock().unwrap().config(),

            offset::PORTSC => self.lock().unwrap().portsc.read(),
            offset::PORTLI => 0,

            // xHC Runtime Registers
            offset::IMAN => self.lock().unwrap().interrupt_management,
            offset::IMOD => self.lock().unwrap().interrupt_moderation_interval,
            offset::ERSTSZ => 1,
            offset::ERSTBA => self.lock().unwrap().event_ring.read_base_address(),
            offset::ERSTBA_HI => 0,
            offset::ERDP => self.lock().unwrap().event_ring.read_dequeue_pointer(),
            offset::ERDP_HI => 0,
            offset::DOORBELL_CONTROLLER => 0, // kernel reads the doorbell after write
            offset::DOORBELL_DEVICE => 0,

            // Everything else is Reserved Zero
            addr => {
                panic!("unknown read {}", addr);
            }
        }
    }

    fn bar(&self, bar_no: u8) -> Option<BarInfo> {
        self.lock().unwrap().config_space.bar(bar_no)
    }
}
