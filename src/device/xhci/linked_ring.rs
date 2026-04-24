use tracing::trace;

use crate::device::{
    bus::BusDeviceRef,
    pci::constants::xhci::rings::TRB_SIZE,
    xhci::trb::{
        zeroed_trb_buffer, LinkTrb, RawTrb, RawTrbBuffer, TransferTrb, TransferTrbVariant,
    },
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
    /// function might read two TRBs to return a single one.
    pub fn next_trb(&mut self) -> Option<RawTrb> {
        // retrieve TRB at dequeue pointer and return None if there is no fresh
        // TRB
        let first_trb_buffer = self.next_trb_raw()?;

        match TransferTrbVariant::parse(first_trb_buffer) {
            TransferTrbVariant::Normal(data) => {
                trace!("next_trb: {:?}", data);
            }
            TransferTrbVariant::SetupStage(data) => {
                trace!("next_trb: {:?}", data);
            }
            TransferTrbVariant::DataStage(data) => {
                trace!("next_trb: {:?}", data);
            }
            TransferTrbVariant::StatusStage(data) => {
                trace!("next_trb: {:?}", data);
            }
            TransferTrbVariant::EventData(data) => {
                trace!("next_trb: {:?}", data);
            }
            a => {
                trace!("next_trb wildcard: {:?}", a);
            }
        }

        let final_buffer = if let Some(link_trb) = LinkTrb::parse(first_trb_buffer) {
            // encountered Link TRB
            // update dequeue pointer.
            self.dequeue_pointer = link_trb.ring_segment_pointer;
            if link_trb.toggle_cycle {
                self.cycle_state = !self.cycle_state;
            }
            trace!(
                "encountered Link TRB; set dequeue from {:x} to {:x}; toggle_cycle_bit: {}",
                self.dequeue_pointer,
                link_trb.ring_segment_pointer,
                link_trb.toggle_cycle
            );

            // lookup first TRB in the new memory segment
            let second_trb_buffer = self.next_trb_raw()?;
            if LinkTrb::parse(second_trb_buffer).is_some() {
                panic!("Link TRB should not follow directly after another Link TRB");
            }
            second_trb_buffer
        } else {
            first_trb_buffer
        };

        let address = self.dequeue_pointer;
        let trb = RawTrb {
            address,
            buffer: final_buffer,
        };

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
            "interpreting TRB at dequeue pointer {:x}; cycle state = {}, TRB = {:?}",
            self.dequeue_pointer,
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

    pub const fn get_dequeue_pointer(&self) -> (u64, bool) {
        (self.dequeue_pointer, self.cycle_state)
    }
}

#[cfg(test)]
mod tests {
    use crate::device::bus::testutils::TestBusDevice;
    use std::sync::Arc;

    use super::*;

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

        // construct memory segment for a ring that can contain 5 TRBs and an endpoint context
        let ram = Arc::new(TestBusDevice::new(&[0; TRB_SIZE * 5]));
        let mut ring = LinkedRing::new(ram.clone(), 0x0, true);

        // the ring is still empty
        let trb = ring.next_trb();
        assert!(
            trb.is_none(),
            "When no fresh TRB is on the ring, next_trb should return None, instead got: {trb:?}"
        );

        // place three TRBs
        // set cycle bit
        // place setup
        ram.write_bulk(0, &setup);
        ram.write_bulk(12, &[0x1]);

        // place data
        ram.write_bulk(TRB_SIZE as u64, &data);
        ram.write_bulk(TRB_SIZE as u64 + 12, &[0x1]);

        // place status
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
        // place setup
        ram.write_bulk(TRB_SIZE as u64 * 3, &setup);
        ram.write_bulk(TRB_SIZE as u64 * 3 + 12, &[0x1]);

        // place link
        ram.write_bulk(TRB_SIZE as u64 * 4, &link);
        ram.write_bulk(TRB_SIZE as u64 * 4 + 12, &[0x1]);
        // set cycle bit without affecting the toggle_cycle bit
        ram.write_bulk(TRB_SIZE as u64 * 4 + 12, &[0x1 | link[12]]);

        // place status
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
}
