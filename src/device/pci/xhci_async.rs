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
        constants::xhci::offset,
        device_slots::DeviceSlotManager,
        rings::{CommandRing, EventRing},
        traits::PciDevice,
    },
};

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
    event_ring: EventRing,
    command_ring: CommandRing,
    device_slot_manager: DeviceSlotManager,
    /// Interrupt management register
    interrupt_management: AtomicU64,
    /// The minimum interval in 250ns increments between interrupts.
    interrupt_moderation_interval: AtomicU64,
    interrupt_line: Arc<dyn InterruptLine>,
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
            offset::IMAN => self.interrupt_management.store(value, Ordering::Relaxed),
            offset::IMOD => self
                .interrupt_moderation_interval
                .store(value, Ordering::Relaxed),
            offset::ERSTSZ => {} //{
            //     let sz = (value as u32) & 0xFFFF;
            //     guard.event_ring.lock().unwrap().set_erst_size(sz);
            // }
            offset::ERSTBA => {} //guard.event_ring.lock().unwrap().configure(value),
            offset::ERSTBA_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::ERDP => {} //guard
            // .event_ring
            // .lock()
            // .unwrap()
            // .update_dequeue_pointer(value),
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

    fn read_io(&self, region: u32, req: crate::device::bus::Request) -> u64 {}
}
