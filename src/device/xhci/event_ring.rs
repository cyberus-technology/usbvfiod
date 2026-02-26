use tracing::{debug, trace};

use crate::device::{
    bus::{BusDeviceRef, Request, RequestSize},
    pci::{
        constants::xhci::rings::{
            event_ring::segments_table_entry_offsets::{SEGMENT_BASE, SIZE},
            TRB_SIZE,
        },
        trb::EventTrb,
    },
};

///Ring: A unidirectional means of communication, allowing the XHCI
/// controller to send events to the driver.
///
/// This implementation supports multiple segments as specified in the XHCI
/// specification. The Event Ring can span multiple segments in the Event Ring
/// Segment Table.
#[derive(Debug)]
pub struct EventRing {
    /// Access to guest memory.
    ///
    /// The Event Ring lives in guest memory and we need DMA access to write
    /// events to the ring.
    dma_bus: BusDeviceRef,
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
    /// Table Entry. Once the count reaches 0, we advance to the next segment
    /// in the segment table, wrapping to segment 0 after the last segment.
    trb_count: u32,
    /// The index of the Event Ring segment currently being filled.
    ///
    /// The value is initialized to 0 (`ERST[0]`). When the current segment
    /// is exhausted, it advances to the next segment and wraps to 0 after
    /// the last segment.
    erst_count: u32,
    /// The producer cycle state.
    ///
    /// The driver tracks cycle state as well and can deduce the enqueue
    /// pointer by detecting cycle-state mismatches.
    /// Initially, the state has to be true (corresponds to TRB cycle bits
    /// equal to 1), so new TRBs can be written over the zero-initialized
    /// memory. Later, the cycle_state has to flip after every full pass of the
    /// event ring (i.e., when we wrap from the last segment back to segment 0).
    cycle_state: bool,
}

impl EventRing {
    /// Create a new Event Ring.
    ///
    /// # Parameters
    ///
    /// - dma_bus: access to guest memory
    pub fn new(dma_bus: BusDeviceRef) -> Self {
        Self {
            dma_bus,
            enqueue_pointer: 0,
            trb_count: 0,
            erst_count: 0,
            cycle_state: false,
        }
    }

    /// Configure the Event Ring.
    ///
    /// Call this function when the driver writes to the ERSTBA register (as
    /// part of setting up the controller).
    /// Besides setting the base address of the Event Ring Segment Table, this
    /// method initializes `enqueue_pointer` to the start of segment 0 and
    /// sets `trb_count` from `ERST[0]`.
    ///
    /// # Parameters
    ///
    /// - `erstba`: base address of the Event Ring Segment Table (ERST).
    pub fn configure(&mut self, base_address: u64, erst_size: u32) {
        assert_eq!(base_address & 0x3f, 0, "unaligned event ring base address");

        assert!(
            erst_size > 0,
            "ERSTSZ must be set before ERSTBA; misconfigured driver"
        );

        self.enqueue_pointer = self.dma_bus.read(Request::new(
            base_address.wrapping_add(SEGMENT_BASE),
            RequestSize::Size8,
        ));
        self.trb_count = self.dma_bus.read(Request::new(
            base_address.wrapping_add(SIZE),
            RequestSize::Size4,
        )) as u32;
        self.cycle_state = true;

        debug!("event ring segment table is at {:#x}", base_address);
        debug!(
            "initializing event ring enqueue pointer from ERST[0] base: {:#x}",
            self.enqueue_pointer
        );
        debug!(
            "retrieving TRB count of the first event ring segment from the segment table: {}",
            self.trb_count
        );
    }

    /// Enqueue a new Event TRB into the Ring.
    ///
    /// # Parameters
    /// - `trb`: the TRB to enqueue.
    ///
    /// # Limitations
    /// The current implementation does not handle ring-full recovery and will panic (`todo!()`) in that case.
    pub fn enqueue(
        &mut self,
        trb: &EventTrb,
        base_address: u64,
        erst_size: u32,
        dequeue_pointer: u64,
    ) {
        // TODO: Proper handling of full Event Ring
        // According to xHCI §4.9.4, the xHC must:
        //
        // 1. Stop fetching new TRBs from the Transfer and Command Rings.
        // 2. Emit an Event Ring Full Error Event TRB to the Event Ring (if supported).
        // 3. Advance the Event Ring Enqueue Pointer (EREP) accordingly.
        // 4. Wait for software (the host driver) to advance the Event Ring Dequeue Pointer (ERDP),
        //    at which point normal event generation can resume.
        if self.check_event_ring_full(base_address, erst_size, dequeue_pointer) {
            todo!("The Event Ring is full!");
        }

        self.dma_bus
            .write_bulk(self.enqueue_pointer, &trb.to_bytes(self.cycle_state));

        self.trb_count -= 1;

        trace!(
            "enqueued TRB in segment {} (total_segments={}) of event ring at address {:#x}. Space for {} more TRBs left in segment; cycle={}; (TRB: {:?})",
            self.erst_count, erst_size,  self.enqueue_pointer, self.trb_count, self.cycle_state, trb
        );

        self.advance_enqueue_pointer(base_address, erst_size);
    }

