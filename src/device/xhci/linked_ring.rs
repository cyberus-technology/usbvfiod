use tracing::{debug, trace};

use crate::device::{
    bus::BusDeviceRef,
    pci::{
        constants::xhci::rings::TRB_SIZE,
        trb::{zeroed_trb_buffer, LinkTrb, RawTrb, RawTrbBuffer},
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

        let final_buffer = if let Some(link_trb) = LinkTrb::parse(first_trb_buffer) {
            // encountered Link TRB
            // update dequeue pointer.
            self.dequeue_pointer = link_trb.ring_segment_pointer;
            if link_trb.toggle_cycle {
                self.cycle_state = !self.cycle_state;
            }

            // lookup first TRB in the new memory segment
            let second_trb_buffer = self.next_trb_raw()?;
            if let Some(_) = LinkTrb::parse(second_trb_buffer) {
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
            "interpreting transfer TRB at dequeue pointer; cycle state = {}, TRB = {:?}",
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

    pub fn advance(&mut self) {
        // advance to next TRB
        self.dequeue_pointer = self.dequeue_pointer.wrapping_add(TRB_SIZE as u64);
    }

    pub fn set_dequeue_pointer(&mut self, dequeue_pointer: u64, cycle_state: bool) {
        self.dequeue_pointer = dequeue_pointer;
        self.cycle_state = cycle_state;
    }

    pub fn get_dequeue_pointer(&self) -> (u64, bool) {
        (self.dequeue_pointer, self.cycle_state)
    }
}

// #[cfg(test)]
// mod tests {
//     use crate::device::bus::testutils::TestBusDevice;
//     use std::sync::Arc;

//     use super::*;

//     // test summary:
//     //
//     // This test checks the parsing of USB control requests from two and
//     // three TRBs as well as correct handling of wrap around/Link TRBs.
//     //
//     // steps:
//     //
//     // - transfer ring with 5 TRBs
//     // - prepare
//     //   [Setup Stage] [Data Stage] [Status Stage] [non-fresh TRB] [non-fresh TRB]
//     // - request should be parsed from the three TRBs
//     // - prepare
//     //   [Status Stage] [non-fresh TRB] [non-fresh TRB] [Setup Stage] [Link]
//     // - request should be parsed from the two TRBs
//     #[test]
//     fn transfer_ring_retrieve_control_requests() {
//         let setup = [
//             0x11, 0x22, 0x44, 0x33, 0x66, 0x55, 0x88, 0x77, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08,
//             0x00, 0x00,
//         ];
//         let data = [
//             0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0c,
//             0x00, 0x00,
//         ];
//         let status = [
//             0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x10, 0x0, 0x0,
//         ];
//         let link = [
//             0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0x18, 0x0, 0x0,
//         ];

//         // construct memory segment for a ring that can contain 5 TRBs and an endpoint context
//         let ram = Arc::new(TestBusDevice::new(&[0; TRB_SIZE * 5 + 32]));
//         let offset_ep_context = TRB_SIZE as u64 * 5;
//         // setup dequeue pointer and cycle state in the endpoint context
//         // (dequeue pointer is 0, thus only setting cycle bit)
//         ram.write_bulk(offset_ep_context + 8, &[0x1]);
//         let ep = EndpointContext::new(offset_ep_context, ram.clone());
//         let transfer_ring = TransferRing::new(ep, ram.clone());

//         // the ring is still empty
//         let request = transfer_ring.next_request();
//         assert!(
//             request.is_none(),
//             "When no fresh request is on the transfer ring, next_request should return None, instead got: {request:?}"
//         );

//         // place first request
//         // place setup
//         ram.write_bulk(0, &setup);
//         // set cycle bit
//         ram.write_bulk(12, &[0x1]);

//         // place data
//         ram.write_bulk(TRB_SIZE as u64, &data);
//         ram.write_bulk(TRB_SIZE as u64 + 12, &[0x1]);

//         // place status
//         ram.write_bulk(TRB_SIZE as u64 * 2, &status);
//         ram.write_bulk(TRB_SIZE as u64 * 2 + 12, &[0x1]);

//         // ring abstraction should parse correctly
//         let expected = Some(Ok(UsbRequest {
//             address: TRB_SIZE as u64 * 2,
//             request_type: 0x11,
//             request: 0x22,
//             value: 0x3344,
//             index: 0x5566,
//             length: 0x7788,
//             data: Some(0x1122334455667788),
//         }));
//         assert_eq!(transfer_ring.next_request(), expected);

//         // no new command placed, should return no new command
//         let request = transfer_ring.next_request();
//         assert!(
//             request.is_none(),
//             "When no fresh request is on the transfer ring, next_request should return None, instead got: {request:?}"
//         );

//         // place second request (include link TRB because the ring needs to
//         // wrap around)
//         // place setup
//         ram.write_bulk(TRB_SIZE as u64 * 3, &setup);
//         ram.write_bulk(TRB_SIZE as u64 * 3 + 12, &[0x1]);

//         // place link
//         ram.write_bulk(TRB_SIZE as u64 * 4, &link);
//         ram.write_bulk(TRB_SIZE as u64 * 4 + 12, &[0x1]);
//         // set cycle bit without affecting the toggle_cycle bit
//         ram.write_bulk(TRB_SIZE as u64 * 4 + 12, &[0x1 | link[12]]);

//         // place status
//         ram.write_bulk(0, &status);
//         // wrap around---cycle bit now needs to be 0
//         ram.write_bulk(0, &[0x0]);

//         // ring abstraction should parse correctly
//         let expected = Some(Ok(UsbRequest {
//             address: 0,
//             request_type: 0x11,
//             request: 0x22,
//             value: 0x3344,
//             index: 0x5566,
//             length: 0x7788,
//             data: None,
//         }));
//         assert_eq!(transfer_ring.next_request(), expected);

//         // no new command placed, should return no new command
//         let request = transfer_ring.next_request();
//         assert!(
//             request.is_none(),
//             "When no fresh request is on the transfer ring, next_request should return None, instead got: {request:?}"
//         );
//     }
// }
