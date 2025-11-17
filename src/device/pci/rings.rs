//! Abstractions of the rings (Event Ring, Command Ring, Transfer Ring) of a
//! USB3 Host (XHCI) controller.
//!
//! The specification is available
//! [here](https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf).

use thiserror::Error;
use tracing::{debug, trace, warn};

use super::{
    device_slots::EndpointContext,
    trb::{CommandTrb, CommandTrbVariant, EventTrb, RawTrbBuffer, TransferTrb, TransferTrbVariant},
    usbrequest::UsbRequest,
};

use crate::device::{
    bus::{BusDeviceRef, Request, RequestSize},
    pci::{
        constants::xhci::{
            operational::crcr,
            rings::{event_ring::segments_table_entry_offsets::*, trb_types, TRB_SIZE},
        },
        trb::zeroed_trb_buffer,
    },
};

/// The Event Ring: A unidirectional means of communication, allowing the XHCI
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
    /// The number of segments currently allocated for the ring.
    ///
    /// This field directly corresponds with the ERSTSZ register in the
    /// XHCI's MMIO region. It defines the maximum valid index for
    /// segment access in the Event Ring Segment Table (valid indices
    /// are 0 to erst_size-1).
    erst_size: u32,
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
            erst_count: 0,
            cycle_state: false,
            erst_size: 0,
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
    pub fn configure(&mut self, erstba: u64) {
        assert_eq!(erstba & 0x3f, 0, "unaligned event ring base address");

        assert!(
            self.erst_size > 0,
            "ERSTSZ must be set before ERSTBA; misconfigured driver"
        );

        self.base_address = erstba;
        self.enqueue_pointer = self.dma_bus.read(Request::new(
            erstba.wrapping_add(SEGMENT_BASE),
            RequestSize::Size8,
        ));
        self.trb_count = self
            .dma_bus
            .read(Request::new(erstba.wrapping_add(SIZE), RequestSize::Size4))
            as u32;
        self.cycle_state = true;

        debug!("event ring segment table is at {:#x}", erstba);
        debug!(
            "initializing event ring enqueue pointer from ERST[0] base: {:#x}",
            self.enqueue_pointer
        );
        debug!(
            "retrieving TRB count of the first event ring segment from the segment table: {}",
            self.trb_count
        );
    }

    pub fn set_erst_size(&mut self, size: u32) {
        //assert!(size >= 1, "erst_size (ERSTSZ) must be >= 1");
        self.erst_size = size;

        if self.erst_count >= self.erst_size {
            self.erst_count = 0;
        }

        trace!("set ERST size (segment count) to {}", self.erst_size);
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

    /// Handle reads to the Event Ring Segment Table Size (ERSTSZ).
    pub const fn read_erst_size(&self) -> u64 {
        self.erst_size as u64
    }

    /// Enqueue a new Event TRB into the Ring.
    ///
    /// # Parameters
    /// - `trb`: the TRB to enqueue.
    ///
    /// # Limitations
    /// The current implementation does not handle ring-full recovery and will panic (`todo!()`) in that case.
    pub fn enqueue(&mut self, trb: &EventTrb) {
        // TODO: Proper handling of full Event Ring
        // According to xHCI §4.9.4, the xHC must:
        //
        // 1. Stop fetching new TRBs from the Transfer and Command Rings.
        // 2. Emit an Event Ring Full Error Event TRB to the Event Ring (if supported).
        // 3. Advance the Event Ring Enqueue Pointer (EREP) accordingly.
        // 4. Wait for software (the host driver) to advance the Event Ring Dequeue Pointer (ERDP),
        //    at which point normal event generation can resume.
        if self.check_event_ring_full() {
            todo!("The Event Ring is full!");
        }

        self.dma_bus
            .write_bulk(self.enqueue_pointer, &trb.to_bytes(self.cycle_state));

        self.trb_count -= 1;

        trace!(
            "enqueued TRB in segment {} (total_segments={}) of event ring at address {:#x}. Space for {} more TRBs left in segment; cycle={}; (TRB: {:?})",
            self.erst_count, self.erst_size,  self.enqueue_pointer, self.trb_count, self.cycle_state, trb
        );

        self.advance_enqueue_pointer();
    }

    /// Advances the enqueue pointer to the next slot in the event ring,
    /// wrapping to the start when the end of the segment is reached.
    fn advance_enqueue_pointer(&mut self) {
        if self.trb_count == 0 {
            self.advance_segment_or_wrap();
        } else {
            self.enqueue_pointer = self.enqueue_pointer.wrapping_add(TRB_SIZE as u64);
        }
    }

    /// Checks whether the Event Ring is full, based on xHCI §4.9.4.
    ///
    /// # Return
    /// - `true` if the Event Ring is full and an Event Ring Full Error Event should be enqueued at the current position.
    /// - `false` if there is at least one more slot available.
    fn check_event_ring_full(&self) -> bool {
        if self.trb_count == 1 {
            let next_seg = (self.erst_count + 1) % self.erst_size;

            let entry_addr = self.base_address.wrapping_add((next_seg as u64) * 16);
            let next_seg_pointer = self.dma_bus.read(Request::new(
                entry_addr.wrapping_add(SEGMENT_BASE),
                RequestSize::Size8,
            ));

            self.dequeue_pointer == next_seg_pointer
        } else {
            self.dequeue_pointer == self.enqueue_pointer.wrapping_add(TRB_SIZE as u64)
        }
    }

    /// Advance to the next segment in the Event Ring Segment Table.
    ///
    /// Increments `erst_count` to move to the next segment. Wraps to segment 0
    /// and flips the producer cycle when the index reaches the end. Updates
    /// `enqueue_pointer` and `trb_count` from the selected ERST entry.
    fn advance_segment_or_wrap(&mut self) {
        self.erst_count += 1;
        let wrapped = self.erst_count == self.erst_size;
        if wrapped {
            self.cycle_state = !self.cycle_state;
            self.erst_count = 0;
        }
        let entry_addr = self
            .base_address
            .wrapping_add((self.erst_count as u64) * 16);
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
                self.erst_size
            );
        } else {
            trace!(
                "advanced to segment {}; base={:#x}, trb_count={}, cycle={}, total_segments={}",
                self.erst_count,
                self.enqueue_pointer,
                self.trb_count,
                self.cycle_state,
                self.erst_size
            );
        }
    }
}