    /// Advances the enqueue pointer to the next slot in the event ring,
    /// wrapping to the start when the end of the segment is reached.
    fn advance_enqueue_pointer(&mut self, base_address: u64, erst_size: u32) {
        if self.trb_count == 0 {
            self.advance_segment_or_wrap(base_address, erst_size);
        } else {
            self.enqueue_pointer = self.enqueue_pointer.wrapping_add(TRB_SIZE as u64);
        }
    }

    /// Checks whether the Event Ring is full, based on xHCI §4.9.4.
    ///
    /// # Return
    /// - `true` if the Event Ring is full and an Event Ring Full Error Event should be enqueued at the current position.
    /// - `false` if there is at least one more slot available.
    fn check_event_ring_full(
        &self,
        base_address: u64,
        erst_size: u32,
        dequeue_pointer: u64,
    ) -> bool {
        if self.trb_count == 1 {
            let next_seg = (self.erst_count + 1) % erst_size;

            let entry_addr = base_address.wrapping_add((next_seg as u64) * 16);
            let next_seg_pointer = self.dma_bus.read(Request::new(
                entry_addr.wrapping_add(SEGMENT_BASE),
                RequestSize::Size8,
            ));

            dequeue_pointer == next_seg_pointer
        } else {
            dequeue_pointer == self.enqueue_pointer.wrapping_add(TRB_SIZE as u64)
        }
    }

    /// Advance to the next segment in the Event Ring Segment Table.
    ///
    /// Increments `erst_count` to move to the next segment. Wraps to segment 0
    /// and flips the producer cycle when the index reaches the end. Updates
    /// `enqueue_pointer` and `trb_count` from the selected ERST entry.
    fn advance_segment_or_wrap(&mut self, base_address: u64, erst_size: u32) {
        self.erst_count += 1;
        let wrapped = self.erst_count == erst_size;
        if wrapped {
            self.cycle_state = !self.cycle_state;
            self.erst_count = 0;
        }
        let entry_addr = base_address.wrapping_add((self.erst_count as u64) * 16);
        self.enqueue_pointer = self.dma_bus.read(Request::new(
            entry_addr.wrapping_add(SEGMENT_BASE),
            RequestSize::Size8,
        ));
        self.trb_count = self.dma_bus.read(Request::new(
            entry_addr.wrapping_add(SIZE),
            RequestSize::Size4,
        )) as u32;

        if wrapped {
            trace!(
                "wrapped to segment 0; base={:#x}, trb_count={}, cycle={}, total_segments={}",
                self.enqueue_pointer,
                self.trb_count,
                self.cycle_state,
                erst_size
            );
        } else {
            trace!(
                "advanced to segment {}; base={:#x}, trb_count={}, cycle={}, total_segments={}",
                self.erst_count,
                self.enqueue_pointer,
                self.trb_count,
                self.cycle_state,
                erst_size
            );
        }
    }
}

// #[cfg(test)]
// mod tests {
//     use crate::device::bus::testutils::TestBusDevice;
//     use crate::device::pci::trb::CompletionCode;
//     use std::sync::Arc;

//     use super::*;

//     fn init_ram_and_ring() -> (Arc<TestBusDevice>, EventRing) {
//         let erste = [
//             // segment 0
//             // segment_base = 0x30
//             // trb_count = 3
//             0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
//             0x00, 0x00,
//             // segment 1
//             // segment_base = 0x60
//             // trb_count = 1
//             0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
//             0x00, 0x00,
//             // segment 2
//             // segment_base = 0x70
//             // trb_count = 2
//             0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00,
//             0x00, 0x00,
//         ];

//         let ram = Arc::new(TestBusDevice::new(&[0; 0x90]));
//         ram.write_bulk(0x0, &erste);
//         let mut ring = EventRing::new(ram.clone());
//         ring.set_erst_size(3);
//         ring.configure(0x0);
//         ring.update_dequeue_pointer(
//             ring.dma_bus
//                 .read(Request::new(ring.base_address, RequestSize::Size8)),
//         );

