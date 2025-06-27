//! # Device Slot Handling
//!
//! This module offers an abstraction for device slots.

use crate::device::bus::{BusDeviceRef, Request, RequestSize};

use super::rings::TransferRing;

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

    pub const fn get_dcbaap(&self) -> u64 {
        self.dcbaap
    }

    pub fn reserve_slot(&mut self) -> Option<usize> {
        let available_slot_id =
            (1..=self.num_slots).find(|slot_id| !self.used_slots.contains(slot_id));

        if let Some(slot_id) = available_slot_id {
            self.used_slots.push(slot_id);
        }

        available_slot_id
    }

    pub fn get_device_context(&self, slot_id: u8) -> DeviceContext {
        assert!(
            self.used_slots.contains(&(slot_id as usize)),
            "requested DeviceContext for unassigned slot_id"
        );
        // lookup address of device context in device context base address array
        let device_context_address = self.dma_bus.read(Request::new(
            self.dcbaap + slot_id as u64 * 8,
            RequestSize::Size8,
        ));

        DeviceContext::new(device_context_address, self.dma_bus.clone())
    }
}

pub enum EndpointDirection {
    Out,
    In,
}

/// XHCI spec 6.2.1
#[derive(Debug)]
pub struct DeviceContext {
    address: u64,
    dma_bus: BusDeviceRef,
}

impl DeviceContext {
    pub const fn new(address: u64, dma_bus: BusDeviceRef) -> Self {
        Self { address, dma_bus }
    }

    /// call on AddressDeviceCommand
    pub fn initialize(&self, addr_input_context: u64) {
        let add_drop_flags = self
            .dma_bus
            .read(Request::new(addr_input_context, RequestSize::Size8));
        assert!(
            add_drop_flags == 0x300000000,
            "expected only A0 and A1 flags to be set"
        );

        let mut input_context = [0; 1056];
        self.dma_bus
            .read_bulk(addr_input_context, &mut input_context);

        // copy slot context (as indicated by flag A0)
        self.dma_bus
            .write_bulk(self.address, &input_context[32..64]);

        // copy endpoint context 0 (as indicated by flag A1)
        self.dma_bus
            .write_bulk(self.address + 32, &input_context[64..96]);

        // set slot state to addressed
        let slot_state_addressed = 2;
        self.dma_bus
            .write_bulk(self.address + 15, &[slot_state_addressed << 3; 1]);

        // set endpoint state to enabled
        let ep_state_running = 1;
        self.dma_bus
            .write_bulk(self.address + 32, &[ep_state_running]);
    }

    /// indices of the device context (1 to 31)
    fn get_endpoint_context_internal(&self, index: u64) -> EndpointContext {
        assert!(
            index >= 1 && index <= 31,
            "index has to be between 1 and 31 (inclusive)"
        );

        EndpointContext::new(self.address + 32 * index, self.dma_bus.clone())
    }

    /// indices of the endpoints (0 to 15, but 0 should not be requested with
    /// this function; use get_control_endpoint_context instead)
    fn get_endpoint_context(&self, ep_index: u64, dir: EndpointDirection) -> EndpointContext {
        assert!(
            ep_index >= 0 && ep_index <= 15,
            "endpoint index has to be between 0 and 15 (inclusive)"
        );

        let direction_offset = match dir {
            EndpointDirection::Out => 0,
            EndpointDirection::In => 1,
        };
        let index = (ep_index * 2 + direction_offset);
        self.get_endpoint_context_internal(index)
    }

    /// Endpoint 0 is a special endpoint. It always exists and it is bi-directional.
    fn get_control_endpoint_context(&self) -> EndpointContext {
        self.get_endpoint_context_internal(1)
    }

    pub fn get_transfer_ring(&self, ep_index: u64, dir: EndpointDirection) -> TransferRing {
        TransferRing::new(
            self.get_endpoint_context(ep_index, dir),
            self.dma_bus.clone(),
        )
    }

    pub fn get_control_transfer_ring(&self) -> TransferRing {
        TransferRing::new(self.get_control_endpoint_context(), self.dma_bus.clone())
    }
}

/// XHCI spec 6.2.3
#[derive(Debug)]
pub struct EndpointContext {
    address: u64,
    dma_bus: BusDeviceRef,
}

impl EndpointContext {
    const fn new(address: u64, dma_bus: BusDeviceRef) -> Self {
        Self { address, dma_bus }
    }

    pub fn get_dequeue_pointer_and_cycle_state(&self) -> (u64, bool) {
        let bytes = self
            .dma_bus
            .read(Request::new(self.address + 8, RequestSize::Size8));
        let dequeue_pointer = bytes & !0xf;
        let cycle_state = bytes & 0x1 != 0;
        (dequeue_pointer, cycle_state)
    }

    pub fn set_dequeue_pointer_and_cycle_state(&self, dequeue_pointer: u64, cycle_state: bool) {
        assert!(
            dequeue_pointer & 0xf == 0,
            "dequeue_pointer has to be aligned to 16 bytes"
        );
        self.dma_bus.write(
            Request::new(self.address + 8, RequestSize::Size8),
            dequeue_pointer | cycle_state as u64,
        )
    }
}
