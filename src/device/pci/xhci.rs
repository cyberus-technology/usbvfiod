//! Emulation of a USB3 Host (XHCI) controller.
//!
//! The specification is available
//! [here](https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf).

use std::sync::{
    atomic::{fence, Ordering},
    Arc, Mutex,
};
use tracing::{debug, info, trace, warn};

use crate::device::{
    bus::{BusDeviceRef, Request, SingleThreadedBusDevice},
    interrupt_line::{DummyInterruptLine, InterruptLine},
    pci::{
        config_space::{ConfigSpace, ConfigSpaceBuilder},
        constants::xhci::{
            capability, offset, operational::portsc, runtime, MAX_INTRS, MAX_SLOTS, NUM_USB2_PORTS,
            NUM_USB3_PORTS, OP_BASE, RUN_BASE,
        },
        traits::PciDevice,
        trb::{CommandTrbVariant, CompletionCode, EventTrb},
    },
};

use super::{
    config_space::BarInfo,
    constants::xhci::{device_slots::endpoint_state, operational::usbsts, MAX_PORTS},
    device_slots::DeviceSlotManager,
    realdevice::{EndpointWorkerInfo, RealDevice},
    registers::PortscRegister,
    rings::{CommandRing, EventRing},
    trb::{
        AddressDeviceCommandTrbData, CommandTrb, ConfigureEndpointCommandTrbData,
        StopEndpointCommandTrbData,
    },
};

/// The emulation of a XHCI controller.
#[derive(Debug)]
pub struct XhciController {
    /// real USB devices
    device_slots: [Option<Box<dyn RealDevice>>; MAX_PORTS as usize],

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
    event_ring: Arc<Mutex<EventRing>>,

    /// Device Slot Management
    device_slot_manager: DeviceSlotManager,

    /// Interrupt management register
    interrupt_management: u64,

    /// The minimum interval in 250ns increments between interrupts.
    interrupt_moderation_interval: u64,

    /// The interrupt line triggered to signal device events.
    interrupt_line: Arc<dyn InterruptLine>,

    /// USB3 PORTSC registers array
    portsc_usb3: Vec<PortscRegister>,

