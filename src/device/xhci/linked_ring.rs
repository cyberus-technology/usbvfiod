use tracing::{trace, warn};

use crate::device::{
    bus::BusDeviceRef,
    pci::constants::xhci::rings::TRB_SIZE,
    xhci::trb::{zeroed_trb_buffer, LinkTrb, RawTrb, RawTrbBuffer},
};

/// Transfer Rings: Unidirectional means of communication, allowing the
/// driver to send requests over the XHCI controller to device endpoints.
///
/// All state lives in guest memory, this struct is merely a wrapper providing
/// convenient methods to access the rings.
#[derive(Debug)]
pub struct LinkedRing {
    dequeue_pointer: u64,
    cycle_state: bool,
    /// A reference to guest memory.
    dma_bus: BusDeviceRef,
}

impl LinkedRing {
    /// Create a new instance
    ///
    /// # Parameters
    ///
    /// - `endpoint_context`: the endpoint the rings belongs to.
    /// - `dma_bus`: a reference to guest memory.
    pub fn new(dma_bus: BusDeviceRef, dequeue_pointer: u64, cycle_state: bool) -> Self {
        Self {
            dequeue_pointer,
            cycle_state,
            dma_bus,
        }
    }

    /// Try to retrieve a new TRB from a transfer ring.
    ///
    /// This function only returns `TransferTrb`s that are not Link TRBs.
    /// Instead, Link TRBs are handled correctly, which is the reason why the
    /// function might read multiple TRBs to return a single one. When a Link
    /// TRB is handled the dequeue pointer is set to the Link TRB's data field
    /// value.
    ///
    /// Encountering multiple consecutive LinkTrb is described in the xhci specification as:
    /// - allowed in the form of Link TD (chapter 4.11.7)
    /// - undefined behavior within the same TD (chapter 4.11.7)
    /// - having performance degrading effects (chapter 6.4.4.1).
    ///
    /// There is an upper limit on how many Link TRB are tolerated before this
    /// function will return None instead of following more Link TRB. A sensible
    /// driver should not hit that limit.
    pub fn next_trb(&mut self) -> Option<RawTrb> {
        // retrieve TRB at dequeue pointer and return None if there is no fresh
        // TRB
        let mut buffer = self.next_trb_raw()?;

        let mut link_trb_counter = 0;
        while let Some(link_trb) = LinkTrb::parse(buffer) {
            if link_trb_counter == 256 {
                warn!("encountered unreasonable amount of consecutive Link TRB");
                return None;
            }
            link_trb_counter += 1;

            // encountered Link TRB
            // update dequeue pointer.
            self.dequeue_pointer = link_trb.ring_segment_pointer;

            if link_trb.toggle_cycle {
                self.cycle_state = !self.cycle_state;
            }

            // lookup TRB in the new memory segment
            buffer = self.next_trb_raw()?;
        }

        let address = self.dequeue_pointer;
        let trb = RawTrb { address, buffer };

        Some(trb)
    }

    /// Try to retrieve a new TRB from a transfer ring.
    ///
    /// If there is a fresh TRB at the dequeue pointer, the function tries to
    /// parse the transfer TRB and returns the result. If there is a fresh Link
    /// TRB, this function will return it!
    fn next_trb_raw(&self) -> Option<RawTrbBuffer> {
        // retrieve TRB at current dequeue_pointer
        let mut trb_buffer = zeroed_trb_buffer();
        self.dma_bus
            .read_bulk(self.dequeue_pointer, &mut trb_buffer);

        trace!(
            "interpreting TRB at dequeue pointer; cycle state = {}, TRB = {:?}",
            self.cycle_state as u8,
            trb_buffer
        );

        // check if the TRB is fresh
        let cycle_bit = trb_buffer[12] & 0x1 != 0;
        if cycle_bit != self.cycle_state {
            // cycle-bit mismatch: no new TRB available
            return None;
        }

        // TRB is fresh; return it
        Some(trb_buffer)
    }

    pub const fn advance(&mut self) {
        // advance to next TRB
        self.dequeue_pointer = self.dequeue_pointer.wrapping_add(TRB_SIZE as u64);
    }

    pub const fn set_dequeue_pointer(&mut self, dequeue_pointer: u64, cycle_state: bool) {
        self.dequeue_pointer = dequeue_pointer;
        self.cycle_state = cycle_state;
    }

    /// returns (dequeue_pointer, cycle_state)
    pub const fn get_dequeue_pointer(&self) -> (u64, bool) {
        (self.dequeue_pointer, self.cycle_state)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        device::{
            bus::{testutils::TestBusDevice, BusDevice},
            pci::constants::xhci::rings::trb_types,
            xhci::trb::testutils::RawTrbBuilder,
        },
        dynamic_bus::DynamicBus,
    };
    use std::sync::Arc;

    use super::*;

    const FIRST_ADDRESS: u64 = 0x00;
    const THIRD_ADDRESS: u64 = 0x20;

