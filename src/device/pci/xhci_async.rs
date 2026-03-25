use std::sync::{Arc, Mutex};

use tokio::{runtime, sync::mpsc};

use crate::device::{
    bus::{BusDeviceRef, SingleThreadedBusDevice},
    interrupt_line::InterruptLine,
    pci::{
        config_space::{ConfigSpace, ConfigSpaceBuilder},
        constants::xhci::{offset, MAX_INTRS, RUN_BASE},
        registers::{new_usbcmd_and_usbsts, UsbcmdRegister, UsbstsRegister},
        traits::PciDevice,
    },
    xhci::{
        endpoint_launcher::EndpointLauncher,
        port::{get_portli_index, get_portsc_index, HotplugControl, PortArray},
        real_device::{Identifier, RealDevice},
        slot_manager::SlotManager,
    },
};
use crate::device::{pci::constants::xhci::capability, xhci::command_ring::CommandRing};
use crate::device::{pci::constants::xhci::OP_BASE, xhci::interrupter::Interrupter};

#[derive(Debug)]
pub struct XhciController<RD: RealDevice, ID: Identifier> {
    /// The PCI Configuration Space of the controller.
    ///
    /// Only the vfio-user thread accesses this field, so we use a standard Mutex
    /// instead of the tokio variant.
    config_space: Mutex<ConfigSpace>,
    interrupter: Interrupter,
    port_array: PortArray<RD, ID>,
    command_ring: CommandRing,
    slot_manager: SlotManager,
    usbcmd: UsbcmdRegister,
    usbsts: UsbstsRegister,
}

impl<RD: RealDevice, ID: Identifier> XhciController<RD, ID> {
    pub fn new(dma_bus: BusDeviceRef, async_runtime: runtime::Handle) -> Self {
        let interrupter = Interrupter::new(dma_bus.clone(), &async_runtime);
        let port_array = PortArray::new(interrupter.create_event_sender(), async_runtime.clone());
        let (ep_launch_sender, ep_launch_recv) = mpsc::unbounded_channel();
        EndpointLauncher::start(
            ep_launch_recv,
            port_array.msg_sender.clone(),
            async_runtime.clone(),
            dma_bus.clone(),
            interrupter.create_event_sender(),
        );
        let slot_manager = SlotManager::new(dma_bus.clone(), &async_runtime, ep_launch_sender);
        let command_ring = CommandRing::new(
            dma_bus.clone(),
            &async_runtime,
            interrupter.create_event_sender(),
            slot_manager.msg_send.clone(),
        );
        let (usbcmd, usbsts) = new_usbcmd_and_usbsts();

        Self {
            config_space: Mutex::new(Self::build_config_space()),
            interrupter,
            port_array,
            command_ring,
            slot_manager,
            usbcmd,
            usbsts,
        }
    }

    fn build_config_space() -> ConfigSpace {
        use crate::device::pci::constants::config_space::*;

        ConfigSpaceBuilder::new(vendor::REDHAT, device::REDHAT_XHCI)
            .class(class::SERIAL, subclass::SERIAL_USB, progif::USB_XHCI)
            // TODO Should be a 64-bit BAR.
            .mem32_nonprefetchable_bar(0, 4 * 0x1000)
            .mem32_nonprefetchable_bar(3, 2 * 0x1000)
            .msix_capability(MAX_INTRS.try_into().unwrap(), 3, 0, 3, 0x1000)
            .config_space()
    }

    pub fn connect_irq(&self, irq: Arc<dyn InterruptLine>) {
        self.interrupter
            .set_interrupt_line(irq)
            .expect("Interrupter should be alive");
    }

    pub fn hotplug_control(&self) -> HotplugControl<RD, ID> {
        self.port_array.create_hotplug_control()
    }
}

impl<RD: RealDevice, ID: Identifier> PciDevice for XhciController<RD, ID> {
    fn write_cfg(&self, req: crate::device::bus::Request, value: u64) {
        self.config_space.lock().unwrap().write(req, value);
    }

    fn read_cfg(&self, req: crate::device::bus::Request) -> u64 {
        self.config_space.lock().unwrap().read(req)
    }

    fn bar(&self, bar_no: u8) -> Option<super::config_space::BarInfo> {
        self.config_space.lock().unwrap().bar(bar_no)
    }

