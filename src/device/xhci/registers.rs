use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};

use tokio::sync::Notify;
use tracing::trace;

use crate::device::{
    pci::constants::xhci::{
        operational::{portsc, usbsts},
        MAX_SLOTS,
    },
    xhci::{interrupter::EventSender, port::UsbVersion, trb::EventTrb},
};

/// A somewhat simple PORTSC register implementation supporting RW1C bits and
/// handling custom port reset logic.
///
/// The PORTSC register requires us to initially set some bits and
/// later react to 1-to-clear writes (RW1C) to get a device to show up.
/// Additionally the PR bit is handled specific to this implementation to avoid
/// an actual reset. Instead we pretend it was successful.
/// We might need further specific logic or access to the bits or state
/// handling later, for now this implementation is enough.
#[derive(Debug)]
pub struct PortscRegister {
    value: AtomicU64,
    event_sender: EventSender,
    usb_version: UsbVersion,
    port_id: u8,
}

const BITMASK_RW1C: u64 = 0x00260000;

impl PortscRegister {
    pub const fn new(event_sender: EventSender, usb_version: UsbVersion, port_id: u8) -> Self {
        Self {
            value: AtomicU64::new(portsc::PP | portsc::value::PLS_RXDETECT),
            event_sender,
            usb_version,
            port_id,
        }
    }

    /// Write `value` to the register by overwriting the previously stored one.
    pub fn set(&self, value: u64) {
        self.value
            .store(value, std::sync::atomic::Ordering::Relaxed);
    }

    pub const fn usb_version(&self) -> UsbVersion {
        self.usb_version
    }

    /// Read the current register value.
    ///
    /// This function should be called when an MMIO read happens.
    pub fn read(&self) -> u64 {
        self.value.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Update the current register value.
    ///
    /// This function should be called when an MMIO write happens.
    /// RW1C bits are updates according to RW1C semantics.
    /// PR bit is handled by a custom logic path.
    /// All other bits are treated as read-only.
    pub fn write(&self, new_value: u64) -> anyhow::Result<()> {
        let bits_to_clear = new_value & BITMASK_RW1C;
        let port_reset_bit = new_value & portsc::PR != 0;

        match self.value.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |reg| {
                let mut new_reg = reg & !bits_to_clear;

                if port_reset_bit {
                    Self::port_reset(&mut new_reg, self.usb_version);
                }

                Some(new_reg)
            },
        ) {
            Ok(_) => {
                if port_reset_bit {
                    let event = EventTrb::new_port_status_change_event_trb(self.port_id);
                    self.event_sender.send(event)?;
                }
                Ok(())
            }
            Err(_) => unreachable!("update function never returns None"),
        }
    }

    fn port_reset(register: &mut u64, usb_version: UsbVersion) {
        match usb_version {
            UsbVersion::USB2 => {
                trace!("driver attempted to write portsc::PR on USB 2");
                let portsc_update_mask = portsc::PRC | portsc::PED | portsc::PLS;
                Self::update_with_mask(
                    register,
                    portsc::value::PLS_U0 | portsc::PED | portsc::PRC,
                    portsc_update_mask,
                );
            }
            UsbVersion::USB3 => {
                Self::update_with_mask(register, portsc::PRC, portsc::PRC);
            }
        }
    }

    /// Update the masked bits with the given value.
    ///
    /// This function is absolute and does not respect RW rules imposed for
    /// driver access. It shall only be called as part of internal controller
    /// logic.
    /// Set bits in `value` not set in `mask` are silently dropped.
    const fn update_with_mask(register: &mut u64, value: u64, mask: u64) {
        let register_clear = *register & !mask;
        let value_checked = value & mask;
        *register = value_checked | register_clear;
    }
}

/// Port Power Management Status and Control (chapter 5.4.9)
///
/// Limitations:
/// 1. no separation between RW and RWS
/// 1. no checks for RsvdP at: 17 <= bits <= 31
#[derive(Debug, Default, Clone)]
pub struct PortpmscRegister {
    value: Arc<AtomicU32>,
}
impl PortpmscRegister {
    pub fn read(&self) -> u32 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn write(&self, value: u32) {
        self.value.store(value, Ordering::Relaxed);
    }
}