/// The Command Ring: A unidirectional means of communication, allowing the
/// driver to send commands to the XHCI controller.
#[derive(Debug)]
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
                    );
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
    /// correctly, which is the reason why the function might read two TRBs to
    /// return a single one.
    pub fn next_command_trb(&mut self) -> Option<CommandTrb> {
        // retrieve TRB at dequeue pointer and return None if there is no fresh
        // TRB
        let first_trb_buffer = self.next_trb_buffer()?;
        let first_trb = CommandTrbVariant::parse(first_trb_buffer);

        let final_trb = match first_trb {
            CommandTrbVariant::Link(link_data) => {
                // encountered Link TRB
                // update command ring status
                self.dequeue_pointer = link_data.ring_segment_pointer;
                if link_data.toggle_cycle {
                    self.cycle_state = !self.cycle_state;
                }
                // lookup first TRB in the new memory segment
                let second_trb_buffer = self.next_trb_buffer()?;
                let second_trb = CommandTrbVariant::parse(second_trb_buffer);
                if matches!(second_trb, CommandTrbVariant::Link(_)) {
                    panic!("Link TRB should not follow directly after another Link TRB");
                }
                second_trb
            }
            _ => first_trb,
        };

        let address = self.dequeue_pointer;

        // advance to next TRB
        self.dequeue_pointer = self.dequeue_pointer.wrapping_add(TRB_SIZE as u64);

        // return parsed result
        Some(CommandTrb {
            address,
            variant: final_trb,
        })
    }

    /// Try to retrieve a fresh command TRB buffer from the command ring.
    fn next_trb_buffer(&self) -> Option<RawTrbBuffer> {
        // retrieve TRB at current dequeue_pointer
        let mut trb_buffer = zeroed_trb_buffer();
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

        // TRB is fresh; return it
        Some(trb_buffer)
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
    /// This function only returns `TransferTrb`s that are not Link TRBs.
    /// Instead, Link TRBs are handled correctly, which is the reason why the
    /// function might read two TRBs to return a single one.
    pub fn next_transfer_trb(&self) -> Option<TransferTrb> {
        let (mut dequeue_pointer, mut cycle_state) =
            self.endpoint_context.get_dequeue_pointer_and_cycle_state();
        // retrieve TRB at dequeue pointer and return None if there is no fresh
        // TRB
        let first_trb_buffer = self.next_trb_buffer()?;
        let first_trb = TransferTrbVariant::parse(first_trb_buffer);

        let final_trb = match first_trb {
            TransferTrbVariant::Link(link_data) => {
                // encountered Link TRB
                // update transfer ring status
                dequeue_pointer = link_data.ring_segment_pointer;
                if link_data.toggle_cycle {
                    cycle_state = !cycle_state;
                }
                self.endpoint_context
                    .set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);
                // lookup first TRB in the new memory segment
                let second_trb_buffer = self.next_trb_buffer()?;
                let second_trb = TransferTrbVariant::parse(second_trb_buffer);
                if matches!(second_trb, TransferTrbVariant::Link(_)) {
                    panic!("Link TRB should not follow directly after another Link TRB");
                }
                second_trb
            }
            _ => first_trb,
        };

        let address = dequeue_pointer;

        // advance to next TRB
        dequeue_pointer = dequeue_pointer.wrapping_add(TRB_SIZE as u64);
        self.endpoint_context
            .set_dequeue_pointer_and_cycle_state(dequeue_pointer, cycle_state);

        // return parsed result
        Some(TransferTrb {
            address,
            variant: final_trb,
        })
    }

    /// Try to retrieve a new TRB from a transfer ring.
    ///
    /// If there is a fresh TRB at the dequeue pointer, the function tries to
    /// parse the transfer TRB and returns the result. If there is a fresh Link
    /// TRB, this function will return it!
    fn next_trb_buffer(&self) -> Option<RawTrbBuffer> {
        let (dequeue_pointer, cycle_state) =
            self.endpoint_context.get_dequeue_pointer_and_cycle_state();
        // retrieve TRB at current dequeue_pointer
        let mut trb_buffer = zeroed_trb_buffer();
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

        // TRB is fresh; return it
        Some(trb_buffer)
    }

    /// Retrieve the next USB control request from a transfer ring.
    ///
    /// Takes setup+data+status TRBs or setup+status TRBs from transfer ring
    /// and extracts the information into a UsbRequest struct.
    ///
    /// # Limitations
    ///
    /// This function currently assumes that all TRBs are available on the
    /// ring. This assumption should hold true for synchronous handling of
    /// doorbell writes, but once we implement async handling, encountering
    /// partial requests is a valid scenario (and we would have to wait for
    /// the driver to write the missing TRBs).
    pub fn next_request(&self) -> Option<Result<UsbRequest, RequestParseError>> {
        let first_trb = self.next_transfer_trb()?;

        let setup_trb_data = match first_trb.variant {
            TransferTrbVariant::SetupStage(data) => {
                // happy case, we got a Setup Stage TRB
                data
            }
            trb => {
                // got some TRB, but not a Setup Stage
                return Some(Err(RequestParseError::UnexpectedTrbType(
                    vec![trb_types::SETUP_STAGE],
                    trb,
                )));
            }
        };

        let second_trb = self.next_transfer_trb();
        let data_trb_or_address = match second_trb {
            None => {
                // there should follow either Data or Status Stage
                return Some(Err(RequestParseError::MissingTrb));
            }
            Some(TransferTrb {
                address: _,
                variant: TransferTrbVariant::DataStage(data),
            }) => {
                // happy case, we got a Data Stage TRB
                if data.chain {
                    todo!("encountered DataStage with chain bit set");
                }
                Ok(data)
            }
            Some(TransferTrb {
                address,
                variant: TransferTrbVariant::StatusStage,
            }) => {
                // happy case, we skipped Data Stage TRB and already got Status
                // Stage.
                // we indicate the address of the status stage (required for
                // Transfer Event)
                Err(address)
            }
            Some(TransferTrb {
                address: _,
                variant,
            }) => {
                // got some TRB, but neither a Data Stage nor a Status Stage
                return Some(Err(RequestParseError::UnexpectedTrbType(
                    vec![trb_types::DATA_STAGE, trb_types::STATUS_STAGE],
                    variant,
                )));
            }
        };

        let request = match data_trb_or_address {
            Ok(data_trb_data) => {
                // the second TRB was a data stage.
                // We need to retrieve the third TRB and make sure it is a status
                // stage.
                let third_trb = self.next_transfer_trb();
                let address = match third_trb {
                    None => {
                        // there should follow a Status Stage
                        return Some(Err(RequestParseError::MissingTrb));
                    }
                    Some(TransferTrb {
                        address,
                        variant: TransferTrbVariant::StatusStage,
                    }) => {
                        // happy case, we got a Data Stage TRB
                        address
                    }
                    Some(TransferTrb {
                        address: _,
                        variant,
                    }) => {
                        // got some TRB, but not a Status Stage
                        return Some(Err(RequestParseError::UnexpectedTrbType(
                            vec![trb_types::STATUS_STAGE],
                            variant,
                        )));
                    }
                };
                // third TRB was Status Stage.
                // build request with data pointer and return address of third
                // TRB.
                UsbRequest {
                    address,
                    request_type: setup_trb_data.request_type,
                    request: setup_trb_data.request,
                    value: setup_trb_data.value,
                    index: setup_trb_data.index,
                    length: setup_trb_data.length,
                    data: Some(data_trb_data.data_pointer),
                }
            }
            Err(address) => {
                // the second TRB was a status stage.
                // Then, all (two) TRBs were retrieved.
                // build request and use address of second TRB
                UsbRequest {
                    address,
                    request_type: setup_trb_data.request_type,
                    request: setup_trb_data.request,
                    value: setup_trb_data.value,
                    index: setup_trb_data.index,
                    length: setup_trb_data.length,
                    data: None,
                }
            }
        };

        Some(Ok(request))
    }
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum RequestParseError {
    #[error("Encountered unexpected TRB type. Expected type(s) {0:?}, got TRB {1:?}")]
    UnexpectedTrbType(Vec<u8>, TransferTrbVariant),
    #[error("Expected another TRB, but there was none.")]
    MissingTrb,
}

