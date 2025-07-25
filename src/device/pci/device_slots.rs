//! # Device Slot Handling
//!
//! This module offers an abstraction for device slots.

use crate::device::bus::{BusDeviceRef, Request, RequestSize};

use super::rings::TransferRing;

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
    pub num_slots: u64,
    /// Slots that are currently in use.
    used_slots: Vec<u64>,
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
    pub const fn new(num_slots: u64, dma_bus: BusDeviceRef) -> Self {
        assert!(num_slots > 0);
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

    pub const fn get_dcbaap(&self) -> u64 {
        self.dcbaap
    }

    /// Retrieve one of the available slot IDs.
    ///
    /// If a unused slot is available, this function returns the slot ID.
    /// Otherwise, it returns `Option::None`.
    ///
    /// This function has linear time complexity, which is reasonably fast for
    /// the use case of a handful of USB devices.
    pub fn reserve_slot(&mut self) -> Option<u64> {
        let available_slot_id =
            (1..=self.num_slots).find(|slot_id| !self.used_slots.contains(slot_id));

        if let Some(slot_id) = available_slot_id {
            self.used_slots.push(slot_id);
        }

        available_slot_id
    }

    /// Retrieve a device context abstraction.
    ///
    /// Device context are referenced by the DCBAA and indexed by the slot ID.
    /// There should not be accesses to device contexts of slots which were
    /// not previously allocated. Thus, this function panics if the context for
    /// an unused slot ID is requested.
    ///
    /// # Parameters
    ///
    /// - slot_id: the slot ID for which the DeviceContext is requested.
    pub fn get_device_context(&self, slot_id: u8) -> DeviceContext {
        assert!(
            self.used_slots.contains(&(slot_id as u64)),
            "requested DeviceContext for unassigned slot_id"
        );
        // lookup address of device context in device context base address array
        let device_context_address = self.dma_bus.read(Request::new(
            self.dcbaap.wrapping_add(slot_id as u64 * 8),
            RequestSize::Size8,
        ));

        DeviceContext::new(device_context_address, self.dma_bus.clone())
    }
}

/// A wrapper around DMA accesses to device context structures.
///
/// The structure is explained in the XHCI spec 6.2.1.
/// A device context consists of up to 32 entries. The first entry is a slot
/// context, the second entry is the endpoint context for the default control
/// endpoint. Both entries always exist. The remaining 30 entries can be used
/// for other endpoint contexts.
///
/// This struct does not store any state, it only acts as a wrapper for DMA
/// memory accesses.
#[derive(Debug)]
pub struct DeviceContext {
    /// The address of the device context in guest memory.
    address: u64,
    /// Reference to the guest memory.
    dma_bus: BusDeviceRef,
}

impl DeviceContext {
    /// Create a new instance.
    ///
    /// # Parameters
    ///
    /// - address: the address of the device context in guest memory.
    /// - dma_bus: reference to the guest memory.
    pub const fn new(address: u64, dma_bus: BusDeviceRef) -> Self {
        Self { address, dma_bus }
    }

    /// Initialize the device context with an input context.
    ///
    /// Call this function on AddressDeviceCommand. The command contains a
    /// pointer to an input context (which is this function's parameter).
    /// The XHCI controller is supposed to validate the values and copy the
    /// data to the device context---we only do the latter and assume the
    /// input is fine.
    ///
    /// The input context starts with an input control context, which indicates
    /// which following entries have to be considered.
    /// We assume that exactly the slot context and the default control
    /// endpoint get initialized and panic otherwise.
    ///
    /// Additional to copying the input context, we have to set the slot state
    /// in the slot context to "addressed" and the state in the endpoint
    /// context to running.
    ///
    /// # Parameters
    ///
    /// - addr_input_context: address of the input context used for
    ///   initialization.
    pub fn initialize(&self, addr_input_context: u64) {
        let add_drop_flags = self
            .dma_bus
            .read(Request::new(addr_input_context, RequestSize::Size8));
        assert!(
            add_drop_flags == 0x300000000,
            "expected only A0 and A1 flags to be set"
        );

        // read full input context
        let mut input_context = [0; 1056];
        self.dma_bus
            .read_bulk(addr_input_context, &mut input_context);

        // set slot state to addressed
        let slot_state_addressed = 2;
        input_context[32 + 15] = slot_state_addressed << 3;

        // set endpoint state to enabled
        let ep_state_running = 1;
        input_context[64] = ep_state_running;

        // fill slot context and ep0 context (as indicated by flags A0 and A1)
        self.dma_bus
            .write_bulk(self.address, &input_context[32..96]);
    }