    fn write_io(&self, region: u32, req: crate::device::bus::Request, value: u64) {
        // The XHCI Controller has a single MMIO BAR.
        assert_eq!(region, 0);

        match req.addr {
            // xHC Operational Registers
            offset::USBCMD => self.usbcmd.write(value),
            offset::DNCTL => assert_eq!(value, 2, "debug notifications not supported"),
            offset::CRCR => self
                .command_ring
                .control(value)
                .expect("command worker should be alive"),
            offset::CRCR_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::DCBAAP => self.slot_manager.dcbaap.write(value),
            offset::DCBAAP_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::CONFIG => self.slot_manager.config_reg.write(value as u32), // guard.enable_slots(value),
            // USBSTS writes occur but we can ignore them (to get a device enumerated)
            offset::USBSTS => {}
            // xHC Runtime Registers (moved up for performance)
            offset::IMAN => self.interrupter.registers.interrupt_management.write(value),
            offset::IMOD => self
                .interrupter
                .registers
                .interrupt_moderation_interval
                .write(value),
            offset::ERSTSZ => {
                self.interrupter.registers.erst_size.write(value);
            }
            offset::ERSTBA => self.interrupter.registers.erst_base_address.write(value),
            offset::ERSTBA_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::ERDP => self
                .interrupter
                .registers
                .eventring_dequeue_pointer
                .write(value),
            offset::ERDP_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::DOORBELL_CONTROLLER => self
                .command_ring
                .doorbell()
                .expect("command worker should be alive"),
            offset::DOORBELL_DEVICE..offset::DOORBELL_DEVICE_END => {
                let slot_id = ((req.addr - offset::DOORBELL_CONTROLLER) / 4) as u8;
                self.slot_manager.doorbell(slot_id, value as u8);
            }
            addr if get_portsc_index(addr).is_some() => {
                // SAFETY: unwrap() is safe because we already checked is_some() in the match guard above
                let port_index = get_portsc_index(addr).unwrap();
                // port ids start at 1, so we have to convert the MMIO address offset to the id
                let port_id = port_index + 1;
                self.port_array.write_portsc(port_id, value);
            }
            addr => {
                todo!("unknown write {}", addr);
            }
        }
    }

    fn read_io(&self, region: u32, req: crate::device::bus::Request) -> u64 {
        // The XHCI Controller has a single MMIO BAR.
        assert_eq!(region, 0);

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
            offset::USBSTS => self.usbsts.read(),
            offset::DNCTL => 2,
            offset::CRCR => self.command_ring.status(),
            offset::CRCR_HI => 0,
            offset::DCBAAP => self.slot_manager.dcbaap.read(),
            offset::DCBAAP_HI => 0,
            offset::PAGESIZE => 0x1, /* 4k Pages */
            offset::CONFIG => self.slot_manager.config_reg.read() as u64,

            // xHC Runtime Registers (moved up for performance)
            offset::IMAN => self.interrupter.registers.interrupt_management.read(),
            offset::IMOD => self
                .interrupter
                .registers
                .interrupt_moderation_interval
                .read(),
            offset::ERSTSZ => self.interrupter.registers.erst_base_address.read(),
            offset::ERSTBA => self.interrupter.registers.erst_size.read(),
            offset::ERSTBA_HI => 0,
            offset::ERDP => self.interrupter.registers.eventring_dequeue_pointer.read(),
            offset::ERDP_HI => 0,
            offset::DOORBELL_CONTROLLER => 0, // kernel reads the doorbell after write
            // Device Doorbell Registers (DOORBELL_DEVICE)
            offset::DOORBELL_DEVICE..offset::DOORBELL_DEVICE_END => 0,

            // Port Status and Control Register (PORTSC)
            addr if get_portsc_index(addr).is_some() => {
                // SAFETY: unwrap() is safe because we already checked is_some() in the match guard above
                let port_index = get_portsc_index(addr).unwrap();
                // port ids start at 1, so we have to convert the MMIO address offset to the id
                let port_id = port_index + 1;
                self.port_array.read_portsc(port_id)
            }
            // Port Link Info Register (PORTLI_USB3)
            addr if get_portli_index(addr).is_some() => 0,

            // Everything else is Reserved Zero
            addr => {
                todo!("unknown read {}", addr);
            }
        }
    }
}