#[cfg(test)]
mod tests {
    use crate::device::bus::testutils::TestBusDevice;
    use crate::device::pci::trb::CompletionCode;
    use std::sync::Arc;

    use super::*;

    fn init_ram_and_ring() -> (Arc<TestBusDevice>, EventRing) {
        let erste = [
            // segment 0
            // segment_base = 0x30
            // trb_count = 3
            0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
            // segment 1
            // segment_base = 0x60
            // trb_count = 1
            0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
            // segment 2
            // segment_base = 0x70
            // trb_count = 2
            0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];

        let ram = Arc::new(TestBusDevice::new(&[0; 0x90]));
        ram.write_bulk(0x0, &erste);
        let mut ring = EventRing::new(ram.clone());
        ring.set_erst_size(3);
        ring.configure(0x0);
        ring.update_dequeue_pointer(
            ring.dma_bus
                .read(Request::new(ring.base_address, RequestSize::Size8)),
        );

        (ram, ring)
    }

    fn dummy_trb() -> EventTrb {
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
    fn event_ring_start_empty_enqueue_fill_then_wraparound_after_dequeue_pointer_move() {
        let (ram, mut ring) = init_ram_and_ring();

        // segment 0
        ring.enqueue(&dummy_trb()); // TRB 1
        ring.enqueue(&dummy_trb()); // TRB 2
        ring.enqueue(&dummy_trb()); // TRB 3

        assert_trb_written(&ram, 0x30, true);
        assert_trb_written(&ram, 0x30 + 16, true);
        assert_trb_written(&ram, 0x30 + 32, true);

        ring.update_dequeue_pointer(0x30 + 32);

        // segment 1
        ring.enqueue(&dummy_trb()); // TRB 1

        assert_trb_written(&ram, 0x60, true);

        ring.update_dequeue_pointer(0x60);

        // segment 2
        ring.enqueue(&dummy_trb()); // TRB 1

        assert_trb_written(&ram, 0x70, true);

        ring.enqueue(&dummy_trb()); // TRB 2 and wraparound
        assert_trb_written(&ram, 0x70 + 16, true);

        ring.enqueue(&dummy_trb()); // write one more TRB after wraparound
        assert_trb_written(&ram, 0x30, false);
    }

    #[test]
    #[should_panic(expected = "Event Ring is full")]
    fn event_ring_panics_on_wraparound_mid_segment_full() {
        let (_ram, mut ring) = init_ram_and_ring();

        // segment 0
        ring.enqueue(&dummy_trb()); // TRB 1
        ring.enqueue(&dummy_trb()); // TRB 2
        ring.enqueue(&dummy_trb()); // TRB 3

        ring.update_dequeue_pointer(0x30 + 16);

        // segment 1
        ring.enqueue(&dummy_trb()); // TRB 1

        // segment 2
        ring.enqueue(&dummy_trb()); // TRB 1
        ring.enqueue(&dummy_trb()); // TRB 2 and wraparound

        // segment 0
        ring.enqueue(&dummy_trb()); // TRB 1

        // ring is full now, the new TRB could not be written
        // and test should panic
        ring.enqueue(&dummy_trb());
    }

    #[test]
    fn event_ring_multiple_wraparound() {
        let (ram, mut ring) = init_ram_and_ring();

        // ring 1
        // segment 0
        ring.enqueue(&dummy_trb()); // TRB 1
        ring.enqueue(&dummy_trb()); // TRB 2
        ring.enqueue(&dummy_trb()); // TRB 3

        // segment 1
        ring.enqueue(&dummy_trb()); // TRB 1

        // segment 2
        ring.enqueue(&dummy_trb()); // TRB 1
        ring.update_dequeue_pointer(0x30 + 16);
        ring.enqueue(&dummy_trb()); // TRB 2 and wraparound

        // check the the last TRB's Cycle State of the ring
        assert_trb_written(&ram, 0x80, true);

        // ring 2
        // segment 0
        ring.update_dequeue_pointer(0x30 + 16 * 5);
        ring.enqueue(&dummy_trb()); // TRB 1
        ring.enqueue(&dummy_trb()); // TRB 2
        ring.enqueue(&dummy_trb()); // TRB 3

        // segment 1
        ring.enqueue(&dummy_trb()); // TRB 1
        ring.update_dequeue_pointer(0x30 + 32);

        // segment 2
        ring.enqueue(&dummy_trb()); // TRB 1
        assert_trb_written(&ram, 0x70, false);
        ring.enqueue(&dummy_trb()); // TRB 2 and wraparound

        // check the the last TRB's Cycle State of the ring
        assert_trb_written(&ram, 0x80, false);

        // ring 3
        // segment 0
        ring.enqueue(&dummy_trb()); // TRB 1
        assert_trb_written(&ram, 0x30, true);
    }

    #[test]
    #[should_panic(expected = "ERSTSZ must be set before ERSTBA")]
    fn configure_requires_erstsz_first() {
        let erste = [
            0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let ram = Arc::new(TestBusDevice::new(&[0; 0x90]));
        ram.write_bulk(0x0, &erste);
        let mut ring = EventRing::new(ram);
        ring.configure(0x0);
        ring.update_dequeue_pointer(
            ring.dma_bus
                .read(Request::new(ring.base_address, RequestSize::Size8)),
        );
    }

    #[test]
    fn event_ring_dynamic_grow_from_1_to_3() {
        let erste = [
            0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x70, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        let ram = Arc::new(TestBusDevice::new(&[0; 0x90]));
        ram.write_bulk(0x0, &erste);
        let mut ring = EventRing::new(ram.clone());
        // set ERSTSZ = 1
        ring.set_erst_size(1);
        ring.configure(0x0);
        ring.update_dequeue_pointer(
            ring.dma_bus
                .read(Request::new(ring.base_address, RequestSize::Size8)),
        );

        // segment 0
        ring.enqueue(&dummy_trb()); // TRB 1
        ring.enqueue(&dummy_trb()); // TRB 2

        ring.update_dequeue_pointer(0x30 + 16);
        // set ERSTSZ to 3
        ring.set_erst_size(3);

        ring.enqueue(&dummy_trb()); // TRB 3
        assert_trb_written(&ram, 0x30 + 32, true);

        // should enter segment 1 without wraparound
        ring.enqueue(&dummy_trb());
        assert_trb_written(&ram, 0x60, true);

        // continue write until the ring is full
        ring.enqueue(&dummy_trb()); // TRB 1 in segment 2
        ring.enqueue(&dummy_trb()); // TRB 2 in segment 2
        assert_trb_written(&ram, 0x70, true);
        assert_trb_written(&ram, 0x70 + 16, true);

        // write one more TRB, it should be wraparound now
        ring.update_dequeue_pointer(0x30 + 32);
        ring.enqueue(&dummy_trb());
        assert_trb_written(&ram, 0x30, false);
    }

    #[test]
    fn event_ring_dynamic_shrink_to_1() {
        let (ram, mut ring) = init_ram_and_ring();

        ring.enqueue(&dummy_trb()); // TRB 1
        ring.enqueue(&dummy_trb()); // TRB 2

        ring.update_dequeue_pointer(0x30 + 16);

        // before write the last TRB to segment 0, shrink ERSTSZ to 1
        ring.set_erst_size(1);

        ring.enqueue(&dummy_trb()); // TRB 3
        assert_trb_written(&ram, 0x50, true);

        ring.update_dequeue_pointer(0x30 + 32);

        // wraparound
        ring.enqueue(&dummy_trb());
        assert_trb_written(&ram, 0x30, false);
    }

    #[test]
    fn event_ring_dynamic_overwrite() {
        let (ram, mut ring) = init_ram_and_ring();

        // segment 0
        ring.enqueue(&dummy_trb()); // TRB 1
        ring.enqueue(&dummy_trb()); // TRB 2

        // modify the segment 1
        let erste_new = [
            0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, //set size of segment 1 to 2
            0x60, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        ram.write_bulk(0x0, &erste_new);
        ring.set_erst_size(2);

        ring.enqueue(&dummy_trb()); // TRB 3 in segment 0
        ring.update_dequeue_pointer(0x30 + 32);

        // new segment 1
        ring.enqueue(&dummy_trb()); // TRB 1
        ring.enqueue(&dummy_trb()); // TRB 2
        assert_trb_written(&ram, 0x60 + 16, true);

        // should be wraparounded
        ring.enqueue(&dummy_trb());
        assert_trb_written(&ram, 0x30, false);
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
        let ram = Arc::new(TestBusDevice::new(&[0; 16 * 4]));
        let mut command_ring = CommandRing::new(ram.clone());
        command_ring.control(0x1);

        // the ring is still empty
        let trb = command_ring.next_command_trb();
        assert!(
            trb.is_none(),
            "When no fresh command is on the command ring, next_command_trb should return None, instead got: {trb:?}"
        );

        // place a noop command in the first TRB slot
        ram.write_bulk(0, &noop_command);
        // set cycle bit
        ram.write_bulk(12, &[0x1]);

        // ring abstraction should parse correctly
        let expected = Some(CommandTrb {
            address: 0,
            variant: CommandTrbVariant::NoOp,
        });
        assert_eq!(command_ring.next_command_trb(), expected);

        // no new command placed, should return no new command
        let trb = command_ring.next_command_trb();
        assert!(
            trb.is_none(),
            "When no fresh command is on the command ring, next_command_trb should return None, instead got: {trb:?}"
        );

        // place two noop commands
        ram.write_bulk(16, &noop_command);
        ram.write_bulk(16 + 12, &[0x1]);
        ram.write_bulk(32, &noop_command);
        ram.write_bulk(32 + 12, &[0x1]);

        // parse first noop
        let expected = Some(CommandTrb {
            address: 16,
            variant: CommandTrbVariant::NoOp,
        });
        assert_eq!(command_ring.next_command_trb(), expected);

        // parse second noop
        let expected = Some(CommandTrb {
            address: 32,
            variant: CommandTrbVariant::NoOp,
        });
        assert_eq!(command_ring.next_command_trb(), expected);

        // no new command placed, should return no new command
        let trb = command_ring.next_command_trb();
        assert!(
            trb.is_none(),
            "When no fresh command is on the command ring, next_command_trb should return None, instead got: {trb:?}"
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
            "When no fresh command is on the command ring, next_command_trb should return None, instead got: {trb:?}"
        );

        // make noop command fresh by toggling the cycle bit
        ram.write_bulk(12, &[0x0]);

        // parse refreshed noop
        let expected = Some(CommandTrb {
            address: 0,
            variant: CommandTrbVariant::NoOp,
        });
        assert_eq!(command_ring.next_command_trb(), expected);
    }

    // test summary:
    //
    // This test checks the parsing of USB control requests from two and
    // three TRBs as well as correct handling of wrap around/Link TRBs.
    //
    // steps:
    //
    // - transfer ring with 5 TRBs
    // - prepare
    //   [Setup Stage] [Data Stage] [Status Stage] [non-fresh TRB] [non-fresh TRB]
    // - request should be parsed from the three TRBs
    // - prepare
    //   [Status Stage] [non-fresh TRB] [non-fresh TRB] [Setup Stage] [Link]
    // - request should be parsed from the two TRBs
    #[test]
    fn transfer_ring_retrieve_control_requests() {
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
        let ram = Arc::new(TestBusDevice::new(&[0; TRB_SIZE * 5 + 32]));
        let offset_ep_context = TRB_SIZE as u64 * 5;
        // setup dequeue pointer and cycle state in the endpoint context
        // (dequeue pointer is 0, thus only setting cycle bit)
        ram.write_bulk(offset_ep_context + 8, &[0x1]);
        let ep = EndpointContext::new(offset_ep_context, ram.clone());
        let transfer_ring = TransferRing::new(ep, ram.clone());

        // the ring is still empty
        let request = transfer_ring.next_request();
        assert!(
            request.is_none(),
            "When no fresh request is on the transfer ring, next_request should return None, instead got: {request:?}"
        );

        // place first request
        // place setup
        ram.write_bulk(0, &setup);
        // set cycle bit
        ram.write_bulk(12, &[0x1]);

        // place data
        ram.write_bulk(TRB_SIZE as u64, &data);
        ram.write_bulk(TRB_SIZE as u64 + 12, &[0x1]);

        // place status
        ram.write_bulk(TRB_SIZE as u64 * 2, &status);
        ram.write_bulk(TRB_SIZE as u64 * 2 + 12, &[0x1]);

        // ring abstraction should parse correctly
        let expected = Some(Ok(UsbRequest {
            address: TRB_SIZE as u64 * 2,
            request_type: 0x11,
            request: 0x22,
            value: 0x3344,
            index: 0x5566,
            length: 0x7788,
            data: Some(0x1122334455667788),
        }));
        assert_eq!(transfer_ring.next_request(), expected);

        // no new command placed, should return no new command
        let request = transfer_ring.next_request();
        assert!(
            request.is_none(),
            "When no fresh request is on the transfer ring, next_request should return None, instead got: {request:?}"
        );

        // place second request (include link TRB because the ring needs to
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

        // ring abstraction should parse correctly
        let expected = Some(Ok(UsbRequest {
            address: 0,
            request_type: 0x11,
            request: 0x22,
            value: 0x3344,
            index: 0x5566,
            length: 0x7788,
            data: None,
        }));
        assert_eq!(transfer_ring.next_request(), expected);

        // no new command placed, should return no new command
        let request = transfer_ring.next_request();
        assert!(
            request.is_none(),
            "When no fresh request is on the transfer ring, next_request should return None, instead got: {request:?}"
        );
    }
}