    // test summary:
    //
    // This test checks the retrieval of raw TRBs according to the cycle state and cycle bits,
    // as well as the correct handling of wrap around/Link TRBs.
    //
    // steps:
    //
    // - linked ring with 5 TRBs
    // - prepare
    //   [Setup Stage] [Data Stage] [Status Stage] [non-fresh TRB] [non-fresh TRB]
    // - the three TRBs should be retrievable
    // - prepare
    //   [Status Stage] [non-fresh TRB] [non-fresh TRB] [Setup Stage] [Link]
    // - the two TRBs should be retrievable
    #[test]
    fn linked_ring_retrieve_trbs() {
        let setup = [
            0x11, 0x22, 0x44, 0x33, 0x66, 0x55, 0x88, 0x77, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08,
            0x00, 0x00,
        ];
        let data = [
            0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0c,
            0x00, 0x00,
        ];
        let status = [
            0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x10, 0x0, 0x0,
        ];
        let link = [
            0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0x18, 0x0, 0x0,
        ];

        // construct memory segment for a ring that can contain 5 TRBs
        let ram = Arc::new(TestBusDevice::new(&[0; TRB_SIZE * 5]));
        let mut ring = LinkedRing::new(ram.clone(), 0x0, true);

        // the ring is still empty
        let trb = ring.next_trb();
        assert!(
            trb.is_none(),
            "When no fresh TRB is on the ring, next_trb should return None, instead got: {trb:?}"
        );

        // place three TRBs and set their cycle bit
        // setup
        ram.write_bulk(0, &setup);
        ram.write_bulk(12, &[0x1]);

        // data
        ram.write_bulk(TRB_SIZE as u64, &data);
        ram.write_bulk(TRB_SIZE as u64 + 12, &[0x1]);

        // status
        ram.write_bulk(TRB_SIZE as u64 * 2, &status);
        ram.write_bulk(TRB_SIZE as u64 * 2 + 12, &[0x1]);

        // ring abstraction should parse first TRB correctly
        check_trb(ring.next_trb(), 0x00, setup);

        // without manually advancing, we should receive the same TRB again
        check_trb(ring.next_trb(), 0x00, setup);

        // ring abstraction should parse second TRB correctly
        ring.advance();
        check_trb(ring.next_trb(), 0x10, data);

        // ring abstraction should parse third TRB correctly
        ring.advance();
        check_trb(ring.next_trb(), 0x20, status);

        // no new TRB placed, should return no new TRB
        ring.advance();
        let trb = ring.next_trb();
        assert!(
            trb.is_none(),
            "When no fresh TRB is on the transfer ring, next_trb should return None, instead got: {trb:?}"
        );

        // place second batch of TRBs (include link TRB because the ring needs to
        // wrap around)
        // setup
        ram.write_bulk(TRB_SIZE as u64 * 3, &setup);
        ram.write_bulk(TRB_SIZE as u64 * 3 + 12, &[0x1]);

        // link
        ram.write_bulk(TRB_SIZE as u64 * 4, &link);
        ram.write_bulk(TRB_SIZE as u64 * 4 + 12, &[0x1]);
        // set cycle bit without affecting the toggle_cycle bit
        ram.write_bulk(TRB_SIZE as u64 * 4 + 12, &[0x1 | link[12]]);

        // status
        ram.write_bulk(0, &status);
        // wrap around---cycle bit now needs to be 0
        ram.write_bulk(0, &[0x0]);

        // ring abstraction should parse first TRB of second batch correctly
        check_trb(ring.next_trb(), 0x30, setup);

        // ring abstraction should wrap around and parse second TRB of
        // second batch correctly
        ring.advance();
        check_trb(ring.next_trb(), 0x00, status);

        // no new TRB placed, should return no new TRB
        ring.advance();
        let trb = ring.next_trb();
        assert!(
            trb.is_none(),
            "When no fresh TRB is on the transfer ring, next_trb should return None, instead got: {trb:?}"
        );
    }

    // check if the TRB is as expected. The cycle bit is ignored.
    fn check_trb(trb: Option<RawTrb>, expected_addr: u64, expected_data: RawTrbBuffer) {
        assert!(
            trb.is_some(),
            "expected TRB data retrieved from {expected_data:?}, but next_trb returned None"
        );
        let trb = trb.unwrap();
        assert_eq!(trb.address, expected_addr);

        // zero cycle bits
        let mut actual_data = trb.buffer;
        actual_data[12] &= 0xfe;
        let mut expected_data = expected_data;
        expected_data[12] &= 0xfe;

        assert_eq!(actual_data, expected_data);
    }

