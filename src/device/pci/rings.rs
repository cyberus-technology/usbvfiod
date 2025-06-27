//! Abstractions of the rings (Event Ring, Command Ring, Transfer Ring) of a
//! USB3 Host (XHCI) controller.
//!
//! The specification is available
//! [here](https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf).

use tracing::{debug, trace, warn};

use super::{
    device_slots::EndpointContext,
    trb::{CommandTrb, EventTrb, TransferTrb, TrbParseError},
};

use crate::device::{
    bus::{BusDeviceRef, Request, RequestSize},
    pci::constants::xhci::{
        operational::crcr,
        rings::{event_ring::segments_table_entry_offsets::*, TRB_SIZE},
    },
};

/// The Event Ring: A unidirectional means of communication, allowing the XHCI
/// controller to send events to the driver.
///
/// This implementation is a simplified version of the full mechanism specified
/// in the XHCI specification. We assume that the Event Ring Segment Table only
/// holds a single segment.
#[derive(Debug, Clone)]
pub struct EventRing {
    /// Access to guest memory.
    ///
    /// The Event Ring lives in guest memory and we need DMA access to write
    /// events to the ring.
    dma_bus: BusDeviceRef,
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
    /// Create a new Event Ring.
    ///
    /// # Parameters
    ///
    /// - dma_bus: access to guest memory
    pub fn new(dma_bus: BusDeviceRef) -> Self {
        Self {
            dma_bus,
            base_address: 0,
            dequeue_pointer: 0,
            enqueue_pointer: 0,
            trb_count: 0,
            cycle_state: false,
        }
    }

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
    pub fn configure(&mut self, erstba: u64) {
        assert_eq!(erstba & 0x3f, 0, "unaligned event ring base address");

        self.base_address = erstba;
        self.enqueue_pointer = self
            .dma_bus
            .read(Request::new(erstba + BASE_ADDR, RequestSize::Size8));
        self.trb_count = self
            .dma_bus
            .read(Request::new(erstba + SIZE, RequestSize::Size4)) as u32;
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
    pub fn enqueue(&mut self, trb: &EventTrb) {
        if self.check_event_ring_full() {
            todo!();
        }

        self.dma_bus
            .write_bulk(self.enqueue_pointer, &trb.to_bytes(self.cycle_state));

        let enqueue_address = self.enqueue_pointer;

        self.enqueue_pointer += TRB_SIZE as u64;
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

/// The Command Ring: A unidirectional means of communication, allowing the
/// driver to send commands to the XHCI controller.
#[derive(Debug, Clone)]
pub struct CommandRing {
    /// Access to guest memory.
    ///
    /// The Command Ring lives in guest memory and we need DMA access to
    /// retrieve commands from the ring.
    dma_bus: BusDeviceRef,
    /// The controller's running state.
    ///
    /// This flag should be true when the controller is started (R/S bit ==1)
    /// and a write to doorbell 0 happens.
    /// On the other hand, the driver can turn the command ring off
    /// independently of the whole controller by writing the CA (command abort)
    /// or CS (command stop) bits in the CRCR register.
    ///
    /// We currently ignore the value and assume the ring is always running.
    running: bool,
    /// The Command Ring Dequeue Pointer.
    ///
    /// The driver initializes this pointer with a write to the CRCR register.
    /// Subsequently, only the controller advances the pointer as it processes
    /// incoming commands.
    /// The controller reports advancement of the dequeue pointer as part of
    /// the Command Completion Events.
    dequeue_pointer: u64,
    /// The controller's consumer cycle state.
    ///
    /// The controller checks whether the command TRB at the dequeue pointer is
    /// fresh by comparing its cycle state and the cycle bit in the TRB.
    cycle_state: bool,
}

impl CommandRing {
    /// Create a new Command Ring.
    ///
    /// # Parameters
    ///
    /// - dma_bus: access to guest memory
    pub fn new(dma_bus: BusDeviceRef) -> Self {
        Self {
            dma_bus,
            running: false,
            dequeue_pointer: 0,
            cycle_state: false,
        }
    }

    /// Control the Command Ring.
    ///
    /// Call this function when the driver writes to the CRCR register.
    ///
    /// # Parameters
    ///
    /// - `value`: the value the driver wrote to the CRCR register
    ///
    /// # Limitations
    ///
    /// The current implementation of this function is expecting to only be
    /// called for initial setup. Any further writes (e.g., driver stopping the
    /// command ring because a command has timed out) are currently not handled
    /// properly.
    pub fn control(&mut self, value: u64) {
        if self.running {
            match value {
                abort if abort & crcr::CA != 0 => todo!(),
                stop if stop & crcr::CS != 0 => todo!(),
                ignored => {
                    warn!(
                        "received useless write to CRCR while running {:#x}",
                        ignored
                    )
                }
            }
        } else {
            self.dequeue_pointer = value & crcr::DEQUEUE_POINTER_MASK;
            // Update internal consumer cycle state for next TRB fetch.
            self.cycle_state = value & crcr::RCS != 0;
            debug!(
                "configuring command ring with dp={:#x} and cs={}",
                self.dequeue_pointer, self.cycle_state as u8
            );
        }
    }

    /// Returns the current value of the `CRCR` register.
    ///
    /// All bits are zero except the CRR bit, which indicates whether the
    /// command ring is running.
    //
    // Right now, self.running is never changed, so clippy wants the function
    // to be const. Once self.running is actually set, the deny statement can
    // be removed.
    #[allow(clippy::missing_const_for_fn)]
    pub fn status(&self) -> u64 {
        if self.running {
            crcr::CRR
        } else {
            0
        }
    }

    /// Try to retrieve a new command from the command ring.
    ///
    /// This function only returns `CommandTrb`s that represent commands,
    /// i.e., it will not return Link TRBs. Instead, Link TRBs are handled
    /// correctly, which is the reason why the function reads two TRBs to
    /// return a single one.
    pub fn next_command_trb(&mut self) -> Option<(u64, Result<CommandTrb, TrbParseError>)> {
        // retrieve TRB at dequeue pointer and return None if there is no fresh
        // TRB
        let first_trb_result = self.next_trb()?;
        // special handling for Link TRBs
        let trb_result = if let Ok(CommandTrb::Link(link_data)) = first_trb_result {
            // encountered Link TRB
            // update command ring status
            self.dequeue_pointer = link_data.ring_segment_pointer;
            if link_data.toggle_cycle {
                self.cycle_state = !self.cycle_state;
            }
            // lookup first TRB in the new memory segment
            let second_trb_result = self.next_trb()?;
            if let Ok(CommandTrb::Link(_)) = second_trb_result {
                panic!("Link TRB should not follow directly after another Link TRB");
            }
            second_trb_result
        } else {
            first_trb_result
        };

        let trb_address = self.dequeue_pointer;

        // advance to next TRB
        self.dequeue_pointer += TRB_SIZE as u64;

        // return parsed result
        Some((trb_address, trb_result))
    }

    /// Try to retrieve a new command from the command ring.
    ///
    /// If there is a fresh TRB at the dequeue pointer, the function tries to
    /// parse the command TRB and returns the result. If there is a fresh Link
    /// TRB, this function will return it!
    fn next_trb(&self) -> Option<Result<CommandTrb, TrbParseError>> {
        // retrieve TRB at current dequeue_pointer
        let mut trb_buffer = vec![0; TRB_SIZE];
        self.dma_bus
            .read_bulk(self.dequeue_pointer, &mut trb_buffer);

        debug!(
            "interpreting TRB at dequeue pointer; cycle state = {}, TRB = {:?}",
            self.cycle_state as u8, trb_buffer
        );

        // check if the TRB is fresh
        let cycle_bit = trb_buffer[12] & 0x1 != 0;
        if cycle_bit != self.cycle_state {
            // cycle-bit mismatch: no new command TRB available
            return None;
        }

        // TRB is fresh; try to parse
        let trb_result = CommandTrb::try_from(&trb_buffer[..]);

        Some(trb_result)
    }
}

/// Transfer Rings: Unidirectional means of communication, allowing the
/// driver to send requests over the XHCI controller to device endpoints.
///
/// All state lives in guest memory, this struct is merely a wrapper providing
/// convenient methods to access the rings.
#[derive(Debug)]
pub struct TransferRing {
    /// The context of the endpoint that the ring belongs to.
    endpoint_context: EndpointContext,
    /// A reference to guest memory.
    dma_bus: BusDeviceRef,
}

impl TransferRing {
    /// Create a new instance
    ///
    /// # Parameters
    ///
    /// - `endpoint_context`: the endpoint the rings belongs to.
    /// - `dma_bus`: a reference to guest memory.
    pub fn new(endpoint_context: EndpointContext, dma_bus: BusDeviceRef) -> Self {
        Self {
            endpoint_context,
            dma_bus,
        }
    }

    /// Try to retrieve a new TRB from a transfer ring.
    ///
    /// This function only returns `TransferTrb`s that represent commands that
    /// are not Link TRBs. Instead, Link TRBs are handled correctly, which is
    /// the reason why the function might read two TRBs to return a single one.
    pub fn next_transfer_trb(&mut self) -> Option<(u64, Result<TransferTrb, TrbParseError>)> {
        let (mut dequeue_pointer, mut cycle_state) =
            self.endpoint_context.get_dequeue_pointer_and_cycle_state();
        // retrieve TRB at dequeue pointer and return None if there is no fresh
        // TRB
        let first_trb_result = self.next_trb()?;
        // special handling for Link TRBs
        let trb_result = if let Ok(TransferTrb::Link(link_data)) = first_trb_result {
            // encountered Link TRB
            // update transfer ring status
            dequeue_pointer = link_data.ring_segment_pointer;
            if link_data.toggle_cycle {
                cycle_state = !cycle_state;
            }
            self.endpoint_context
                .set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
            // lookup first TRB in the new memory segment
            let second_trb_result = self.next_trb()?;
            if let Ok(TransferTrb::Link(_)) = second_trb_result {
                panic!("Link TRB should not follow directly after another Link TRB");
            }
            second_trb_result
        } else {
            first_trb_result
        };

        let trb_address = dequeue_pointer;

        // advance to next TRB
        dequeue_pointer += TRB_SIZE as u64;
        self.endpoint_context
            .set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);

        // return parsed result
        Some((trb_address, trb_result))
    }

    /// Try to retrieve a new TRB from a transfer ring.
    ///
    /// If there is a fresh TRB at the dequeue pointer, the function tries to
    /// parse the transfer TRB and returns the result. If there is a fresh Link
    /// TRB, this function will return it!
    fn next_trb(&self) -> Option<Result<TransferTrb, TrbParseError>> {
        let (dequeue_pointer, cycle_state) =
            self.endpoint_context.get_dequeue_pointer_and_cycle_state();
        // retrieve TRB at current dequeue_pointer
        let mut trb_buffer = vec![0; TRB_SIZE];
        self.dma_bus.read_bulk(dequeue_pointer, &mut trb_buffer);

        debug!(
            "interpreting transfer TRB at dequeue pointer; cycle state = {}, TRB = {:?}",
            cycle_state as u8, trb_buffer
        );

        // check if the TRB is fresh
        let cycle_bit = trb_buffer[12] & 0x1 != 0;
        if cycle_bit != cycle_state {
            // cycle-bit mismatch: no new TRB available
            return None;
        }

        // TRB is fresh; try to parse
        let trb_result = TransferTrb::try_from(&trb_buffer[..]);

        Some(trb_result)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::device::bus::BusDevice;

    use super::*;

    /// A device that only accepts bulk reads and writes.
    #[derive(Debug)]
    struct BulkOnlyDevice {
        data: Mutex<Vec<u8>>,
    }

    impl BulkOnlyDevice {
        fn new(data: &[u8]) -> Self {
            Self {
                data: Mutex::new(data.to_vec()),
            }
        }
    }

    impl BusDevice for BulkOnlyDevice {
        fn size(&self) -> u64 {
            self.data.lock().unwrap().len().try_into().unwrap()
        }

        fn read(&self, _req: Request) -> u64 {
            panic!("Must not call byte read on this device")
        }

        fn write(&self, _req: Request, _value: u64) {
            panic!("Must not call byte write on this device")
        }

        fn read_bulk(&self, offset: u64, data: &mut [u8]) {
            let offset: usize = offset.try_into().unwrap();
            data.copy_from_slice(&self.data.lock().unwrap()[offset..(offset + data.len())])
        }

        fn write_bulk(&self, offset: u64, data: &[u8]) {
            let offset: usize = offset.try_into().unwrap();
            self.data.lock().unwrap()[offset..(offset + data.len())].copy_from_slice(data)
        }
    }

    #[test]
    fn command_ring_single_segment_traversal() {
        let noop_command = [
            0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x5c, 0x0, 0x0,
        ];
        let link = [
            0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x0, 0x2, 0x18, 0x0, 0x0,
        ];

        // construct memory segment for a ring that can contain 4 TRBs
        let ram = Arc::new(BulkOnlyDevice::new(&[0; 16 * 4]));
        let mut command_ring = CommandRing::new(ram.clone());
        command_ring.control(0x1);

        // the ring is still empty
        let trb = command_ring.next_command_trb();
        assert!(
            trb.is_none(),
            "When no fresh command is on the command ring, next_command_trb should return None, instead got: {:?}",
            trb
        );

        // place a noop command in the first TRB slot
        ram.write_bulk(0, &noop_command);
        // set cycle bit
        ram.write_bulk(12, &[0x1]);

        // ring abstraction should parse correctly
        let trb = command_ring.next_command_trb();
        if let Some((address, Ok(CommandTrb::NoOpCommand))) = trb {
            assert_eq!(0, address, "incorrect address of the next TRB returned");
        } else {
            panic!("Expected to parse a NoOpCommand, instead got: {:?}", trb);
        }

        // no new command placed, should return no new command
        let trb = command_ring.next_command_trb();
        assert!(
            trb.is_none(),
            "When no fresh command is on the command ring, next_command_trb should return None, instead got: {:?}",
            trb
        );

        // place two noop commands
        ram.write_bulk(16, &noop_command);
        ram.write_bulk(16 + 12, &[0x1]);
        ram.write_bulk(32, &noop_command);
        ram.write_bulk(32 + 12, &[0x1]);

        // parse first noop
        let trb = command_ring.next_command_trb();
        if let Some((address, Ok(CommandTrb::NoOpCommand))) = trb {
            assert_eq!(16, address, "incorrect address of the next TRB returned");
        } else {
            panic!("Expected to parse a NoOpCommand, instead got: {:?}", trb);
        }

        // parse second noop
        let trb = command_ring.next_command_trb();
        if let Some((address, Ok(CommandTrb::NoOpCommand))) = trb {
            assert_eq!(32, address, "incorrect address of the next TRB returned");
        } else {
            panic!("Expected to parse a NoOpCommand, instead got: {:?}", trb);
        }

        // no new command placed, should return no new command
        let trb = command_ring.next_command_trb();
        assert!(
            trb.is_none(),
            "When no fresh command is on the command ring, next_command_trb should return None, instead got: {:?}",
            trb
        );

        // place link TRB back to the start of the memory segment
        ram.write_bulk(48, &link);
        // set cycle bit without affecting the toggle_cycle bit
        ram.write_bulk(48 + 12, &[0x1 | link[12]]);

        // we cannot observe it, but the dequeue_pointer should now point to 0 again and the cycle
        // state should have toggled to false. The dequeue_pointer now points at the first written
        // noop command. Cycle bits don't match, so the command ring should not report a new
        // command.
        let trb = command_ring.next_command_trb();
        assert!(
            trb.is_none(),
            "When no fresh command is on the command ring, next_command_trb should return None, instead got: {:?}",
            trb
        );

        // make noop command fresh by toggling the cycle bit
        ram.write_bulk(12, &[0x0]);

        // parse refreshed noop
        let trb = command_ring.next_command_trb();
        if let Some((address, Ok(CommandTrb::NoOpCommand))) = trb {
            assert_eq!(0, address, "incorrect address of the next TRB returned");
        } else {
            panic!("Expected to parse a NoOpCommand, instead got: {:?}", trb);
        }
    }
}
