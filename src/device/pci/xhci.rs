//! Emulation of a USB3 Host (XHCI) controller.
//!
//! See XHCI specification Section 3 for an overview about the scope of the xHC.

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
            capability, offset, operational::portsc, runtime, MAX_INTRS, MAX_SLOTS, NUM_USB3_PORTS,
            OP_BASE, RUN_BASE,
        },
        realdevice::EndpointType,
        traits::PciDevice,
        trb::{CommandTrbVariant, CompletionCode, DisableSlotCommandTrbData, EventTrb},
    },
};
use usbvfiod::hotplug_protocol::response::Response;

use super::{
    config_space::BarInfo,
    constants::xhci::{device_slots::endpoint_state, operational::usbsts, MAX_PORTS},
    device_slots::DeviceSlotManager,
    realdevice::{EndpointWorkerInfo, IdentifiableRealDevice, RealDevice, Speed},
    registers::PortscRegister,
    rings::{CommandRing, EventRing},
    trb::{
        AddressDeviceCommandTrbData, CommandTrb, ConfigureEndpointCommandTrbData,
        StopEndpointCommandTrbData,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsbVersion {
    USB2,
    USB3,
}

impl UsbVersion {
    const fn from_speed(speed: Speed) -> Self {
        if speed.is_usb2_speed() {
            Self::USB2
        } else {
            Self::USB3
        }
    }
}

/// Make a fixed size array index start at one (instead of zero).
/// This is a common pattern within XHCI for port and slot IDs, and
/// manually handling the difference is error-prone.
#[derive(Debug)]
struct OneIndexed<T, const S: usize> {
    array: [T; S],
}

impl<T, const S: usize> OneIndexed<T, S> {
    fn iter(&self) -> impl Iterator<Item = &T> {
        self.array.iter()
    }
    #[allow(unused)]
    fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.array.iter_mut()
    }
    fn get(&self, index: usize) -> Option<&T> {
        self.array.get(index.wrapping_sub(1))
    }
    /// Enumerating elements with correct index.
    ///
    /// Using `some_one_indexed.iter().enumerate()` generates an iterator like
    /// `(0, some_one_indexed[1]), (1, some_one_indexed[2]), ...`
    ///
    /// Using `some_one_indexed.enumerate()` instead generates an iterator like
    /// `(1, some_one_indexed[1]), (2, some_one_indexed[2]), ...`
    ///
    /// This method is useful for avoiding manual "one-shifting" when trying to
    /// filter for the indices of items with specific properties.
    fn enumerate(&self) -> impl Iterator<Item = (usize, &T)> {
        self.array.iter().enumerate().map(|(i, e)| (i + 1, e))
    }
}
impl<T, const S: usize> std::convert::From<[T; S]> for OneIndexed<T, S> {
    fn from(val: [T; S]) -> Self {
        Self { array: val }
    }
}
impl<T, const S: usize> std::ops::Index<usize> for OneIndexed<T, S> {
    type Output = T;
    fn index(&self, index: usize) -> &T {
        &self.array[index.wrapping_sub(1)]
    }
}
impl<T, const S: usize> std::ops::IndexMut<usize> for OneIndexed<T, S> {
    fn index_mut(&mut self, index: usize) -> &mut T {
        &mut self.array[index.wrapping_sub(1)]
    }
}

/// The emulation of a XHCI controller.
#[derive(Debug)]
pub struct XhciController {
    /// real USB devices
    devices: OneIndexed<Option<IdentifiableRealDevice>, { MAX_PORTS as usize }>,

    /// Slot-to-port mapping.
    slot_to_port: OneIndexed<Option<usize>, { MAX_SLOTS as usize }>,

    /// A reference to the VM memory to perform DMA on.
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

    /// PORTSC registers array
    portsc: OneIndexed<PortscRegister, { MAX_PORTS as usize }>,
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
            devices: [const { None }; MAX_PORTS as usize].into(),
            slot_to_port: [None; MAX_SLOTS as usize].into(),
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
            portsc: [PortscRegister::new(portsc::PP); MAX_PORTS as usize].into(),
        }
    }

    fn device_by_slot_mut<'a>(
        slot_to_port: &OneIndexed<Option<usize>, { MAX_SLOTS as usize }>,
        devices: &'a mut OneIndexed<Option<IdentifiableRealDevice>, { MAX_PORTS as usize }>,
        slot_id: u8,
    ) -> Option<&'a mut Box<dyn RealDevice>> {
        let device = slot_to_port
            .get(slot_id as usize)
            .and_then(|port_id| *port_id)
            .and_then(|port_id| devices[port_id].as_mut().map(|dev| &mut dev.real_device));
        if device.is_none() {
            info!("attempted to access non available device with slot_id {slot_id}");
        }
        device
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
    pub fn attach_device(&mut self, device: IdentifiableRealDevice) -> Result<Response, Response> {
        if self
            .attached_devices()
            .contains(&(device.bus_number, device.device_number))
        {
            return Err(Response::AlreadyAttached);
        }
        if let Some(speed) = device.real_device.speed() {
            let version = UsbVersion::from_speed(speed);
            let available_port_id = match (1..=MAX_PORTS as usize)
                .find(|&i| {
                    self.devices[i].is_none()
                        && matches!(Self::version_relative_id(i), Some((v, _)) if v == version)
                }) // filter USB2/3
                {
                    Some(port) => port,
                    None => return Err(Response::NoFreePort),
                };

            self.devices[available_port_id] = Some(device);
            self.portsc[available_port_id] = PortscRegister::new(
                portsc::CCS
                    | portsc::PED
                    | portsc::PP
                    | portsc::CSC
                    | portsc::PEC
                    | portsc::PRC
                    | (speed as u64) << 10,
            );

            // Safety: the call for the same id succeeded before in the filter.
            let port_id = Self::version_relative_id(available_port_id).unwrap().1;
            info!(
                "Attached {} device to {:?} port {}",
                speed, version, port_id
            );

            self.send_port_status_change_event(available_port_id as u8);
            Ok(Response::SuccessfulOperation)
        } else {
            warn!("Failed to attach device: Unable to determine speed");
            Err(Response::CouldNotDetermineSpeed)
        }
    }

    pub fn attached_devices(&self) -> Vec<(u8, u8)> {
        self.devices
            .iter()
            .filter_map(|dev| dev.as_ref())
            .map(|dev| (dev.bus_number, dev.device_number))
            .collect()
    }

    fn send_port_status_change_event(&self, port: u8) {
        if self.running {
            let trb = EventTrb::new_port_status_change_event_trb(port);
            self.event_ring.lock().unwrap().enqueue(&trb);

            self.interrupt_line.interrupt();
            debug!("informed the driver about the port change");
        } else {
            debug!("controller is not running, not notifying about the port status change");
        }
    }

    /// Detach a real USB device from the controller.
    pub fn detach_device(
        &mut self,
        bus_number: u8,
        device_number: u8,
    ) -> Result<Response, Response> {
        // find out on which port the device is connected
        let port_id = match self
            .devices
            .enumerate()
            .filter_map(|(i, dev)| dev.as_ref().map(|d| (i, d)))
            .filter(|(_, dev)| dev.bus_number == bus_number && dev.device_number == device_number)
            .map(|(i, _)| i)
            .next()
        {
            Some(i) => i,
            None => return Err(Response::NoSuchDevice),
        };

        // update portsc register
        self.portsc[port_id] = PortscRegister::new(portsc::PP | portsc::CSC);
        self.send_port_status_change_event(port_id as u8);

        // remove
        self.devices[port_id] = None;

        Ok(Response::SuccessfulOperation)
    }

    /// Get USB version and version-relative id for a port.
    ///
    /// Example: We have 3 USB3 ports and 3 USB2 ports.
    ///
    /// port id    version  relative id
    /// 1          USB 3    1
    /// 2          USB 3    2
    /// 3          USB 3    3
    /// 4          USB 2    1
    /// 5          USB 2    2
    /// 6          USB 2    3
    const fn version_relative_id(port_id: usize) -> Option<(UsbVersion, usize)> {
        let first_usb2_id = NUM_USB3_PORTS + 1;
        match port_id as u64 {
            1..=NUM_USB3_PORTS => Some((UsbVersion::USB3, port_id)),
            port_id if first_usb2_id <= port_id && port_id <= MAX_PORTS => {
                Some((UsbVersion::USB2, (port_id - NUM_USB3_PORTS) as usize))
            }
            _ => None,
        }
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

    const fn get_portsc_index(&self, addr: u64) -> Option<usize> {
        Self::get_port_index_from_addr(addr, offset::PORTSC, MAX_PORTS, 0)
    }

    const fn get_portli_index(&self, addr: u64) -> Option<usize> {
        Self::get_port_index_from_addr(addr, offset::PORTSC, MAX_PORTS, 0x8)
    }

    fn write_portsc(&mut self, port_id: usize, value: u64) {
        self.portsc[port_id].write(value);
        let status = Self::describe_portsc_status(value);
        let (version, id) = Self::version_relative_id(port_id).unwrap();
        trace!("{:?} port {} status: {}", version, id, status);
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

            // Send a port status change event for every attached device,
            // signaling the driver to inspect the PORTSC status registers.
            let ports_with_device = self
                .devices
                .iter()
                .enumerate()
                .filter(|(_, dev)| dev.is_some())
                .map(|(index, _)| index as u8 + 1)
                .collect::<Vec<_>>();
            let num_devices = ports_with_device.len();

            for port in ports_with_device {
                let trb = EventTrb::new_port_status_change_event_trb(port);
                self.event_ring.lock().unwrap().enqueue(&trb);
            }

            // if we enqueued an event, we inform the driver with an interrupt.
            if num_devices > 0 {
                self.interrupt_line.interrupt();
                debug!("Enqueue events and signaled interrupt to notify driver of {} attached devices.", num_devices);
            }
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
            CommandTrbVariant::DisableSlot(ref data) => EventTrb::new_command_completion_event_trb(
                cmd.address,
                0,
                self.handle_disable_slot(data),
                data.slot_id,
            ),
            CommandTrbVariant::AddressDevice(ref data) => {
                EventTrb::new_command_completion_event_trb(
                    cmd.address,
                    0,
                    self.handle_address_device(data),
                    data.slot_id,
                )
            }
            CommandTrbVariant::ConfigureEndpoint(ref data) => {
                if self
                    .slot_to_port
                    .get(data.slot_id as usize)
                    .is_some_and(|mapping| mapping.is_some())
                {
                    EventTrb::new_command_completion_event_trb(
                        cmd.address,
                        0,
                        self.handle_configure_endpoint(data),
                        data.slot_id,
                    )
                } else {
                    EventTrb::new_command_completion_event_trb(
                        cmd.address,
                        0,
                        CompletionCode::IncompatibleDeviceError,
                        data.slot_id,
                    )
                }
            }
            CommandTrbVariant::EvaluateContext(data) => {
                // TODO this command probably requires more handling.
                // Currently, we only do a minimal check against a hot-detach race condition
                // and acknowledge to not crash usbvfiod when using USB 1.1.
                warn!("received CommandTrbVariant::EvaluateContext: returning success without taking action");
                EventTrb::new_command_completion_event_trb(
                    cmd.address,
                    0,
                    self.handle_noop(data.slot_id),
                    data.slot_id,
                )
            }
            CommandTrbVariant::ResetEndpoint => todo!(),
            CommandTrbVariant::StopEndpoint(data) => EventTrb::new_command_completion_event_trb(
                cmd.address,
                0,
                self.handle_stop_endpoint(&data),
                data.slot_id,
            ),

            CommandTrbVariant::SetTrDequeuePointer(data) => {
                // TODO this command requires more handling.
                // With the detach functionality this CommandTrb appears after the
                // StopEndpoint and will return an USB error in the attach-detach integration test.
                warn!("received CommandTrbVariant::SetTrDequeuePointer: returning success without taking action");
                EventTrb::new_command_completion_event_trb(
                    cmd.address,
                    0,
                    self.handle_noop(data.slot_id),
                    data.slot_id,
                )
            }
            CommandTrbVariant::ResetDevice(data) => {
                // TODO this command requires more handling. The guest
                // driver will attempt resets when descriptors do not match what
                // the virtual port announces.
                // Currently, we just acknowledge to not crash usbvfiod when
                // testing with unsupported devices.
                // A known exception is the USB 2.0 protocol with one early
                // reset being intended behaviour.
                warn!(
                    "device reset on slot {}! not fully implemented.",
                    data.slot_id
                );
                EventTrb::new_command_completion_event_trb(
                    cmd.address,
                    0,
                    self.handle_noop(data.slot_id),
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

    // This is a general handler to check for device existence and return an error in case
    // of a missing device.
    fn handle_noop(&mut self, slot_id: u8) -> CompletionCode {
        if Self::device_by_slot_mut(&self.slot_to_port, &mut self.devices, slot_id).is_none() {
            return CompletionCode::UsbTransactionError;
        }
        CompletionCode::Success
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

    // A Slot is enabled if it has been reserved in the DeviceSlotManager, but we also
    // have to remove our out-of-spec slot_to_port mapping to properly clean up.
    fn handle_disable_slot(&mut self, data: &DisableSlotCommandTrbData) -> CompletionCode {
        if !self.device_slot_manager.is_reserved(data.slot_id as u64) {
            return CompletionCode::SlotNotEnabledError;
        }

        self.slot_to_port[data.slot_id as usize] = None;
        self.device_slot_manager.free_slot(data.slot_id as u64);
        CompletionCode::Success
    }

    fn handle_address_device(&mut self, data: &AddressDeviceCommandTrbData) -> CompletionCode {
        let Some(device_context) = self.device_slot_manager.get_device_context(data.slot_id) else {
            return CompletionCode::UsbTransactionError;
        };
        let root_hub_port_number = device_context.initialize(data.input_context_pointer);
        if root_hub_port_number < 1 || root_hub_port_number as u64 > MAX_PORTS {
            panic!("address device reported invalid root hub port number: {root_hub_port_number}");
        }
        let port_id = root_hub_port_number as usize;
        self.slot_to_port[data.slot_id as usize] = Some(port_id);

        let Some(device) =
            Self::device_by_slot_mut(&self.slot_to_port, &mut self.devices, data.slot_id)
        else {
            return CompletionCode::UsbTransactionError;
        };

        let worker_info = EndpointWorkerInfo {
            slot_id: data.slot_id,
            endpoint_id: 1,
            transfer_ring: device_context.get_transfer_ring(1),
            dma_bus: self.dma_bus.clone(),
            event_ring: self.event_ring.clone(),
            interrupt_line: self.interrupt_line.clone(),
        };

        // start control trb worker thread
        device.enable_endpoint(worker_info, EndpointType::Control);

        CompletionCode::Success
    }

    fn handle_configure_endpoint(
        &mut self,
        data: &ConfigureEndpointCommandTrbData,
    ) -> CompletionCode {
        if data.deconfigure {
            todo!("encountered Configure Endpoint Command with deconfigure set");
        }
        let Some(device) =
            Self::device_by_slot_mut(&self.slot_to_port, &mut self.devices, data.slot_id)
        else {
            return CompletionCode::UsbTransactionError;
        };

        let Some(device_context) = self.device_slot_manager.get_device_context(data.slot_id) else {
            return CompletionCode::UsbTransactionError;
        };
        let enabled_endpoints = device_context.configure_endpoints(data.input_context_pointer);

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
        CompletionCode::Success
    }

    fn handle_stop_endpoint(&self, data: &StopEndpointCommandTrbData) -> CompletionCode {
        let Some(device_context) = self.device_slot_manager.get_device_context(data.slot_id) else {
            return CompletionCode::UsbTransactionError;
        };
        device_context.set_endpoint_state(data.endpoint_id, endpoint_state::STOPPED);
        CompletionCode::Success
    }

    fn doorbell_device(&mut self, slot_id: u8, value: u32) {
        debug!("Ding Dong Device Slot {} with value {}!", slot_id, value);

        match value {
            ep if ep == 0 || ep > 31 => panic!("invalid value {ep} on doorbell write"),
            ep => {
                assert!(
                    u64::from(slot_id) <= MAX_SLOTS,
                    "invalid slot_id {slot_id} in doorbell"
                );
                if let Some(device) =
                    Self::device_by_slot_mut(&self.slot_to_port, &mut self.devices, slot_id)
                {
                    device.transfer(ep as u8);
                }
            }
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

            addr if guard.get_portsc_index(addr).is_some() => {
                // SAFETY: unwrap() is safe because we already checked is_some() in the match guard above
                let port_index = guard.get_portsc_index(addr).unwrap();
                // port ids start at 1, so we have to convert the MMIO address offset to the id
                let port_id = port_index + 1;
                guard.write_portsc(port_id, value);
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

            // Port Status and Control Register (PORTSC)
            addr if guard.get_portsc_index(addr).is_some() => {
                // SAFETY: unwrap() is safe because we already checked is_some() in the match guard above
                let port_index = guard.get_portsc_index(addr).unwrap();
                // port ids start at 1, so we have to convert the MMIO address offset to the id
                let port_id = port_index + 1;
                guard.portsc[port_id].read()
            }
            // Port Link Info Register (PORTLI_USB3)
            addr if guard.get_portli_index(addr).is_some() => 0,

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
