use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    Arc,
};

use tokio::sync::Notify;

use crate::device::pci::constants::xhci::{
    operational::{portsc, usbsts},
    MAX_SLOTS,
};

/// A simple PORTSC register implementation supporting RW1C bits.
///
/// The PORTSC register requires us to initially set some bits and
/// later react to 1-to-clear writes (RW1C) to get a device to show up.
/// Perhaps later we need more fine-grained access to the bits or state
/// handling, but we can use the simplistic implementation for now.
#[derive(Debug)]
pub struct PortscRegister {
    value: AtomicU64,
}

const BITMASK_RW1C: u64 = 0x00260000;

impl Default for PortscRegister {
    fn default() -> Self {
        Self {
            value: AtomicU64::new(portsc::PP | portsc::value::PLS_RXDETECT),
        }
    }
}

impl PortscRegister {
    pub fn set(&self, value: u64) {
        self.value
            .store(value, std::sync::atomic::Ordering::Relaxed);
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
    /// RW1C bits are updates according to RW1C semantics, all
    /// other bits are treated as read-only.
    pub fn write(&self, new_value: u64) {
        let _ = self.value.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |reg| {
                let bits_to_clear = new_value & BITMASK_RW1C;
                let new_value = reg & !bits_to_clear;
                Some(new_value)
            },
        );
    }

    /// Update the masked bits with the given value.
    ///
    /// This function is absolute and does not respect RW rules imposed for
    /// driver access. It shall only be called as part of internal controller
    /// logic.
    /// Set bits in `value` not set in `mask` are silently dropped.
    pub fn update_with_mask(&self, value: u64, mask: u64) {
        let _previous_value = self.value.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |register| {
                let register_clear = register & !mask;
                let value_checked = value & mask;
                let new_register = value_checked | register_clear;
                Some(new_register)
            },
        );

        //self.value &= !mask;
        //self.value |= value & mask;
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
    use super::*;

    #[test]
    fn portsc_read_write() {
        let reg = PortscRegister::default();
        reg.set(0x00260203);
        assert_eq!(reg.read(), 0x00260203);

        reg.write(0x0);
        assert_eq!(
            reg.read(),
            0x00260203,
            "writing 0 should affect neither the read-only nor the RW1C bits."
        );

        reg.write(0x00200000);
        assert_eq!(
            reg.read(),
            0x00060203,
            "writing 1 to bit 21 should clear the bit."
        );

        reg.write(0x00040000);
        assert_eq!(
            reg.read(),
            0x00020203,
            "writing 1 to bit 18 should clear the bit."
        );

        reg.write(0x00020000);
        assert_eq!(
            reg.read(),
            0x00000203,
            "writing 1 to bit 17 should clear the bit."
        );
    }
}