#[derive(Debug, Default, Clone)]
pub struct ConfigureRegister {
    value: Arc<AtomicU32>,
}

impl ConfigureRegister {
    pub fn read(&self) -> u32 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn write(&self, value: u32) {
        let slots_enabled = (value & 0xff) as u8;
        assert!(slots_enabled <= MAX_SLOTS as u8);
        self.value.store(value, Ordering::Relaxed);
    }

    pub fn num_slots_enabled(&self) -> u8 {
        (self.read() & 0xff) as u8
    }
}

#[derive(Debug, Default, Clone)]
pub struct DcbaapRegister {
    value: Arc<AtomicU64>,
}

impl DcbaapRegister {
    pub fn read(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn write(&self, new_value: u64) {
        self.value.store(new_value & !0x1f, Ordering::Relaxed);
    }
}

#[derive(Debug)]
pub struct UsbcmdRegister {
    running: Arc<AtomicBool>,
}

impl UsbcmdRegister {
    pub fn new() -> Self {
        let running = Arc::new(AtomicBool::new(false));

        Self { running }
    }

    pub fn write(&self, value: u64) {
        self.running.store(value & 0x1 != 0, Ordering::Relaxed);
    }

    pub fn running_bit(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }
}

#[derive(Debug)]
pub struct UsbstsRegister {
    running: Arc<AtomicBool>,
}

impl UsbstsRegister {
    pub const fn new(running: Arc<AtomicBool>) -> Self {
        Self { running }
    }

    pub fn read(&self) -> u64 {
        let hch = if self.running.load(Ordering::Relaxed) {
            0
        } else {
            usbsts::HCH
        };
        hch | usbsts::EINT | usbsts::PCD
    }
}

#[derive(Debug, Default, Clone)]
pub struct GenericRwRegister {
    value: Arc<AtomicU64>,
}

impl GenericRwRegister {
    pub fn new(value: u64) -> Self {
        Self {
            value: Arc::new(AtomicU64::new(value)),
        }
    }

    pub fn read(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn write(&self, new_value: u64) {
        self.value.store(new_value, Ordering::Relaxed);
    }
}

#[derive(Debug, Default, Clone)]
pub struct ErstbaRegister {
    value: Arc<AtomicU64>,
    notify: Arc<Notify>,
}

impl ErstbaRegister {
    pub fn read(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }

    pub fn erstba(&self) -> u64 {
        self.value.load(Ordering::Relaxed) & !0x1f
    }

    pub fn write(&self, new_value: u64) {
        self.value.store(new_value, Ordering::Relaxed);
        self.notify.notify_waiters();
    }

    pub async fn write_notification(&self) {
        self.notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use crate::device::xhci::interrupter::Interrupter;
    use crate::dynamic_bus::DynamicBus;
    use crate::{init_runtime, runtime};

    use super::*;

    #[test]
    fn portsc_read_write() {
        // TODO this is conflicting with other tests using runtime (currently only this one)
        init_runtime().expect("Failed to initialize async runtime");
        let async_runtime = runtime();
        let dma_bus = Arc::new(DynamicBus::new());
        let interrupter = Interrupter::new(dma_bus, async_runtime);
        let reg = PortscRegister::new(interrupter.create_event_sender(), UsbVersion::USB3, 1);

        reg.set(0x00260203);
        assert_eq!(reg.read(), 0x00260203);

        reg.write(0x0).unwrap();
        assert_eq!(
            reg.read(),
            0x00260203,
            "writing 0 should affect neither the read-only nor the RW1C bits."
        );

        reg.write(0x00200000).unwrap();
        assert_eq!(
            reg.read(),
            0x00060203,
            "writing 1 to bit 21 should clear the bit."
        );

        reg.write(0x00040000).unwrap();
        assert_eq!(
            reg.read(),
            0x00020203,
            "writing 1 to bit 18 should clear the bit."
        );

        reg.write(0x00020000).unwrap();
        assert_eq!(
            reg.read(),
            0x00000203,
            "writing 1 to bit 17 should clear the bit."
        );
    }
}