    #[test]
    fn get_and_set_dequeue_pointer_with_cycle_bit() {
        // construct memory segment for a ring that can contain TRBs
        let dma = Arc::new(TestBusDevice::new(&[0; TRB_SIZE * 32]));
        let mut ring = LinkedRing::new(dma, THIRD_ADDRESS, true);

        // the dequeue pointer is initialized on some address in the middle
        // with a cycle bit preventing `next_trb()` to see any trbs
        assert_eq!(ring.get_dequeue_pointer(), (THIRD_ADDRESS, true));
        assert_eq!(ring.next_trb(), None);

        // setting the pointer backwards and changing the cycle bit interpretation
        // so we interpret the zero buffer as trb from the current cycle
        ring.set_dequeue_pointer(FIRST_ADDRESS, false);
        assert_eq!(ring.get_dequeue_pointer(), (FIRST_ADDRESS, false));
        check_trb(ring.next_trb(), FIRST_ADDRESS, [0; 16]);

        // with the cycle bit we can see the previously none trb
        ring.set_dequeue_pointer(THIRD_ADDRESS, false);
        assert_eq!(ring.get_dequeue_pointer(), (THIRD_ADDRESS, false));
        check_trb(ring.next_trb(), THIRD_ADDRESS, [0; 16]);
    }

    #[test]
    fn next_trb_advances_dequeue_pointer_over_link_trb() {
        const SEG_1: u64 = 0x000;
        const SEG_2: u64 = 0x100;

        const LINK_TRB_ADDRESS: u64 = 0x30;

        // construct memory segments for a ring that can contain TRBs
        let dma_bus = Arc::new(DynamicBus::new());
        dma_bus
            .add(SEG_1, Arc::new(TestBusDevice::new(&[0; 0x40])))
            .expect("Adding Memory to the DynamicBus should never fail.");
        dma_bus
            .add(SEG_2, Arc::new(TestBusDevice::new(&[0; 0x40])))
            .expect("Adding Memory to the DynamicBus should never fail.");

        let mut ring = LinkedRing::new(dma_bus.clone(), SEG_1 + THIRD_ADDRESS, true);

        // use a link trb to connect the two memory segments
        let link = RawTrbBuilder::new(SEG_1 + LINK_TRB_ADDRESS)
            .with_data_pointer(SEG_2 + FIRST_ADDRESS)
            .with_trb_type(trb_types::LINK)
            .build();

        dma_bus.write_bulk(link.address, &link.buffer);

        // advance to the link trb
        ring.set_dequeue_pointer(SEG_1 + LINK_TRB_ADDRESS, false);

        // the test assumes that `next_trb()` will move the dequeue pointer to the
        // transfer trb it returns and never mention any link trb it skipped over
        let trb = ring.next_trb();
        assert_ne!(
            ring.get_dequeue_pointer(),
            (SEG_1 + LINK_TRB_ADDRESS, false)
        );
        assert_eq!(ring.get_dequeue_pointer(), (SEG_2 + FIRST_ADDRESS, false));
        check_trb(trb, SEG_2 + FIRST_ADDRESS, [0; 16]);

        // jump back to the link trb but use a different cycle bit so we do not see the link trb
        ring.set_dequeue_pointer(SEG_1 + LINK_TRB_ADDRESS, true);

        assert_eq!(ring.next_trb(), None);

        // since we did not encounter a actionable link trb we remained at this address
        assert_eq!(ring.get_dequeue_pointer(), (SEG_1 + LINK_TRB_ADDRESS, true));
    }

    #[test]
    fn encountering_two_consecutive_link_trb_is_supported() {
        const SEG_1: u64 = 0x00;
        const SEG_2: u64 = 0x20;
        const SEG_3: u64 = 0x60;

        // construct memory segments for a ring that can contain TRBs
        let dma_bus = Arc::new(DynamicBus::new());
        dma_bus
            .add(SEG_1, Arc::new(TestBusDevice::new(&[0; 0x10])))
            .expect("Adding Memory to the DynamicBus should never fail.");
        dma_bus
            .add(SEG_2, Arc::new(TestBusDevice::new(&[0; 0x10])))
            .expect("Adding Memory to the DynamicBus should never fail.");
        dma_bus
            .add(SEG_3, Arc::new(TestBusDevice::new(&[0; 0x10])))
            .expect("Adding Memory to the DynamicBus should never fail.");

        let dma = Arc::new(TestBusDevice::new(&[0; TRB_SIZE * 8]));
        let mut ring = LinkedRing::new(dma.clone(), SEG_1, false);

        // place two consecutive link trb td
        let link = RawTrbBuilder::new(SEG_1)
            .with_data_pointer(SEG_2)
            .with_trb_type(trb_types::LINK)
            .build();
        dma.write_bulk(link.address, &link.buffer);

        let link = RawTrbBuilder::new(SEG_2)
            .with_data_pointer(SEG_3)
            .with_trb_type(trb_types::LINK)
            .build();
        dma.write_bulk(link.address, &link.buffer);

        // LinkTrb --> LinkTrb --> zero buffer
        let trb = ring.next_trb();
        assert_eq!(ring.get_dequeue_pointer(), (SEG_3, false));
        check_trb(trb, SEG_3, [0; 16]);
    }
}