//         (ram, ring)
//     }

//     fn dummy_trb() -> EventTrb {
//         EventTrb::new_transfer_event_trb(
//             0,                       // trb_pointer
//             0,                       // trb_transfer_length
//             CompletionCode::Success, // completion_code
//             false,                   // event_data
//             1,                       // endpoint_id
//             1,                       // slot_id
//         )
//     }

//     fn assert_trb_written(ram: &TestBusDevice, addr: u64, cycle_state: bool) {
//         let mut buf = [0u8; 16];
//         ram.read_bulk(addr, &mut buf);
//         let cycle_bit = buf[12] & 0x1 != 0;
//         assert_eq!(
//             cycle_bit, cycle_state,
//             "TRB not written at address {addr:#x}"
//         );
//     }

//     #[test]
//     fn event_ring_start_empty_enqueue_fill_then_wraparound_after_dequeue_pointer_move() {
//         let (ram, mut ring) = init_ram_and_ring();

//         // segment 0
//         ring.enqueue(&dummy_trb()); // TRB 1
//         ring.enqueue(&dummy_trb()); // TRB 2
//         ring.enqueue(&dummy_trb()); // TRB 3

//         assert_trb_written(&ram, 0x30, true);
//         assert_trb_written(&ram, 0x30 + 16, true);
//         assert_trb_written(&ram, 0x30 + 32, true);

//         ring.update_dequeue_pointer(0x30 + 32);

//         // segment 1
//         ring.enqueue(&dummy_trb()); // TRB 1

//         assert_trb_written(&ram, 0x60, true);

//         ring.update_dequeue_pointer(0x60);

//         // segment 2
//         ring.enqueue(&dummy_trb()); // TRB 1

//         assert_trb_written(&ram, 0x70, true);

//         ring.enqueue(&dummy_trb()); // TRB 2 and wraparound
//         assert_trb_written(&ram, 0x70 + 16, true);

//         ring.enqueue(&dummy_trb()); // write one more TRB after wraparound
//         assert_trb_written(&ram, 0x30, false);
//     }

//     #[test]
//     #[should_panic(expected = "Event Ring is full")]
//     fn event_ring_panics_on_wraparound_mid_segment_full() {
//         let (_ram, mut ring) = init_ram_and_ring();

//         // segment 0
//         ring.enqueue(&dummy_trb()); // TRB 1
//         ring.enqueue(&dummy_trb()); // TRB 2
//         ring.enqueue(&dummy_trb()); // TRB 3

//         ring.update_dequeue_pointer(0x30 + 16);

//         // segment 1
//         ring.enqueue(&dummy_trb()); // TRB 1

//         // segment 2
//         ring.enqueue(&dummy_trb()); // TRB 1
//         ring.enqueue(&dummy_trb()); // TRB 2 and wraparound

//         // segment 0
//         ring.enqueue(&dummy_trb()); // TRB 1

//         // ring is full now, the new TRB could not be written
//         // and test should panic
//         ring.enqueue(&dummy_trb());
//     }

//     #[test]
//     fn event_ring_multiple_wraparound() {
//         let (ram, mut ring) = init_ram_and_ring();

//         // ring 1
//         // segment 0
//         ring.enqueue(&dummy_trb()); // TRB 1
//         ring.enqueue(&dummy_trb()); // TRB 2
//         ring.enqueue(&dummy_trb()); // TRB 3

//         // segment 1
//         ring.enqueue(&dummy_trb()); // TRB 1

//         // segment 2
//         ring.enqueue(&dummy_trb()); // TRB 1
//         ring.update_dequeue_pointer(0x30 + 16);
//         ring.enqueue(&dummy_trb()); // TRB 2 and wraparound

//         // check the the last TRB's Cycle State of the ring
//         assert_trb_written(&ram, 0x80, true);

//         // ring 2
//         // segment 0
//         ring.update_dequeue_pointer(0x30 + 16 * 5);
//         ring.enqueue(&dummy_trb()); // TRB 1
//         ring.enqueue(&dummy_trb()); // TRB 2
//         ring.enqueue(&dummy_trb()); // TRB 3

//         // segment 1
//         ring.enqueue(&dummy_trb()); // TRB 1
//         ring.update_dequeue_pointer(0x30 + 32);

//         // segment 2
//         ring.enqueue(&dummy_trb()); // TRB 1
//         assert_trb_written(&ram, 0x70, false);
//         ring.enqueue(&dummy_trb()); // TRB 2 and wraparound

