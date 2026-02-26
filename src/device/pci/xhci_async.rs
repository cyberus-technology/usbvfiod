use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use tokio::sync::mpsc;

use crate::device::{
    bus::{BusDeviceRef, SingleThreadedBusDevice},
    interrupt_line::InterruptLine,
    pci::{
        config_space::ConfigSpace,
        constants::xhci::{offset, RUN_BASE},
        device_slots::DeviceSlotManager,
        traits::PciDevice,
    },
};
use crate::device::{pci::constants::xhci::capability, xhci::command_ring::CommandRing};
use crate::device::{pci::constants::xhci::OP_BASE, xhci::interrupter::Interrupter};

#[derive(Debug)]
pub enum ControllerState {
    NotRunning,
    Running(ControllerRunningData),
}

#[derive(Debug)]
pub struct ControllerRunningData {
    event_ring_worker_sender: mpsc::Sender<()>,
    command_ring_worker_sender: mpsc::Sender<()>,
}

#[derive(Debug)]
pub struct XhciController {
    // devices
    dma_bus: BusDeviceRef,
    /// The PCI Configuration Space of the controller.
    ///
    /// Only the vfio-user thread accesses this field, so we use a standard Mutex
    /// instead of the tokio variant.
    config_space: Mutex<ConfigSpace>,
    state: ControllerState,
    interrupter: Arc<Interrupter>,
    command_ring: CommandRing,
    device_slot_manager: DeviceSlotManager,
}

impl XhciController {
    fn usbcmd_write(&self, value: u64) {
        // determine start/stop
    }
}

impl PciDevice for XhciController {
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
            offset::USBCMD => self.usbcmd_write(value),
            offset::DNCTL => assert_eq!(value, 2, "debug notifications not supported"),
            offset::CRCR => {} // guard.command_ring.control(value),
            offset::CRCR_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::DCBAAP => {} // guard.configure_device_contexts(value),
            offset::DCBAAP_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::CONFIG => {} // guard.enable_slots(value),
            // USBSTS writes occur but we can ignore them (to get a device enumerated)
            offset::USBSTS => {}
            // xHC Runtime Registers (moved up for performance)
            offset::IMAN => self.interrupter.write_iman(value),
            offset::IMOD => self.interrupter.write_imod(value),
            offset::ERSTSZ => {
                let sz = (value as u32) & 0xFFFF;
                self.interrupter.write_erstsz(sz);
            }
            offset::ERSTBA => self.interrupter.write_erstba(value),
            offset::ERSTBA_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::ERDP => self.interrupter.write_erdp(value),
            offset::ERDP_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::DOORBELL_CONTROLLER => {} //guard.doorbell_controller(),
            offset::DOORBELL_DEVICE..offset::DOORBELL_DEVICE_END => {} //{
            //     let slot_id = ((req.addr - offset::DOORBELL_CONTROLLER) / 4) as u8;
            //     guard.doorbell_device(slot_id, value as u32);
            // }
            addr if guard.get_portsc_index(addr).is_some() => {} //{
            //     // SAFETY: unwrap() is safe because we already checked is_some() in the match guard above
            //     let port_index = guard.get_portsc_index(addr).unwrap();
            //     // port ids start at 1, so we have to convert the MMIO address offset to the id
            //     let port_id = port_index + 1;
            //     guard.write_portsc(port_id, value);
            // }
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
            offset::USBSTS => guard.status(),
            offset::DNCTL => 2,
            offset::CRCR => self.command_ring.status(),
            offset::CRCR_HI => 0,
            offset::DCBAAP => guard.device_slot_manager.get_dcbaap(),
            offset::DCBAAP_HI => 0,
            offset::PAGESIZE => 0x1, /* 4k Pages */
            offset::CONFIG => guard.config(),

            // xHC Runtime Registers (moved up for performance)
            offset::IMAN => self.interrupter.read_iman(),
            offset::IMOD => self.interrupter.read_imod(),
            offset::ERSTSZ => self.interrupter.read_erstsz(),
            offset::ERSTBA => self.interrupter.read_erstba(),
            offset::ERSTBA_HI => 0,
            offset::ERDP => self.interrupter.read_erdp(),
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
}