    /// Give access to an endpoint context based on its index in the device
    /// context.
    ///
    /// The device context contains 32 entries of 32 bytes. The entries look as
    /// follows.
    ///
    /// - entry 0: slot context
    /// - entry 1: endpoint context for endpoint 0
    /// - entry 2: endpoint context for endpoint 1 OUT
    /// - entry 3: endpoint context for endpoint 1 IN
    /// - entry 4: endpoint context for endpoint 2 OUT
    /// - ...
    /// - entry 31: endpoint context for endpoint 15 IN
    ///
    /// # Parameters
    ///
    /// - index: index in the device context. `1 <= index <= 31`.
    fn get_endpoint_context_internal(&self, index: u64) -> EndpointContext {
        assert!((1..=31).contains(&index));

        let addr = self.address.wrapping_add(32 * index);
        EndpointContext::new(addr, self.dma_bus.clone())
    }

    /// Give access to context of the default control endpoint.
    ///
    /// Endpoint 0 is a special endpoint. It always exists and it is bi-directional.
    fn get_control_endpoint_context(&self) -> EndpointContext {
        // internal index 1 refers to the context of endpoint 0
        self.get_endpoint_context_internal(1)
    }

    /// Give access to the transfer ring of the default control endpoint.
    ///
    /// Endpoint 0 is a special endpoint. It always exists and it is bi-directional.
    pub fn get_control_transfer_ring(&self) -> TransferRing {
        TransferRing::new(self.get_control_endpoint_context(), self.dma_bus.clone())
    }
}

/// A wrapper around DMA accesses to endpoint context structures.
///
/// The structure is explained in the XHCI spec 6.2.3.
/// An endpoint context has a size of 32 bytes, lies in guest memory, and
/// contains information about an endpoint, most importantly for us the dequeue
/// pointer and cycle state of the associated transfer ring.
#[derive(Debug)]
pub struct EndpointContext {
    /// The address of the endpoint context in guest memory.
    address: u64,
    /// Reference to the guest memory.
    dma_bus: BusDeviceRef,
}

impl EndpointContext {
    /// Create a new instance.
    ///
    /// # Parameters
    ///
    /// - address: the address of the endpoint context in guest memory.
    /// - dma_bus: reference to the guest memory.
    pub const fn new(address: u64, dma_bus: BusDeviceRef) -> Self {
        Self { address, dma_bus }
    }

    /// DMA read the dequeue pointer and consumer cycle state of the endpoint's
    /// transfer ring.
    pub fn get_dequeue_pointer_and_cycle_state(&self) -> (u64, bool) {
        let bytes = self.dma_bus.read(Request::new(
            self.address.wrapping_add(8),
            RequestSize::Size8,
        ));
        let dequeue_pointer = bytes & !0xf;
        let cycle_state = bytes & 0x1 != 0;
        (dequeue_pointer, cycle_state)
    }

    /// DMA write the dequeue pointer and consumer cycle state of the endpoint's
    /// transfer ring.
    ///
    /// Call this function after retrieving TRBs from the transfer ring.
    pub fn set_dequeue_pointer_and_cycle_state(&self, dequeue_pointer: u64, cycle_state: bool) {
        assert!(
            dequeue_pointer & 0xf == 0,
            "dequeue_pointer has to be aligned to 16 bytes"
        );
        self.dma_bus.write(
            Request::new(self.address.wrapping_add(8), RequestSize::Size8),
            dequeue_pointer | cycle_state as u64,
        )
    }
}

#[cfg(test)]
mod tests {

    use std::sync::Arc;

    use crate::device::bus::BusDevice;

    use super::*;

    #[derive(Debug, Default)]
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
        let mut device_slot_manager = DeviceSlotManager::new(1, Arc::new(DummyMemory::default()));

        // reserve the only slot
        assert_eq!(Some(1), device_slot_manager.reserve_slot());

        // reserving another slot should fail
        assert_eq!(None, device_slot_manager.reserve_slot());
    }
}