//         // check the the last TRB's Cycle State of the ring
//         assert_trb_written(&ram, 0x80, false);

//         // ring 3
//         // segment 0
//         ring.enqueue(&dummy_trb()); // TRB 1
//         assert_trb_written(&ram, 0x30, true);
//     }

//     #[test]
//     #[should_panic(expected = "ERSTSZ must be set before ERSTBA")]
//     fn configure_requires_erstsz_first() {
//         let erste = [
//             0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
//             0x00, 0x00, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
//             0x00, 0x00, 0x00, 0x00, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00,
//             0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
//         ];

//         let ram = Arc::new(TestBusDevice::new(&[0; 0x90]));
//         ram.write_bulk(0x0, &erste);
//         let mut ring = EventRing::new(ram);
//         ring.configure(0x0);
//         ring.update_dequeue_pointer(
//             ring.dma_bus
//                 .read(Request::new(ring.base_address, RequestSize::Size8)),
//         );
//     }

//     #[test]
//     fn event_ring_dynamic_grow_from_1_to_3() {
//         let erste = [
//             0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
//             0x00, 0x00, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
//             0x00, 0x00, 0x00, 0x00, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00,
//             0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
//         ];

//         let ram = Arc::new(TestBusDevice::new(&[0; 0x90]));
//         ram.write_bulk(0x0, &erste);
//         let mut ring = EventRing::new(ram.clone());
//         // set ERSTSZ = 1
//         ring.set_erst_size(1);
//         ring.configure(0x0);
//         ring.update_dequeue_pointer(
//             ring.dma_bus
//                 .read(Request::new(ring.base_address, RequestSize::Size8)),
//         );

//         // segment 0
//         ring.enqueue(&dummy_trb()); // TRB 1
//         ring.enqueue(&dummy_trb()); // TRB 2

//         ring.update_dequeue_pointer(0x30 + 16);
//         // set ERSTSZ to 3
//         ring.set_erst_size(3);

//         ring.enqueue(&dummy_trb()); // TRB 3
//         assert_trb_written(&ram, 0x30 + 32, true);

//         // should enter segment 1 without wraparound
//         ring.enqueue(&dummy_trb());
//         assert_trb_written(&ram, 0x60, true);

//         // continue write until the ring is full
//         ring.enqueue(&dummy_trb()); // TRB 1 in segment 2
//         ring.enqueue(&dummy_trb()); // TRB 2 in segment 2
//         assert_trb_written(&ram, 0x70, true);
//         assert_trb_written(&ram, 0x70 + 16, true);

//         // write one more TRB, it should be wraparound now
//         ring.update_dequeue_pointer(0x30 + 32);
//         ring.enqueue(&dummy_trb());
//         assert_trb_written(&ram, 0x30, false);
//     }

//     #[test]
//     fn event_ring_dynamic_shrink_to_1() {
//         let (ram, mut ring) = init_ram_and_ring();

//         ring.enqueue(&dummy_trb()); // TRB 1
//         ring.enqueue(&dummy_trb()); // TRB 2

//         ring.update_dequeue_pointer(0x30 + 16);

//         // before write the last TRB to segment 0, shrink ERSTSZ to 1
//         ring.set_erst_size(1);

//         ring.enqueue(&dummy_trb()); // TRB 3
//         assert_trb_written(&ram, 0x50, true);

//         ring.update_dequeue_pointer(0x30 + 32);

//         // wraparound
//         ring.enqueue(&dummy_trb());
//         assert_trb_written(&ram, 0x30, false);
//     }

//     #[test]
//     fn event_ring_dynamic_overwrite() {
//         let (ram, mut ring) = init_ram_and_ring();

//         // segment 0
//         ring.enqueue(&dummy_trb()); // TRB 1
//         ring.enqueue(&dummy_trb()); // TRB 2

//         // modify the segment 1
//         let erste_new = [
//             0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
//             0x00, 0x00, //set size of segment 1 to 2
//             0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00,
//             0x00, 0x00,
//         ];
//         ram.write_bulk(0x0, &erste_new);
//         ring.set_erst_size(2);

//         ring.enqueue(&dummy_trb()); // TRB 3 in segment 0
//         ring.update_dequeue_pointer(0x30 + 32);

//         // new segment 1
//         ring.enqueue(&dummy_trb()); // TRB 1
//         ring.enqueue(&dummy_trb()); // TRB 2
//         assert_trb_written(&ram, 0x60 + 16, true);

//         // should be wraparounded
//         ring.enqueue(&dummy_trb());
//         assert_trb_written(&ram, 0x30, false);
//     }
// }
