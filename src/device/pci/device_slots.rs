//! # Device Slot Handling
//!
//! This module offers an abstraction for device slots.

use crate::device::bus::BusDeviceRef;

#[derive(Debug, Clone)]
pub struct DeviceSlotManager {
    num_slots: usize,
    used_slots: Vec<usize>,
    dcbaap: u64,
    dma_bus: BusDeviceRef,
}

impl DeviceSlotManager {
    pub const fn new(num_slots: usize, dma_bus: BusDeviceRef) -> Self {
        Self {
            num_slots,
            used_slots: Vec::new(),
            dcbaap: 0,
            dma_bus,
        }
    }

    pub const fn set_dcbaap(&mut self, dcbaap: u64) {
        self.dcbaap = dcbaap;
    }

    pub fn reserve_slot(&mut self) -> Option<usize> {
        let available_slot_id =
            (1..=self.num_slots).find(|slot_id| !self.used_slots.contains(slot_id));

        if let Some(slot_id) = available_slot_id {
            self.used_slots.push(slot_id);
        }

        available_slot_id
    }
}
