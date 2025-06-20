//! Abstractions of the rings (Event Ring, Command Ring, Transfer Ring) of a
//! USB3 Host (XHCI) controller.
//!
//! The specification is available
//! [here](https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf).

use tracing::{debug, trace};

use crate::device::{
    bus::{BusDeviceRef, Request, RequestSize},
    pci::constants::xhci::rings::event_ring::segments_table_entry_offsets::*,
};

use super::trb::EventTrb;

/// The Event Ring: A unidirectional means of communication, allowing the XHCI
/// controller to send events to the driver.
///
/// This implementation is a simplified version of the full mechanism specified
/// in the XHCI specification. We assume that the Event Ring Segment Table only
/// holds a single segment.
#[derive(Debug, Default, Clone)]
pub struct EventRing {
    /// The address of the Event Ring Segment Table.
    ///
    /// This field directly corresponds with the ERSTBA register(s) in the
    /// XHCI's MMIO region.
    base_address: u64,
    /// The Event Ring Dequeue Pointer.
    ///
    /// This field directly corresponds with the ERDP register(s) in the
    /// XHCI's MMIO region.
    /// The driver updates the pointer after processing one or multiple events.
    ///
    /// When the ring is not empty, the pointer indicates the address of the
    /// last processed TRB.
    /// When the ring is empty, the pointer is equal to the enqueue pointer
    /// (EREP).
    dequeue_pointer: u64,
    /// The Event Ring Enqueue Pointer (EREP).
    ///
    /// The EREP is an internal variable of the XHCI controller.
    /// The driver implicitly knows it reached the enqueue pointer (and thus
    /// can conclude the ring is empty), when it detects a cycle-bit mismatch
    /// at ERDP.
    enqueue_pointer: u64,
    /// The number of TRBs that fits into the current segment.
    ///
    /// The count is initialized from the size field of an Event Ring Segment
    /// Table Entry. Once the count reaches 0, we have to advance to the next
    /// segment---because we only support one, we move back to the start of the
    /// same segment.
    trb_count: u32,
    /// The producer cycle state.
    ///
    /// The driver tracks cycle state as well and can deduce the enqueue
    /// pointer by detecting cycle-state mismatches.
    /// Initially, the state has to be true (corresponds to TRB cycle bits
    /// equal to 1), so new TRBs can be written over the zero-initialized
    /// memory. Later, the cycle_state has to flip after every full pass of the
    /// event ring (i.e., in our case, when we move from the back of the
    /// segment to the front of the single segment).
    cycle_state: bool,
}

impl EventRing {
    /// Configure the Event Ring.
    ///
    /// Call this function when the driver writes to the ERSTBA register (as
    /// part of setting up the controller).
    /// Amongst setting the base address of the Event Ring Segment Table, this
    /// method initializes the enqueue_pointer to the start of the first and
    /// only segment, the trb_count to
    ///
    /// # Parameters
    ///
    /// - `erstba`: base address of the Event Ring Segment Table
    /// - `dma_bus`: the bus to use for DMA accesses
    pub fn configure(&mut self, erstba: u64, dma_bus: BusDeviceRef) {
        assert_eq!(erstba & 0x3f, 0, "unaligned event ring base address");

        self.base_address = erstba;
        self.enqueue_pointer = dma_bus.read(Request::new(erstba + BASE_ADDR, RequestSize::Size8));
        self.trb_count = dma_bus.read(Request::new(erstba + SIZE, RequestSize::Size4)) as u32;
        self.cycle_state = true;

        debug!("event ring segment table is at {:#x}", erstba);
        debug!(
            "initializing event ring enqueue pointer with base address of the first (and only) segment: {:#x}",
            self.enqueue_pointer
        );
        debug!(
            "retrieving TRB count of the first (and only) event ring segment from the segment table: {}",
            self.trb_count
        );
    }

    /// Handle writes to the Event Ring Dequeue Pointer (ERDP).
    ///
    /// # Parameters
    ///
    /// - `erdp`: value that the driver has written to the ERDP register.
    pub fn update_dequeue_pointer(&mut self, erdp: u64) {
        self.dequeue_pointer = erdp;
        debug!("driver set event ring dequeue pointer to {:#x}", erdp);
    }

    /// Handle reads to the Event Ring Segment Table Base Address (ERSTBA).
    pub const fn read_base_address(&self) -> u64 {
        self.base_address
    }

    /// Handle reads to the Event Ring Dequeue Pointer (ERDP).
    pub const fn read_dequeue_pointer(&self) -> u64 {
        self.dequeue_pointer
    }

    /// Enqueue an Event TRB to the ring.
    ///
    /// # Current Limitations
    ///
    /// The method is not capable of wrapping around to the start of the single
    /// segment. We fail once the first segment is full
    ///
    /// # Parameters
    ///
    /// - `trb`: the TRB to enqueue.
    /// - `dma_bus`: the bus to use for DMA accesses
    pub fn enqueue(&mut self, trb: &EventTrb, dma_bus: BusDeviceRef) {
        if self.check_event_ring_full() {
            todo!();
        }

        dma_bus.write_bulk(self.enqueue_pointer, &trb.to_bytes(self.cycle_state));

        let enqueue_address = self.enqueue_pointer;

        self.enqueue_pointer += 16;
        self.trb_count -= 1;

        trace!(
            "enqueued TRB in first segment of event ring at address {:#x}. Space for {} more TRBs left (TRB: {:?})",
            enqueue_address, self.trb_count, trb
        );
    }

    // The method is currently not capable of dealing with wrapping around to
    // the start of the single segment and just reports full once the segment
    // is filled up.
    const fn check_event_ring_full(&self) -> bool {
        self.trb_count == 0
    }
}
