//! # Device Slot Handling
//!
//! This module offers an abstraction for device slots.

use crate::device::bus::BusDeviceRef;

/// Abstraction for Device Slots.
///
/// Each USB device needs a device slot ID to be addressable.
/// The slot ID is used in several places:
///
/// - index of the device context base address array (DCBAA), which points to
///   the associated device context.
/// - index of the doorbell register.
/// - referenced in event and command TRBs
///
/// The XHCI controller reports the maximum number of device slots in the
/// HCSPARAMS1 register. For device initialization, the driver requests a slot
/// ID using the Enable Slot Command. The `DeviceSlotManager` is responsible
/// of tracking which slot IDs are currently in use.
#[derive(Debug, Clone)]
pub struct DeviceSlotManager {
    /// Number of available slots.
    num_slots: usize,
    /// Slots that are currently in use.
    used_slots: Vec<usize>,
    /// DMA address of the device context base address array.
    dcbaap: u64,
    /// Reference to the guest memory.
    dma_bus: BusDeviceRef,
}

impl DeviceSlotManager {
    /// Construct a new instance.
    ///
    /// There should only exist one `DeviceSlotManager` per `XhciController`.
    ///
    /// # Parameters
    ///
    /// - num_slots: number of available slots. Use the same value as the
    ///   controller reports in HCSPARAMS1.
    /// - dma_bus: a reference to the guest's memory.
    pub const fn new(num_slots: usize, dma_bus: BusDeviceRef) -> Self {
        Self {
            num_slots,
            used_slots: Vec::new(),
            dcbaap: 0,
            dma_bus,
        }
    }

    /// Set the address to the DCBAA.
    ///
    /// Call this function on writes to the DCBAAP MMIO register.
    pub const fn set_dcbaap(&mut self, dcbaap: u64) {
        self.dcbaap = dcbaap;
    }

    /// Retrieve one of the available slot IDs.
    ///
    /// If a unused slot is available, this function returns the slot ID.
    /// Otherwise, it returns `Option::None`.
    pub fn reserve_slot(&mut self) -> Option<usize> {
        let available_slot_id =
            (1..=self.num_slots).find(|slot_id| !self.used_slots.contains(slot_id));

        if let Some(slot_id) = available_slot_id {
            self.used_slots.push(slot_id);
        }

        available_slot_id
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use crate::device::bus::BusDevice;

    use super::*;

    #[derive(Debug)]
    struct DummyMemory {}

    impl BusDevice for DummyMemory {
        fn size(&self) -> u64 {
            0
        }

        fn read(&self, _: crate::device::bus::Request) -> u64 {
            0
        }

        fn write(&self, _: crate::device::bus::Request, _: u64) {}
    }

    #[test]
    fn device_slot_reservation() {
        // we test with only one device slot, because that case is currently
        // what we run with
        let mut device_slot_manager = DeviceSlotManager::new(1, Arc::new(DummyMemory {}));

        // reserve the only slot
        assert_eq!(Some(1), device_slot_manager.reserve_slot());

        // reserving another slot should fail
        assert_eq!(None, device_slot_manager.reserve_slot());
    }
}
