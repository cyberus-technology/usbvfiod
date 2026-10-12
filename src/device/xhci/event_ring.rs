use std::sync::atomic::fence;

use tracing::{debug, trace};

use crate::device::{
    bus::{BusDeviceRef, Request, RequestSize},
    pci::constants::xhci::rings::{
        event_ring::segments_table_entry_offsets::{SEGMENT_BASE, SIZE},
        TRB_SIZE,
    },
    xhci::trb::EventTrb,
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
    erst_count: u16,
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

    pub const fn reset(&mut self) {
        // The driver configures a fresh event ring after reset by writing ERSTBA.
        self.enqueue_pointer = 0;
        self.trb_count = 0;
        self.erst_count = 0;
        self.cycle_state = false;
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
    pub fn configure(&mut self, base_address: u64, erst_size: u16) {
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
        segment_table_size: u16,
        base_address: u64,
        dequeue_pointer: u64,
    ) {
        // TODO: Proper handling of full Event Ring
        // According to xHCI §4.9.4, the xHC must:
        //
        // 1. Stop fetching new TRBs from the Transfer and Command Rings.
        // 2. Emit an Event Ring Full Error Event TRB to the Event Ring (if supported).
        // 3. Advance the Event Ring Enqueue Pointer (EREP) accordingly.
        // 4. Wait for system software to use a Doorbell as a signal for free space
        //    after advancing the Event Ring Dequeue Pointer (ERDP).
        if self.check_event_ring_full(base_address, segment_table_size, dequeue_pointer) {
            todo!("The Event Ring is full! 0x{:x}", self.enqueue_pointer);
        }

        // write TRB with flipped cycle state first
        let trb_bytes = trb.to_bytes(!self.cycle_state);
        self.dma_bus.write_bulk(self.enqueue_pointer, &trb_bytes);

        // ensure TRB contents are written before toggling cycle bit
        fence(std::sync::atomic::Ordering::Release);

        // cycle-bit toggle releases TRB to the driver
        let byte12 = (trb_bytes[12] & !0x1) | if self.cycle_state { 0x1 } else { 0x0 };
        self.dma_bus.write(
            Request {
                addr: self.enqueue_pointer.wrapping_add(12),
                size: RequestSize::Size1,
            },
            byte12 as u64,
        );

        self.trb_count -= 1;

        trace!(
            "enqueued TRB in segment {} (total_segments={}) of event ring at address {:#x}. Space for {} more TRBs left in segment; cycle={}; (TRB: {:?})",
            self.erst_count, segment_table_size,  self.enqueue_pointer, self.trb_count, self.cycle_state, trb
        );

        self.advance_enqueue_pointer(base_address, segment_table_size);
    }

    /// Advances the enqueue pointer to the next slot in the event ring,
    /// wrapping to the start when the end of the segment is reached.
    fn advance_enqueue_pointer(&mut self, base_address: u64, erst_size: u16) {
        if self.trb_count == 0 {
            self.advance_segment_or_wrap(base_address, erst_size);
        } else {
            self.enqueue_pointer = self.enqueue_pointer.wrapping_add(TRB_SIZE as u64);
        }
    }

    /// Checks whether the Event Ring is full, based on xHCI §4.9.4.
    ///
    /// # Return
    /// - `true` if the Event Ring has one free slot and an Event Ring Full Error Event should be enqueued at the current position.
    /// - `false` if there is at least two slots available.
    fn check_event_ring_full(
        &self,
        base_address: u64,
        erst_size: u16,
        dequeue_pointer: u64,
    ) -> bool {
        const EVENT_RING_SEGMENT_TABLE_ENTRY_SIZE: u64 = 16;

        if self.trb_count == 1 {
            trace!("check_event_ring_full on self.trb_count == 1");
            trace!("dequeue_pointer: 0x{:x}", dequeue_pointer);

            let next_segment_table_entry_index = (self.erst_count + 1) % erst_size;

            let next_segment_table_entry_address = base_address.wrapping_add(
                next_segment_table_entry_index as u64 * EVENT_RING_SEGMENT_TABLE_ENTRY_SIZE,
            );

            let next_segment_base_address = self.dma_bus.read(Request::new(
                next_segment_table_entry_address.wrapping_add(SEGMENT_BASE),
                RequestSize::Size8,
            ));

            if dequeue_pointer == next_segment_base_address {
                debug!("event ring has one free slot");
                true
            } else {
                false
            }
        } else {
            trace!("check_event_ring_full on self.trb_count != 1");
            trace!("dequeue_pointer: 0x{:x}", dequeue_pointer);

            if dequeue_pointer == self.enqueue_pointer.wrapping_add(TRB_SIZE as u64) {
                debug!("event ring has one free slot");
                true
            } else {
                false
            }
        }
    }

    /// Advance to the next segment in the Event Ring Segment Table.
    ///
    /// Increments `erst_count` to move to the next segment. Wraps to segment 0
    /// and flips the producer cycle when the index reaches the end. Updates
    /// `enqueue_pointer` and `trb_count` from the selected ERST entry.
    fn advance_segment_or_wrap(&mut self, base_address: u64, erst_size: u16) {
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

#[cfg(test)]
mod tests {
    use crate::device::bus::testutils::TestBusDevice;
    use crate::device::xhci::registers::{ErdpRegister, ErstbaRegister, ErstszRegister};
    use crate::device::xhci::trb::CompletionCode;
    use std::sync::Arc;

    use super::*;

    /// This field defines the number of TRBs supported by the ring segment,
    /// Valid values for this field are 16 to 4096, i.e. an Event Ring segment
    /// shall contain at least 16 entries.
    const MIN_RING_SEGMENT_SIZE: u16 = 0x10;

    /// These fields represent the high order bits of the 64-bit base address
    /// of the Event Ring Segment. The memory structure referenced by this
    /// physical memory pointer shall begin on a 64-byte address boundary.
    const RING_SEGMENT_0_BASE: u64 = 0x40;
    const RING_SEGMENT_1_BASE: u64 = 0x180;
    const RING_SEGMENT_2_BASE: u64 = 0x280;

    #[derive(Default)]
    struct EventRingRegisterCollection {
        erstsz: ErstszRegister,
        erstba: ErstbaRegister,
        erdp: ErdpRegister,
    }

    impl EventRingRegisterCollection {
        fn new(erstsz: u64, erstba: u64, erdp: u64) -> Self {
            let dummy = Self::default();
            dummy.erstsz.write(erstsz);
            dummy.erstba.write(erstba);
            dummy.erdp.write(erdp);
            dummy
        }
    }

    /// Provides an event ring with up to (offset, trb_count):
    /// segment 1 (0x40, 16)
    /// unused memory (0x140, 4)
    /// segment 2 (0x180, 16)
    /// segment 3 (0x280, 16)
    ///
    /// The TestBusDevice is always allocated for erstsz = 3.
    fn init_ram_and_ring_and_registers(
        erstsz: Option<u32>,
    ) -> (Arc<TestBusDevice>, EventRing, EventRingRegisterCollection) {
        let erstsz = erstsz.map_or(3, |erstsz| {
            assert!(erstsz < 4);
            erstsz
        });

        let erst = [
            RING_SEGMENT_0_BASE.to_le_bytes()[0],
            RING_SEGMENT_0_BASE.to_le_bytes()[1],
            RING_SEGMENT_0_BASE.to_le_bytes()[2],
            RING_SEGMENT_0_BASE.to_le_bytes()[3],
            0x00,
            0x00,
            0x00,
            0x00,
            MIN_RING_SEGMENT_SIZE.to_le_bytes()[0],
            MIN_RING_SEGMENT_SIZE.to_le_bytes()[1],
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            //
            RING_SEGMENT_1_BASE.to_le_bytes()[0],
            RING_SEGMENT_1_BASE.to_le_bytes()[1],
            RING_SEGMENT_1_BASE.to_le_bytes()[2],
            RING_SEGMENT_1_BASE.to_le_bytes()[3],
            0x00,
            0x00,
            0x00,
            0x00,
            MIN_RING_SEGMENT_SIZE.to_le_bytes()[0],
            MIN_RING_SEGMENT_SIZE.to_le_bytes()[1],
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            //
            RING_SEGMENT_2_BASE.to_le_bytes()[0],
            RING_SEGMENT_2_BASE.to_le_bytes()[1],
            RING_SEGMENT_2_BASE.to_le_bytes()[2],
            RING_SEGMENT_2_BASE.to_le_bytes()[3],
            0x00,
            0x00,
            0x00,
            0x00,
            MIN_RING_SEGMENT_SIZE.to_le_bytes()[0],
            MIN_RING_SEGMENT_SIZE.to_le_bytes()[1],
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ];

        let ram = Arc::new(TestBusDevice::new(
            &[0; RING_SEGMENT_2_BASE as usize + TRB_SIZE * MIN_RING_SEGMENT_SIZE as usize],
        ));
        ram.write_bulk(0x0, &erst);
        let mut ring = EventRing::new(ram.clone());
        let reg = EventRingRegisterCollection::new(erstsz as u64, 0x0, RING_SEGMENT_0_BASE);

        ring.configure(0x0, reg.erstsz.segment_table_size());

        (ram, ring, reg)
    }

    fn dummy_event_trb() -> EventTrb {
        EventTrb::new_transfer_event_trb(
            0,                       // trb_pointer
            0,                       // trb_transfer_length
            CompletionCode::Success, // completion_code
            false,                   // event_data
            1,                       // endpoint_id
            1,                       // slot_id
        )
    }

    fn assert_trb_written(ram: &TestBusDevice, addr: u64, cycle_state: bool) {
        let mut buf = [0u8; 16];
        ram.read_bulk(addr, &mut buf);
        let cycle_bit = buf[12] & 0x1 != 0;
        assert_eq!(
            cycle_bit, cycle_state,
            "TRB not written at address {addr:#x}"
        );
    }

    #[test]
    fn event_ring_wraparound() {
        let (ram, mut ring, reg) = init_ram_and_ring_and_registers(None);

        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);

        // fill segment 0
        for i in 0..MIN_RING_SEGMENT_SIZE {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
            assert_trb_written(&ram, RING_SEGMENT_0_BASE + TRB_SIZE as u64 * i as u64, true);
            debug!("some {}", ring.enqueue_pointer);
        }

        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_1_BASE);

        // fill segment 1
        for i in 0..MIN_RING_SEGMENT_SIZE {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
            assert_trb_written(&ram, RING_SEGMENT_1_BASE + TRB_SIZE as u64 * i as u64, true);
        }

        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_2_BASE);

        // Move the dequeue pointer to not accidentally hit a full event ring.
        reg.erdp.write(RING_SEGMENT_1_BASE);

        // fill segment 2
        for i in 0..MIN_RING_SEGMENT_SIZE {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
            assert_trb_written(&ram, RING_SEGMENT_2_BASE + TRB_SIZE as u64 * i as u64, true);
        }

        // Every segment has the dummy event trb written once now.
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);

        // We wrapped around and write the first TRB slot on the first segment
        // with a toggled cycle bit.
        ring.enqueue(
            &dummy_event_trb(),
            reg.erstsz.segment_table_size(),
            reg.erstba.base_address(),
            reg.erdp.dequeue_pointer(),
        );
        assert_trb_written(&ram, RING_SEGMENT_0_BASE, false);
    }

    #[test]
    #[should_panic(expected = "not yet implemented: The Event Ring is full! 0x190")]
    fn event_ring_full_mid_segment() {
        let (_ram, mut ring, reg) = init_ram_and_ring_and_registers(None);

        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);

        // Move out of segment 0 so we can also set `ERDP.Dequeue ERST Segment Index`.
        for _ in 0..MIN_RING_SEGMENT_SIZE {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
        }

        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_1_BASE);

        // The event ring full detection does not look at the cycle state so we
        // can pretend the ring is full by moving the dequeue pointer in front
        // of the enqueue pointer.
        let segment_index_field = 0x1;
        let dequeue_pointer_field = RING_SEGMENT_1_BASE + TRB_SIZE as u64 * 2;
        reg.erdp.write(segment_index_field | dequeue_pointer_field);

        // With two free slots one can be queued without issue.
        ring.enqueue(
            &dummy_event_trb(),
            reg.erstsz.segment_table_size(),
            reg.erstba.base_address(),
            reg.erdp.dequeue_pointer(),
        );

        // One free slot will trigger an event ring full error.
        assert_eq!(
            ring.enqueue_pointer,
            reg.erdp.dequeue_pointer() - TRB_SIZE as u64
        );
        ring.enqueue(
            &dummy_event_trb(),
            reg.erstsz.segment_table_size(),
            reg.erstba.base_address(),
            reg.erdp.dequeue_pointer(),
        );
    }

    #[test]
    fn event_ring_multiple_wraparound() {
        let (ram, mut ring, reg) = init_ram_and_ring_and_registers(None);

        for n in 0..10 {
            let cycle = (n % 2) == 0;

            // fill segment 0
            assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);
            for _ in 0..MIN_RING_SEGMENT_SIZE {
                ring.enqueue(
                    &dummy_event_trb(),
                    reg.erstsz.segment_table_size(),
                    reg.erstba.base_address(),
                    reg.erdp.dequeue_pointer(),
                );
            }

            // Move the dequeue pointer out of the way to enable wraparound.
            reg.erdp.write(RING_SEGMENT_0_BASE + TRB_SIZE as u64);

            // fill segment 1 & 2
            for _ in 0..MIN_RING_SEGMENT_SIZE * 2 {
                ring.enqueue(
                    &dummy_event_trb(),
                    reg.erstsz.segment_table_size(),
                    reg.erstba.base_address(),
                    reg.erdp.dequeue_pointer(),
                );
            }
            assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);

            // Check cycle bit of the last TRB in the ring.
            assert_trb_written(
                &ram,
                RING_SEGMENT_2_BASE + TRB_SIZE as u64 * (MIN_RING_SEGMENT_SIZE - 1) as u64,
                cycle,
            );

            // Move the dequeue pointer back for the next loop.
            reg.erdp.write(RING_SEGMENT_0_BASE);
        }
    }

    #[test]
    fn event_ring_reset_clears_local_state() {
        let (_ram, mut ring, reg) = init_ram_and_ring_and_registers(None);

        ring.enqueue(
            &dummy_event_trb(),
            reg.erstsz.segment_table_size(),
            reg.erstba.base_address(),
            reg.erdp.dequeue_pointer(),
        );
        assert_ne!(ring.enqueue_pointer, 0);
        assert_ne!(ring.trb_count, 0);
        assert!(ring.cycle_state);

        ring.reset();

        assert_eq!(ring.enqueue_pointer, 0);
        assert_eq!(ring.trb_count, 0);
        assert_eq!(ring.erst_count, 0);
        assert!(!ring.cycle_state);
    }

    #[test]
    #[should_panic(expected = "ERSTSZ must be set before ERSTBA; misconfigured driver")]
    fn configure_requires_erstsz_first() {
        let erste = [
            RING_SEGMENT_0_BASE.to_le_bytes()[0],
            RING_SEGMENT_0_BASE.to_le_bytes()[1],
            RING_SEGMENT_0_BASE.to_le_bytes()[2],
            RING_SEGMENT_0_BASE.to_le_bytes()[3],
            0x00,
            0x00,
            0x00,
            0x00,
            MIN_RING_SEGMENT_SIZE.to_le_bytes()[0],
            MIN_RING_SEGMENT_SIZE.to_le_bytes()[1],
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ];

        let ram = Arc::new(TestBusDevice::new(
            &[0; RING_SEGMENT_0_BASE as usize + MIN_RING_SEGMENT_SIZE as usize],
        ));
        ram.write_bulk(0x0, &erste);
        let mut ring = EventRing::new(ram);

        ring.configure(0x0, 0x0);
    }

    #[test]
    fn event_ring_dynamic_grow_from_1_to_3() {
        let (ram, mut ring, reg) = init_ram_and_ring_and_registers(Some(1));

        // We start by only using the first entry of the segment table i.e. we
        // use only the first segment as the whole ring.
        assert_eq!(reg.erstsz.read(), 1);

        // Move dequeue pointer out of the way to enable wraparound.
        // This value is not on any segment. The system software owns this and
        // as far as I can see could set this to any nonsensual value.
        reg.erdp.write(0);

        // fill segment 0
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);
        for _ in 0..MIN_RING_SEGMENT_SIZE {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
        }
        assert_trb_written(
            &ram,
            RING_SEGMENT_0_BASE + TRB_SIZE as u64 * (MIN_RING_SEGMENT_SIZE - 1) as u64,
            true,
        );

        // The ring wrapped back to the initial address after writing the first segment.
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);

        // Now use all three segment table entries instead of only one.
        reg.erstsz.write(3);

        // fill segment 0
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);
        for _ in 0..MIN_RING_SEGMENT_SIZE {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
        }

        // This is the second loop on the ring, the cycle bit changed to 0.
        assert_trb_written(
            &ram,
            RING_SEGMENT_0_BASE + TRB_SIZE as u64 * (MIN_RING_SEGMENT_SIZE - 1) as u64,
            false,
        );

        // fill segment 1
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_1_BASE);
        for _ in 0..MIN_RING_SEGMENT_SIZE {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
        }
        assert_trb_written(
            &ram,
            RING_SEGMENT_1_BASE + TRB_SIZE as u64 * (MIN_RING_SEGMENT_SIZE - 1) as u64,
            false,
        );

        // fill segment 2
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_2_BASE);
        for _ in 0..MIN_RING_SEGMENT_SIZE {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
        }
        assert_trb_written(
            &ram,
            RING_SEGMENT_2_BASE + TRB_SIZE as u64 * (MIN_RING_SEGMENT_SIZE - 1) as u64,
            false,
        );

        // wraparound
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);
    }

    #[test]
    fn event_ring_dynamic_shrink_from_3_to_1() {
        let (ram, mut ring, reg) = init_ram_and_ring_and_registers(Some(3));

        // We start by only using the first entry of the segment table i.e. we
        // use only the first segment as a single segment ring.
        assert_eq!(reg.erstsz.read(), 3);

        // Move dequeue pointer out of the way to enable wraparound.
        // This value is not on any segment. The system software owns this and
        // as far as I can see could set this to any nonsensual value.
        reg.erdp.write(0);

        // fill segment 0
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);
        for _ in 0..MIN_RING_SEGMENT_SIZE {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
        }

        assert_trb_written(
            &ram,
            RING_SEGMENT_0_BASE + TRB_SIZE as u64 * (MIN_RING_SEGMENT_SIZE - 1) as u64,
            true,
        );

        // fill segment 1
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_1_BASE);
        for _ in 0..MIN_RING_SEGMENT_SIZE {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
        }
        assert_trb_written(
            &ram,
            RING_SEGMENT_1_BASE + TRB_SIZE as u64 * (MIN_RING_SEGMENT_SIZE - 1) as u64,
            true,
        );

        // fill segment 2
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_2_BASE);
        for _ in 0..MIN_RING_SEGMENT_SIZE {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
        }
        assert_trb_written(
            &ram,
            RING_SEGMENT_2_BASE + TRB_SIZE as u64 * (MIN_RING_SEGMENT_SIZE - 1) as u64,
            true,
        );

        // wraparound
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);

        // Now use only the first segment table entry.
        reg.erstsz.write(1);

        // fill segment 0
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);
        for _ in 0..MIN_RING_SEGMENT_SIZE {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
        }

        // This is the second loop on the ring, the cycle bit changed to 0.
        assert_trb_written(
            &ram,
            RING_SEGMENT_0_BASE + TRB_SIZE as u64 * (MIN_RING_SEGMENT_SIZE - 1) as u64,
            false,
        );

        // Wraparound after segment 0 is full.
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);
    }

    #[test]
    fn event_ring_dynamic_overwrite() {
        let (ram, mut ring, reg) = init_ram_and_ring_and_registers(Some(1));

        // Move dequeue pointer out of the way to enable wraparound.
        // This value is not on any segment. The system software owns this and
        // as far as I can see could set this to any nonsensual value.
        reg.erdp.write(0);

        // Only use first segment table entry.
        assert_eq!(reg.erstsz.read(), 1);

        // fill segment 0 partially
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);
        for _ in 0..(MIN_RING_SEGMENT_SIZE / 2) {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
        }

        let written_bytes = TRB_SIZE as u64 * (MIN_RING_SEGMENT_SIZE as u64 / 2);
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE + written_bytes);
        assert_trb_written(
            &ram,
            RING_SEGMENT_0_BASE + written_bytes - TRB_SIZE as u64,
            true,
        );

        // A replacement for the current segment table that will not touch the second and third entry.
        // Using an erstsz > 1 would overlap ring segments.
        let erst_new = [
            RING_SEGMENT_0_BASE.to_le_bytes()[0],
            RING_SEGMENT_0_BASE.to_le_bytes()[1],
            RING_SEGMENT_0_BASE.to_le_bytes()[2],
            RING_SEGMENT_0_BASE.to_le_bytes()[3],
            0x00,
            0x00,
            0x00,
            0x00,
            (MIN_RING_SEGMENT_SIZE * 3).to_le_bytes()[0],
            (MIN_RING_SEGMENT_SIZE * 3).to_le_bytes()[1],
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
        ];
        ram.write_bulk(0x0, &erst_new);

        /*
        The xHC may prefetch the next ERST entry before completely consuming the
        current ERST entry to avoid temporarily running out of Event Ring entries to
        write. As a result, if the xHC is currently writing to ERST(ERSTSZ - 1) and software
        increases the size of the Event Ring by writing a larger value to ERSTSZ (see
        Section 4.9.4.1) the xHC may have already prefetched ERST(0). In this case the
        effect of increasing the size of the Event Ring will take effect after the xHC
        consumes ERST(0). Similarly, if the xHC is currently writing to ERST(n) and
        software removes ERST(n+1) by shrinking the size of the Event Ring (see Section
        4.9.4.2) the xHC may have already prefetched ERST(n+1). In this case the effect
        of decreasing the size of the Event Ring will take effect after the xHC consumes
        ERST(n+1).
        */

        // fill remaining segment 0
        // Continue with the original erst entry for the segment, it is cached.
        for _ in (MIN_RING_SEGMENT_SIZE / 2)..MIN_RING_SEGMENT_SIZE {
            ring.enqueue(
                &dummy_event_trb(),
                reg.erstsz.segment_table_size(),
                reg.erstba.base_address(),
                reg.erdp.dequeue_pointer(),
            );
        }

        assert_trb_written(
            &ram,
            RING_SEGMENT_0_BASE + TRB_SIZE as u64 * (MIN_RING_SEGMENT_SIZE - 1) as u64,
            true,
        );

        // wraparound
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);

        // We do not prefetch following segments and can already use the updated length in this iteration.

        // fill segment 0 with the updated length
        for n in 1..4 {
            for _ in 0..MIN_RING_SEGMENT_SIZE {
                ring.enqueue(
                    &dummy_event_trb(),
                    reg.erstsz.segment_table_size(),
                    reg.erstba.base_address(),
                    reg.erdp.dequeue_pointer(),
                );
            }
            assert_trb_written(
                &ram,
                RING_SEGMENT_0_BASE + TRB_SIZE as u64 * (MIN_RING_SEGMENT_SIZE * n - 1) as u64,
                false,
            );
        }

        // wraparound
        assert_eq!(ring.enqueue_pointer, RING_SEGMENT_0_BASE);
    }
}