    /// USB2 PORTSC registers array
    portsc_usb2: Vec<PortscRegister>,
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
            device_slots: [const { None }; MAX_PORTS as usize],
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
            event_ring: Arc::new(Mutex::new(EventRing::new(dma_bus_for_event_ring))),
            device_slot_manager: DeviceSlotManager::new(MAX_SLOTS, dma_bus_for_device_slot_manager),
            interrupt_management: 0,
            interrupt_moderation_interval: runtime::IMOD_DEFAULT,
            interrupt_line: Arc::new(DummyInterruptLine::default()),
            portsc_usb3: vec![PortscRegister::new(portsc::PP); NUM_USB3_PORTS as usize],
            portsc_usb2: vec![PortscRegister::new(portsc::PP); NUM_USB2_PORTS as usize],
        }
    }

    /// Attach a real USB device to the controller.
    ///
    /// The device is connected to the first available USB port and becomes available
    /// for the guest driver to interact with. The port's status is updated to reflect
    /// the device's connection and speed.
    ///
    /// # Parameters
    ///
    /// * `device` - The real USB device to attach
    ///
    /// # Panics
    ///
    /// Currently panics if no USB port is available for the device.
    // TODO: Replace the panic (expect) with logic that does nothing if there is no space
    // and indicates with the return value that the attachment failed. There is no good reason
    // for us to crash here, we can continue running as before, it is up to the caller to
    // decide how to handle the failed attachment attempt.
    pub fn set_device(&mut self, device: Box<dyn RealDevice>) {
        if let Some(speed) = device.speed() {
            let slot_index = self
                .device_slots
                .iter()
                .position(|slot| slot.is_none())
                .expect("No available device slots - all slots are occupied");

            self.device_slots[slot_index] = Some(device);

            let portsc = PortscRegister::new(
                portsc::CCS
                    | portsc::PED
                    | portsc::PP
                    | portsc::CSC
                    | portsc::PEC
                    | portsc::PRC
                    | (speed as u64) << 10,
            );
            if speed.is_usb2_speed() {
                // Find first available USB2 port
                let port_idx = self
                    .find_available_usb2_port()
                    .expect("No available USB2 ports - all ports are occupied");

                self.portsc_usb2[port_idx] = portsc;
            } else {
                // Find first available USB3 port
                let port_idx = self
                    .find_available_usb3_port()
                    .expect("No available USB3 ports - all ports are occupied");

                self.portsc_usb3[port_idx] = portsc;
            }

            info!("Attached {} device to slot {}", speed, slot_index + 1);
        } else {
            warn!("Failed to attach device: Unable to determine speed");
        }
    }

    // Helper function to find available port in any port array
    // TODO: This portsc-lookup-approach is the only one viable right now, but as soon as we have
    // multiple devices, we need to track the "port<-->real device" mapping and looking up the
    // empty ports there will be a nicer implementation (portsc lookup is more indirect)
    fn find_available_port_in_array(ports: &[PortscRegister]) -> Option<usize> {
        for (idx, port) in ports.iter().enumerate() {
            // Port is available if it doesn't have CCS (Current Connect Status) bit set
            if port.read() & portsc::CCS == 0 {
                return Some(idx);
            }
        }
        None
    }

    // Find the first available USB3 port (not currently connected)
    fn find_available_usb3_port(&self) -> Option<usize> {
        Self::find_available_port_in_array(&self.portsc_usb3)
    }

    // Find the first available USB2 port (not currently connected)
    fn find_available_usb2_port(&self) -> Option<usize> {
        Self::find_available_port_in_array(&self.portsc_usb2)
    }

    // Helper function to get port index from MMIO address
    const fn get_port_index_from_addr(
        addr: u64,
        base_addr: u64,
        port_count: u64,
        register_offset: u64,
    ) -> Option<usize> {
        if addr >= base_addr && addr < base_addr + (port_count * offset::PORT_STRIDE) {
            // Check if this is the correct register within the port's PORT_STRIDE byte range
            if (addr - base_addr) % offset::PORT_STRIDE == register_offset {
                Some(((addr - base_addr) / offset::PORT_STRIDE) as usize)
            } else {
                None
            }
        } else {
            None
        }
    }

    // Get USB3 port index from MMIO offset, returns None for non-USB3 ports
    const fn get_usb3_portsc_index(&self, addr: u64) -> Option<usize> {
        Self::get_port_index_from_addr(addr, offset::PORTSC_USB3, NUM_USB3_PORTS, 0)
    }

    // Get USB3 PORTLI port index from MMIO offset, returns None for non-PORTLI registers
    const fn get_usb3_portli_index(&self, addr: u64) -> Option<usize> {
        Self::get_port_index_from_addr(addr, offset::PORTSC_USB3, NUM_USB3_PORTS, 0x8)
    }

    // Get USB2 port index from MMIO offset, returns None for non-USB2 ports
    const fn get_usb2_portsc_index(&self, addr: u64) -> Option<usize> {
        Self::get_port_index_from_addr(addr, offset::PORTSC_USB2, NUM_USB2_PORTS, 0)
    }

    // Get USB2 PORTLI port index from MMIO offset, returns None for non-PORTLI registers
    const fn get_usb2_portli_index(&self, addr: u64) -> Option<usize> {
        Self::get_port_index_from_addr(addr, offset::PORTSC_USB2, NUM_USB2_PORTS, 0x8)
    }

    fn write_usb3_portsc(&mut self, port_idx: usize, value: u64) {
        self.portsc_usb3[port_idx].write(value);
        let status = Self::describe_portsc_status(value);
        trace!("USB3 Port idx {} status: {}", port_idx, status);
    }

    fn write_usb2_portsc(&mut self, port_idx: usize, value: u64) {
        self.portsc_usb2[port_idx].write(value);
        let status = Self::describe_portsc_status(value);
        trace!("USB2 Port idx {} status: {}", port_idx, status);
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
        !u64::from(self.running) & usbsts::HCH | usbsts::EINT | usbsts::PCD
    }

    /// Obtain the current host controller configuration as defined for the `CONFIG` register.
    #[must_use]
    pub const fn config(&self) -> u64 {
        self.device_slot_manager.num_slots & 0x8u64
    }

    /// Enable device slots.
    pub fn enable_slots(&self, count: u64) {
        assert!(
            count == self.device_slot_manager.num_slots,
            "we expect the driver to enable all slots that we report"
        );

        debug!("enabled {} device slots", count);
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
            self.event_ring.lock().unwrap().enqueue(&trb);

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
        while let Some(cmd) = self.command_ring.next_command_trb() {
            self.handle_command(cmd);
        }
    }

    const fn describe_portsc_status(value: u64) -> &'static str {
        if value & portsc::CCS != 0 {
            "device connected"
        } else if value & portsc::PP != 0 {
            "empty port"
        } else {
            "port powered off"
        }
    }

    fn handle_command(&mut self, cmd: CommandTrb) {
        debug!("handling command {:?} at {:#x}", cmd, cmd.address);
        let completion_event = match cmd.variant {
            CommandTrbVariant::EnableSlot => {
                let (completion_code, slot_id) = self.handle_enable_slot();
                EventTrb::new_command_completion_event_trb(cmd.address, 0, completion_code, slot_id)
            }
            CommandTrbVariant::DisableSlot => {
                // TODO this command probably requires more handling.
                // Currently, we just acknowledge to not crash usbvfiod in the
                // integration test.
                EventTrb::new_command_completion_event_trb(
                    cmd.address,
                    0,
                    CompletionCode::Success,
                    1,
                )
            }
            CommandTrbVariant::AddressDevice(data) => {
                self.handle_address_device(&data);
                EventTrb::new_command_completion_event_trb(
                    cmd.address,
                    0,
                    CompletionCode::Success,
                    data.slot_id,
                )
            }
            CommandTrbVariant::ConfigureEndpoint(data) => {
                self.handle_configure_endpoint(&data);
                EventTrb::new_command_completion_event_trb(
                    cmd.address,
                    0,
                    CompletionCode::Success,
                    data.slot_id,
                )
            }
            CommandTrbVariant::EvaluateContext => todo!(),
            CommandTrbVariant::ResetEndpoint => todo!(),
            CommandTrbVariant::StopEndpoint(data) => {
                self.handle_stop_endpoint(&data);
                EventTrb::new_command_completion_event_trb(
                    cmd.address,
                    0,
                    CompletionCode::Success,
                    data.slot_id,
                )
            }
            CommandTrbVariant::SetTrDequeuePointer => todo!(),
            CommandTrbVariant::ResetDevice(data) => {
                // TODO this command probably requires more handling. The guest
                // driver will attempt resets when descriptors do not match what
                // the virtual port announces.
                // Currently, we just acknowledge to not crash usbvfiod when
                // testing with unsupported devices.
                warn!("device reset! the driver probably didn't like it.");
                EventTrb::new_command_completion_event_trb(
                    cmd.address,
                    0,
                    CompletionCode::Success,
                    data.slot_id,
                )
            }
            CommandTrbVariant::ForceHeader => todo!(),
            CommandTrbVariant::NoOp => todo!(),
            CommandTrbVariant::Link(_) => unreachable!(),
            CommandTrbVariant::Unrecognized(trb_buffer, error) => todo!(
                "encountered unrecognized command (error: {}, trb: {:?})",
                error,
                trb_buffer
            ),
        };
        // Command handlers might have performed stores to guest memory.
        // The stores have to be finished before the command completion
        // event is written (essentially releasing the data to the driver).
        //
        // Not all commands write to guest memory, so this fence is sometimes
        // not necessary. However, because it declutters the code and avoids
        // missing a fence where it is needed, we choose to place a release
        // barrier before every event enqueue.
        fence(Ordering::Release);
        self.event_ring.lock().unwrap().enqueue(&completion_event);
        self.interrupt_line.interrupt();
    }

    fn handle_enable_slot(&mut self) -> (CompletionCode, u8) {
        // try to reserve a device slot
        let reservation = self.device_slot_manager.reserve_slot();
        reservation.map_or_else(
            || {
                debug!("Answering driver that no free slot is available");
                (CompletionCode::NoSlotsAvailableError, 0)
            },
            |slot_id| {
                debug!("Answering driver to use Slot ID {}", slot_id);
                (CompletionCode::Success, slot_id as u8)
            },
        )
    }

    fn handle_address_device(&self, data: &AddressDeviceCommandTrbData) {
        let device_context = self.device_slot_manager.get_device_context(data.slot_id);
        device_context.initialize(data.input_context_pointer);
    }

    fn handle_configure_endpoint(&mut self, data: &ConfigureEndpointCommandTrbData) {
        if data.deconfigure {
            todo!("encountered Configure Endpoint Command with deconfigure set");
        }
        let device_context = self.device_slot_manager.get_device_context(data.slot_id);
        let enabled_endpoints = device_context.configure_endpoints(data.input_context_pointer);
        // Program requires real USB device for all XHCI operations (pattern used throughout file)
        let device_index = data.slot_id as usize - 1;
        let device = self.device_slots[device_index]
            .as_mut()
            .unwrap_or_else(|| panic!("No device in slot {} (index {}) - cannot configure endpoints without a real device", data.slot_id, device_index));

        for (i, ep_type) in enabled_endpoints {
            let worker_info = EndpointWorkerInfo {
                slot_id: data.slot_id,
                endpoint_id: i,
                transfer_ring: device_context.get_transfer_ring(i as u64),
                dma_bus: self.dma_bus.clone(),
                event_ring: self.event_ring.clone(),
                interrupt_line: self.interrupt_line.clone(),
            };
            device.enable_endpoint(worker_info, ep_type);
        }
    }

    fn handle_stop_endpoint(&self, data: &StopEndpointCommandTrbData) {
        let device_context = self.device_slot_manager.get_device_context(data.slot_id);
        device_context.set_endpoint_state(data.endpoint_id, endpoint_state::STOPPED);
    }

    fn doorbell_device(&mut self, slot_id: u8, value: u32) {
        debug!("Ding Dong Device Slot {} with value {}!", slot_id, value);

        match value {
            ep if ep == 0 || ep > 31 => panic!("invalid value {} on doorbell write", ep),
            1 => self.check_control_endpoint(slot_id),
            ep => {
                // When the driver rings the doorbell with a non-control
                // endpoint id, a lot must have happened before (e.g., descriptor
                // reads on the control endpoint), so we never reach this point
                // when no device is available (except for an invalid doorbell
                // write, in which case panicking is the right thing to do.
                assert!(
                    u64::from(slot_id) <= MAX_SLOTS,
                    "invalid slot_id {} in doorbell",
                    slot_id
                );
                let device_index = slot_id as usize - 1;
                let device = self.device_slots[device_index]
                    .as_mut()
                    .unwrap_or_else(|| panic!("No device in slot {} (index {}) - this should not happen for valid doorbell operations", slot_id, device_index));
                device.transfer(ep as u8);
            }
        };
    }

    fn check_control_endpoint(&self, slot: u8) {
        // check request available
        let transfer_ring = self
            .device_slot_manager
            .get_device_context(slot)
            .get_control_transfer_ring();

        let request = match transfer_ring.next_request() {
            None => {
                // XXX currently, we expect that a doorbell ring always
                // notifies us about a new control request. We want to
                // clearly see when another case occurs, so we want to panic
                // here.
                // Once we know all behaviors, the panic can probably be
                // removed.
                panic!(
                "Device doorbell was rang, but there is no request on the control transfer ring"
            );
            }
            Some(Err(err)) => panic!(
                "Failed to retrieve request from control transfer ring: {:?}",
                err
            ),
            Some(Ok(res)) => res,
        };

        debug!(
            "got request with: request_type={}, request={}, value={}, index={}, length={}, data={:?}",
            request.request_type,
            request.request,
            request.value,
            request.index,
            request.length,
            request.data
        );
        // forward request to device
        // Port status change events are suggestions for the driver to check portsc registers.
        // If no device is found, the driver won't start device initialization. Therefore,
        // when we reach this control transfer path, we should assume a device is present.
        let device_index = slot as usize - 1;
        let device = self.device_slots[device_index]
            .as_ref()
            .unwrap_or_else(|| panic!("No device in slot {} (index {}) - this should not happen for valid control transfers", slot, device_index));
        device.control_transfer(&request, &self.dma_bus);

        // send transfer event
        let trb = EventTrb::new_transfer_event_trb(
            request.address,
            0,
            CompletionCode::Success,
            false,
            1,
            slot,
        );
        self.event_ring.lock().unwrap().enqueue(&trb);
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

    #[allow(clippy::cognitive_complexity)]
    fn write_io(&self, region: u32, req: Request, value: u64) {
        // The XHCI Controller has a single MMIO BAR.
        assert_eq!(region, 0);

        let mut guard = self.lock().unwrap();
        match req.addr {
            // xHC Operational Registers
            offset::USBCMD => guard.run(value),
            offset::DNCTL => assert_eq!(value, 2, "debug notifications not supported"),
            offset::CRCR => guard.command_ring.control(value),
            offset::CRCR_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::DCBAAP => guard.configure_device_contexts(value),
            offset::DCBAAP_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::CONFIG => guard.enable_slots(value),
            // USBSTS writes occur but we can ignore them (to get a device enumerated)
            offset::USBSTS => {}
            // xHC Runtime Registers (moved up for performance)
            offset::IMAN => guard.interrupt_management = value,
            offset::IMOD => guard.interrupt_moderation_interval = value,
            offset::ERSTSZ => {
                let sz = (value as u32) & 0xFFFF;
                guard.event_ring.lock().unwrap().set_erst_size(sz);
            }
            offset::ERSTBA => guard.event_ring.lock().unwrap().configure(value),
            offset::ERSTBA_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::ERDP => guard
                .event_ring
                .lock()
                .unwrap()
                .update_dequeue_pointer(value),
            offset::ERDP_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::DOORBELL_CONTROLLER => guard.doorbell_controller(),
            // Device Doorbell Registers (DOORBELL_DEVICE)
            offset::DOORBELL_DEVICE..offset::DOORBELL_DEVICE_END => {
                let slot_id = ((req.addr - offset::DOORBELL_CONTROLLER) / 4) as u8;
                guard.doorbell_device(slot_id, value as u32);
            }

            // USB 3.0 Port Status and Control Register (PORTSC_USB3)
            addr if guard.get_usb3_portsc_index(addr).is_some() => {
                // SAFETY: unwrap() is safe because we already checked is_some() in the match guard above
                let port_idx = guard.get_usb3_portsc_index(addr).unwrap();
                guard.write_usb3_portsc(port_idx, value);
            }
            // USB 2.0 Port Status and Control Register (PORTSC_USB2)
            addr if guard.get_usb2_portsc_index(addr).is_some() => {
                // SAFETY: unwrap() is safe because we already checked is_some() in the match guard above
                let port_idx = guard.get_usb2_portsc_index(addr).unwrap();
                guard.write_usb2_portsc(port_idx, value);
            }
            addr => {
                todo!("unknown write {}", addr);
            }
        }
        // Drop the guard early to reduce resource contention as suggested by clippy
        drop(guard);
    }

    fn read_io(&self, region: u32, req: Request) -> u64 {
        // The XHCI Controller has a single MMIO BAR.
        assert_eq!(region, 0);

        let guard = self.lock().unwrap();
        match req.addr {
            // xHC Capability Registers
            offset::CAPLENGTH => OP_BASE,
            offset::HCIVERSION => capability::HCIVERSION,
            offset::HCSPARAMS1 => capability::HCSPARAMS1,
            offset::HCSPARAMS2 => capability::HCSPARAMS2,
            offset::HCSPARAMS3 => 0,
            offset::HCCPARAMS1 => capability::HCCPARAMS1,
            offset::DBOFF => offset::DOORBELL_CONTROLLER,
            offset::RTSOFF => RUN_BASE,
            offset::HCCPARAMS2 => 0,

            // xHC Extended Capability ("Supported Protocols Capability")
            offset::SUPPORTED_PROTOCOLS => capability::supported_protocols::CAP_INFO,
            offset::SUPPORTED_PROTOCOLS_CONFIG => capability::supported_protocols::CONFIG,
            offset::SUPPORTED_PROTOCOLS_USB2 => capability::supported_protocols_usb2::CAP_INFO,
            offset::SUPPORTED_PROTOCOLS_USB2_CONFIG => capability::supported_protocols_usb2::CONFIG,

            // xHC Operational Registers
            offset::USBCMD => 0,
            offset::USBSTS => guard.status(),
            offset::DNCTL => 2,
            offset::CRCR => guard.command_ring.status(),
            offset::CRCR_HI => 0,
            offset::DCBAAP => guard.device_slot_manager.get_dcbaap(),
            offset::DCBAAP_HI => 0,
            offset::PAGESIZE => 0x1, /* 4k Pages */
            offset::CONFIG => guard.config(),

            // xHC Runtime Registers (moved up for performance)
            offset::IMAN => guard.interrupt_management,
            offset::IMOD => guard.interrupt_moderation_interval,
            offset::ERSTSZ => guard.event_ring.lock().unwrap().read_erst_size(),
            offset::ERSTBA => guard.event_ring.lock().unwrap().read_base_address(),
            offset::ERSTBA_HI => 0,
            offset::ERDP => guard.event_ring.lock().unwrap().read_dequeue_pointer(),
            offset::ERDP_HI => 0,
            offset::DOORBELL_CONTROLLER => 0, // kernel reads the doorbell after write
            // Device Doorbell Registers (DOORBELL_DEVICE)
            offset::DOORBELL_DEVICE..offset::DOORBELL_DEVICE_END => 0,

            // USB 3.0 Port Status and Control Register (PORTSC_USB3)
            addr if guard.get_usb3_portsc_index(addr).is_some() => {
                // SAFETY: unwrap() is safe because we already checked is_some() in the match guard above
                let port_idx = guard.get_usb3_portsc_index(addr).unwrap();
                guard.portsc_usb3[port_idx].read()
            }
            // USB 3.0 Port Link Info Register (PORTLI_USB3)
            addr if guard.get_usb3_portli_index(addr).is_some() => 0,
            // USB 2.0 Port Status and Control Register (PORTSC_USB2)
            addr if guard.get_usb2_portsc_index(addr).is_some() => {
                // SAFETY: unwrap() is safe because we already checked is_some() in the match guard above
                let port_idx = guard.get_usb2_portsc_index(addr).unwrap();
                guard.portsc_usb2[port_idx].read()
            }
            // USB 2.0 Port Link Info Register (PORTLI_USB2)
            addr if guard.get_usb2_portli_index(addr).is_some() => 0,

            // Everything else is Reserved Zero
            addr => {
                todo!("unknown read {}", addr);
            }
        }
    }

    fn bar(&self, bar_no: u8) -> Option<BarInfo> {
        self.lock().unwrap().config_space.bar(bar_no)
    }
}
