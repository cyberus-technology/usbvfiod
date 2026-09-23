use std::{
    cmp::min,
    fmt::Debug,
    future::Future,
    mem::{self},
    pin::Pin,
};

use anyhow::anyhow;
use replace_with::replace_with_or_abort;
use tracing::{debug, info, trace, warn};

use crate::device::{
    bus::BusDeviceRef,
    pcap::{self, EndpointPcapMeta},
    xhci::{
        hotplug_endpoint_handle::BaseEndpointHandle,
        interrupter::EventSender,
        real_endpoint_handle::{
            ControlRequestProcessingResult, InTrbProcessingResult, InTrbProcessingStatus,
            OutTrbProcessingResult, RealControlEndpointHandle, RealInEndpointHandle,
            RealOutEndpointHandle,
        },
        trb::{
            CompletionCode, DataStageTrb, EventDataTrb, EventTrb, NoOpTrb, NormalTrb, RawTrb,
            SetupStageTrb, StatusStageTrb, SupportedEndpointTrb, TransferTrbVariant, TrbDmaInfo,
        },
        usbrequest::UsbRequest,
    },
};

pub const MAX_VALUE_U24: u32 = 0xff_ffff;

pub trait EndpointHandle: BaseEndpointHandle {
    type TrbCompletionFuture<'a>: Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()>;
    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_>;
}

/// Possible result cases for processing of a TRB.
///
/// Stall and TransactionError carry an Option to support TD-aggregation-based approaches.
/// - None indicates that the stall/error happened on the current TRB; the endpoint state machine
///   then reports the current dequeue pointer through the endpoint context.
/// - Some((addr, cs)) indicates that the stall/error happened on an earlier TRB but we notice it only
///   now because we aggregated all TRBs of a TD before talking to the real device; the endpoint
///   state machine should wind the dequeue pointer (and associated cycle state) back to this TRB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrbProcessingResult {
    Ok,
    Stall(Option<(u64, bool)>),
    TrbError,
    TransactionError(Option<(u64, bool)>),
    Disconnect,
}

pub type DummyEndpointHandle = ();
impl EndpointHandle for DummyEndpointHandle {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, _trb: RawTrb) -> anyhow::Result<()> {
        panic!("should never call functions of dummy endpoint handle");
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        panic!("should never call functions of dummy endpoint handle");
    }
}

impl BaseEndpointHandle for DummyEndpointHandle {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        panic!("should never call functions of dummy endpoint handle");
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        panic!("should never call functions of dummy endpoint handle");
    }
}

/// Values we will need to handle an incoming Event Data Trb.
///
/// When an Event Data is encountered two additional things are needed:
/// - the EDTLA
/// - the completion code of the previously handled TRB
#[derive(Debug, PartialEq, Eq)]
struct EventDataTrbMetadata {
    /// A 24 Bit sized counter to track already transmitted bytes of the current TD.
    edtla: u32,
    previous_completion_code: CompletionCode,
}

impl EventDataTrbMetadata {
    const fn default() -> Self {
        Self {
            edtla: 0,
            previous_completion_code: CompletionCode::Success,
        }
    }

    /// set self.edlta = 0, does not touch self.previous_completion_code
    const fn zero(&mut self) {
        self.edtla = 0;
    }

    /// The value added according to the specification should be 17 bit
    /// (a TRB's transfer_length field).
    /// This function does not check the input value and instead performs a
    /// wrapping add, followed by a cutoff to fit in 24 bit (EDTLA field size).
    const fn add(&mut self, byte_count: u32) {
        self.edtla = MAX_VALUE_U24 & (self.edtla.wrapping_add(byte_count));
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ControlTransfer {
    /// State for verifying a valid Control Transfer sequence.
    pub state: ControlTransferState,
    /// if direction { IN } else { OUT }
    pub direction: bool,
    /// Might only be partial data for a Control Transfer.
    pub usb_request: UsbRequest,
    /// After a ShortPacket, the subsequent TRB with IOC shall use the same CompletionCode (encoded in state/leftover hardware_data) and transfer_length value.
    pub residual_length: u32,
    event_data_metadata: EventDataTrbMetadata,
}

impl ControlTransfer {
    const fn new(direction: bool, usb_request: UsbRequest) -> Self {
        Self {
            state: ControlTransferState::ExpectSetupStageTrb,
            direction,
            usb_request,
            residual_length: 0,
            event_data_metadata: EventDataTrbMetadata::default(),
        }
    }
}

/// Track how far we are with parsing the Control Transfer (chain of TRB).
///
/// ```mermaid
/// graph TD;
///
///     expect_setup_stage_trb((expect_setup_stage_trb))
///     maybe_data((maybe_data))
///     more_data((more_data))
///     expect_status_stage_trb((expect_status_stage_trb))
///     expect_event_data_as_final_trb((expect_event_data_as_final_trb))
///
///     expect_setup_stage_trb--(received setup_stage_trb)-->maybe_data
///     expect_setup_stage_trb--(any other trb)-->expect_setup_stage_trb
///
///     maybe_data--(received setup_stage_trb)-->maybe_data
///     maybe_data--(status_stage, with chain)-->expect_event_data_as_final_trb
///     maybe_data--(status_stage, no chain or any other trb)-->expect_setup_stage_trb
///     maybe_data--(data_stage, no chain)-->expect_status_stage_trb
///     maybe_data--(data_stage, with chain)-->more_data
///
///     more_data--(received setup_stage_trb)-->maybe_data
///     more_data--(any other trb)-->expect_setup_stage_trb
///     more_data--(normal or event_data, with chain)-->more_data
///     more_data--(normal or event_data, no chain)-->expect_status_stage_trb
///
///     expect_status_stage_trb--(received setup_stage)-->maybe_data
///     expect_status_stage_trb--(status_stage, with chain)-->expect_event_data_as_final_trb
///     expect_status_stage_trb--(status_stage, no chain or any other trb)-->expect_setup_stage_trb
///
///     expect_event_data_as_final_trb--(received setup_stage)-->maybe_data
///     expect_event_data_as_final_trb--(event_data, no chain or any other trb)-->expect_setup_stage_trb
/// ```
#[derive(Debug, PartialEq, Eq)]
pub enum ControlTransferState {
    /// Nothing happened yet. Awaiting a Setup Stage Trb and dropping any other
    /// Trb (they will not reach the hardware device).
    ExpectSetupStageTrb,
    /// Either collect data if a Data Stage Trb is received or skip the Data
    /// Stage TD altogether if a Status Stage Trb is received.
    MaybeDataStageTrb,
    MoreData,
    /// Finished processing the Data Stage if there was one.
    ExpectStatusStageTrb,
    /// Status Stage TRB had a chain bit and there will be exactly one
    /// Event Data Trb to finish the Control Transfer.
    ExpectFinalEventDataTrb,
}

#[derive(Debug, Clone)]
pub enum SupportedControlEndpointTrb {
    SetupStage(SetupStageTrb),
    DataStage(DataStageTrb),
    StatusStage(StatusStageTrb),
    Normal(NormalTrb),
    EventData(EventDataTrb),
}

impl TryFrom<TransferTrbVariant> for SupportedControlEndpointTrb {
    type Error = TransferTrbVariant;

    fn try_from(trb: TransferTrbVariant) -> Result<Self, Self::Error> {
        match trb {
            TransferTrbVariant::SetupStage(t) => Ok(Self::SetupStage(t)),
            TransferTrbVariant::DataStage(t) => Ok(Self::DataStage(t)),
            TransferTrbVariant::StatusStage(t) => Ok(Self::StatusStage(t)),
            TransferTrbVariant::Normal(t) => Ok(Self::Normal(t)),
            TransferTrbVariant::EventData(t) => Ok(Self::EventData(t)),
            variant => Err(variant),
        }
    }
}

impl From<SupportedControlEndpointTrb> for TransferTrbVariant {
    fn from(trb: SupportedControlEndpointTrb) -> Self {
        match trb {
            SupportedControlEndpointTrb::SetupStage(t) => Self::SetupStage(t),
            SupportedControlEndpointTrb::DataStage(t) => Self::DataStage(t),
            SupportedControlEndpointTrb::StatusStage(t) => Self::StatusStage(t),
            SupportedControlEndpointTrb::Normal(t) => Self::Normal(t),
            SupportedControlEndpointTrb::EventData(t) => Self::EventData(t),
        }
    }
}

#[derive(Debug)]
pub struct ControlEndpointHandle<RCEH: RealControlEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    pcap_meta: EndpointPcapMeta,
    real_ep: RCEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    /// Aggregating a chain of TRB received from system software. Used for the current architecture with the submit() and next_completion() loop.
    submission_state: ControlSubmission,
    /// Tracking the Control Transfer information without perceiving individual TRB.
    transfer_state: ControlTransfer,
}

impl<RCEH: RealControlEndpointHandle> ControlEndpointHandle<RCEH> {
    pub fn new(
        slot_id: u8,
        endpoint_id: u8,
        pcap_meta: EndpointPcapMeta,
        real_ep: RCEH,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) -> Self {
        Self {
            slot_id,
            endpoint_id,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
            submission_state: ControlSubmission::default(),
            transfer_state: ControlTransfer::new(false, UsbRequest::default()),
        }
    }

    /// Overwrite any previous Control Transfer state. Resets the tracked chain
    /// and drops the aggregated TRBs.
    fn instantiate_setup_trb(&mut self, addr: u64, trb: &SetupStageTrb) {
        let usb_request = UsbRequest {
            address: addr,
            request_type: trb.request_type,
            request: trb.request,
            value: trb.value,
            index: trb.index,
            length: 0, // Aggregate all Data Stage TD and do not use the unreliable wLength field.
            data_pointer: None,
            data: vec![],
        };

        let direction = trb.request_type & 0x80 != 0;
        let new_transfer_state = ControlTransfer::new(direction, usb_request);

        replace_with_or_abort(&mut self.transfer_state, |_| new_transfer_state);
    }

    /// for Control In
    fn collect_transfer_length<T: TrbDmaInfo>(&mut self, trb: &T) {
        let transfer_length = if trb.has_immediate_data() {
            min(8, trb.transfer_length())
        } else {
            trb.transfer_length()
        };

        self.transfer_state.usb_request.length = self
            .transfer_state
            .usb_request
            .length
            .wrapping_add(transfer_length);
    }

    /// for Control Out
    fn collect_transfer_data<T: TrbDmaInfo>(&mut self, trb: &T) {
        if trb.has_immediate_data() {
            // Only event data should follow when immediate data is used here
            // but we do not check for that and allow multiple immediate data
            // TRB in the data stage TD.

            let length = min(8, trb.transfer_length());
            self.transfer_state
                .usb_request
                .data
                .append(&mut trb.data_pointer().to_le_bytes()[..length as usize].to_vec());
        } else {
            let mut data_slice = vec![0u8; trb.transfer_length() as usize];
            self.dma_bus.read_bulk(trb.data_pointer(), &mut data_slice);

            self.transfer_state.usb_request.data.append(&mut data_slice);
        }
    }

    /// Send an EventTrb with CompletionCode::Success for all currently
    /// collected TRB. Then removes all stored TRB.
    fn clean_current_trbs(&mut self) -> anyhow::Result<()> {
        info!("We report success for the so far collected TRB of the Control Transfer.");
        for trb in &self.submission_state.trbs {
            let interrupt_on_completion = match &trb.variant {
                SupportedControlEndpointTrb::SetupStage(setup) => setup.interrupt_on_completion,
                SupportedControlEndpointTrb::DataStage(data) => data.interrupt_on_completion,
                SupportedControlEndpointTrb::Normal(normal) => normal.interrupt_on_completion,
                SupportedControlEndpointTrb::StatusStage(status) => status.interrupt_on_completion,
                SupportedControlEndpointTrb::EventData(event_data) => {
                    event_data.interrupt_on_completion
                }
            };
            if interrupt_on_completion {
                self.transfer_event_success(trb.addr)?;
            }
        }

        self.submission_state.trbs.clear();

        Ok(())
    }

    /// Send a basic Transfer Event TRB with trb_transfer_length = 0 and CompletionCode::Success.
    fn transfer_event_success(&self, address: u64) -> anyhow::Result<()> {
        let event = EventTrb::new_transfer_event_trb(
            address,
            0,
            CompletionCode::Success,
            false,
            self.endpoint_id,
            self.slot_id,
        );
        self.event_sender.send(event)
    }

    /// Copy the full trb.transfer_length from hardware_data to guest memory.
    fn realize_full_slice<T: TrbDmaInfo>(&self, trb: &T, hardware_data: &mut Vec<u8>) {
        let byte_slice: Vec<u8> = hardware_data
            .drain(0..trb.transfer_length() as usize)
            .collect();
        self.dma_bus.write_bulk(trb.data_pointer(), &byte_slice);
    }

    /// Copy as much hardware_data to guest memory as there is. Do not touch the
    /// remaining memory segments for which there is no more data.
    ///
    /// The Transfer Event TRB is then supposed to tell system software how many
    /// bytes were written. This is not done in this Function.
    fn realize_short_slice<T: TrbDmaInfo>(&self, trb: &T, hardware_data: &mut Vec<u8>) {
        let len = hardware_data.len();
        let byte_slice: Vec<u8> = hardware_data.drain(0..len).collect();
        self.dma_bus.write_bulk(trb.data_pointer(), &byte_slice);
    }

    /// Returns the count of not transferred bytes of this slice (i.e. "This TRB's response is short by <u32> bytes.")
    fn copy_slice_to_guest<T: TrbDmaInfo>(
        &mut self,
        trb: &T,
        hardware_data: &mut Vec<u8>,
    ) -> anyhow::Result<(CompletionCode, u32)> {
        assert!(
            !hardware_data.is_empty(),
            "internal error: called a dma operation without data"
        );

        if hardware_data.len() < trb.transfer_length() as usize {
            let residual_length = trb.transfer_length() - hardware_data.len() as u32;
            self.transfer_state.residual_length = residual_length;
            self.realize_short_slice(trb, hardware_data);
            Ok((CompletionCode::ShortPacket, residual_length))
        } else {
            self.realize_full_slice(trb, hardware_data);
            Ok((CompletionCode::Success, 0))
        }
    }

    /// if hardware_data.is_some() { IN } else { OUT }
    fn realize_control_chain(&mut self, hardware_data: &mut Option<Vec<u8>>) -> anyhow::Result<()> {
        debug!("realize_control_chain with data: {:?}", hardware_data);

        for trb in &self.submission_state.trbs.clone() {
            match &trb.variant {
                SupportedControlEndpointTrb::SetupStage(setup) => {
                    // beginning of a TD
                    self.transfer_state.event_data_metadata.zero();

                    if setup.interrupt_on_completion {
                        self.transfer_event_success(trb.addr)?;
                    }
                    self.transfer_state
                        .event_data_metadata
                        .previous_completion_code = CompletionCode::Success;
                }
                SupportedControlEndpointTrb::DataStage(data) => {
                    // beginning of a TD
                    self.transfer_state.event_data_metadata.zero();

                    let (completion_code, residual_length) =
                        if let Some(hardware_data) = hardware_data {
                            if hardware_data.is_empty() {
                                // Subsequent after a short packet we skip slicing while still doing events.
                                (
                                    CompletionCode::ShortPacket,
                                    self.transfer_state.residual_length,
                                )
                            } else {
                                let (completion_code, residual_bytes) =
                                    self.copy_slice_to_guest(data, hardware_data)?;
                                self.transfer_state
                                    .event_data_metadata
                                    .add(data.transfer_length - residual_bytes);
                                (completion_code, residual_bytes)
                            }
                        } else {
                            self.transfer_state
                                .event_data_metadata
                                .add(data.transfer_length);
                            (CompletionCode::Success, 0)
                        };

                    if (completion_code == CompletionCode::ShortPacket && data.interrupt_on_short)
                        || data.interrupt_on_completion
                    {
                        let event = EventTrb::new_transfer_event_trb(
                            trb.addr,
                            residual_length,
                            completion_code,
                            false,
                            self.endpoint_id,
                            self.slot_id,
                        );
                        self.event_sender.send(event)?;
                    }
                    self.transfer_state
                        .event_data_metadata
                        .previous_completion_code = completion_code;
                }
                SupportedControlEndpointTrb::Normal(normal) => {
                    let (completion_code, residual_length) =
                        if let Some(hardware_data) = hardware_data {
                            if hardware_data.is_empty() {
                                // Subsequent after a short packet we skip slicing while still doing events.
                                (
                                    CompletionCode::ShortPacket,
                                    self.transfer_state.residual_length,
                                )
                            } else {
                                let (completion_code, residual_bytes) =
                                    self.copy_slice_to_guest(normal, hardware_data)?;
                                self.transfer_state
                                    .event_data_metadata
                                    .add(normal.transfer_length - residual_bytes);
                                (completion_code, residual_bytes)
                            }
                        } else {
                            self.transfer_state
                                .event_data_metadata
                                .add(normal.transfer_length);
                            (CompletionCode::Success, 0)
                        };

                    if (completion_code == CompletionCode::ShortPacket && normal.interrupt_on_short)
                        || normal.interrupt_on_completion
                    {
                        let event = EventTrb::new_transfer_event_trb(
                            trb.addr,
                            residual_length,
                            completion_code,
                            false,
                            self.endpoint_id,
                            self.slot_id,
                        );
                        self.event_sender.send(event)?;
                    }
                    self.transfer_state
                        .event_data_metadata
                        .previous_completion_code = completion_code;
                }
                SupportedControlEndpointTrb::StatusStage(status) => {
                    // beginning of a TD
                    self.transfer_state.event_data_metadata.zero();

                    if status.interrupt_on_completion {
                        self.transfer_event_success(trb.addr)?;
                    }
                    self.transfer_state
                        .event_data_metadata
                        .previous_completion_code = CompletionCode::Success;
                }
                SupportedControlEndpointTrb::EventData(event) => {
                    if event.interrupt_on_completion {
                        let event = EventTrb::new_transfer_event_trb(
                            event.event_data,
                            self.transfer_state.event_data_metadata.edtla,
                            self.transfer_state
                                .event_data_metadata
                                .previous_completion_code,
                            true,
                            self.endpoint_id,
                            self.slot_id,
                        );
                        self.event_sender.send(event)?;
                    }
                    self.transfer_state.event_data_metadata.zero();
                }
            }
        }

        Ok(())
    }
}

/// Track communication between us and the host hardware.
#[derive(Debug, Default, Clone)]
struct ControlSubmission {
    state: ControlSubmissionState,
    trbs: Vec<SupportedEndpointTrb<SupportedControlEndpointTrb>>,
}

#[derive(Debug, Default, Clone)]
enum ControlSubmissionState {
    #[default]
    NoTrbSubmitted,
    /// Collect a Control Transfer: Setup TD, Data TD and Status TD.
    /// Find any errors the driver could make.
    CollectingTd,
    ParserError(u64),
    UnexpectedTrb(u64, TransferTrbVariant),
    /// A valid chain has been received and a hardware request is submitted.
    /// Find any errors the USB device could make.
    AwaitingControlRequest,
}

impl<RCEH: RealControlEndpointHandle> EndpointHandle for ControlEndpointHandle<RCEH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        // Verify the TRB itself is good for this Endpoint.
        let supported_trb = match SupportedEndpointTrb::<SupportedControlEndpointTrb>::new(
            trb.address,
            trb.buffer,
        ) {
            Ok(supported_trb) => supported_trb,
            Err(transfer_trb) => {
                if let TransferTrbVariant::Unrecognized(_, _) = &transfer_trb {
                    info!(
                        "Failed to parse Transfer Trb on Control Endpoint. slot {}",
                        self.slot_id
                    );
                    self.submission_state.state = ControlSubmissionState::ParserError(trb.address);
                } else {
                    warn!(
                        "Encountered unsupported TRB on Control Endpoint (slot {}): {transfer_trb:?}",
                        self.slot_id
                    );

                    self.submission_state.state =
                        ControlSubmissionState::UnexpectedTrb(trb.address, transfer_trb);
                }
                return Ok(());
            }
        };

        trace!("Control Endpoint got a supported TRB: {:?}", supported_trb);

        // Verify the chain is building a valid Control Transfer.
        match &self.transfer_state.state {
            ControlTransferState::ExpectSetupStageTrb => match &supported_trb.variant {
                SupportedControlEndpointTrb::SetupStage(trb) => {
                    self.submission_state.trbs.clear();

                    self.instantiate_setup_trb(supported_trb.addr, trb);
                    self.submission_state.trbs.push(supported_trb);
                    self.transfer_state.state = ControlTransferState::MaybeDataStageTrb;
                    self.submission_state.state = ControlSubmissionState::CollectingTd;
                }
                _ => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        supported_trb
                    );

                    self.submission_state.state = ControlSubmissionState::UnexpectedTrb(
                        supported_trb.addr,
                        supported_trb.variant.into(),
                    );
                }
            },
            ControlTransferState::MaybeDataStageTrb => match &supported_trb.variant {
                SupportedControlEndpointTrb::SetupStage(setup) => {
                    info!(
                            "received Setup Stage TRB, abort ongoing control transfer in ControlTransferState::MaybeDataStageTrb in favour of this new one"
                        );

                    self.clean_current_trbs()?;

                    self.instantiate_setup_trb(supported_trb.addr, setup);
                    self.submission_state.trbs.push(supported_trb);
                    self.transfer_state.state = ControlTransferState::MaybeDataStageTrb;
                    self.submission_state.state = ControlSubmissionState::CollectingTd;
                }
                SupportedControlEndpointTrb::DataStage(data) => {
                    if data.chain {
                        self.transfer_state.state = ControlTransferState::MoreData;
                    } else {
                        self.transfer_state.state = ControlTransferState::ExpectStatusStageTrb;
                    }

                    if self.transfer_state.direction {
                        self.collect_transfer_length(data);
                    } else {
                        self.collect_transfer_data(data);
                    }

                    self.submission_state.trbs.push(supported_trb);
                    self.submission_state.state = ControlSubmissionState::CollectingTd;
                }
                SupportedControlEndpointTrb::StatusStage(status) => {
                    if status.chain {
                        self.transfer_state.state = ControlTransferState::ExpectFinalEventDataTrb;
                        self.submission_state.state = ControlSubmissionState::CollectingTd;
                    } else {
                        let usb_request = &self.transfer_state.usb_request;
                        pcap::control_submission(self.pcap_meta, usb_request);
                        self.real_ep.submit_control_request(usb_request.clone())?;
                        self.submission_state.state =
                            ControlSubmissionState::AwaitingControlRequest;

                        self.transfer_state.state = ControlTransferState::ExpectSetupStageTrb;
                    }

                    self.submission_state.trbs.push(supported_trb);
                }
                _ => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        supported_trb
                    );

                    self.transfer_state.state = ControlTransferState::ExpectSetupStageTrb;

                    self.submission_state.state = ControlSubmissionState::UnexpectedTrb(
                        supported_trb.addr,
                        supported_trb.variant.into(),
                    );
                }
            },
            ControlTransferState::MoreData => match &supported_trb.variant {
                SupportedControlEndpointTrb::SetupStage(trb) => {
                    info!(
                            "received Setup Stage TRB, abort ongoing control transfer in ControlTransferState::MoreData in favour of this new one"
                        );

                    self.clean_current_trbs()?;

                    self.instantiate_setup_trb(supported_trb.addr, trb);
                    self.submission_state.trbs.push(supported_trb);
                    self.transfer_state.state = ControlTransferState::MaybeDataStageTrb;
                    self.submission_state.state = ControlSubmissionState::CollectingTd;
                }
                SupportedControlEndpointTrb::Normal(normal) => {
                    if normal.chain {
                        self.transfer_state.state = ControlTransferState::MoreData;
                    } else {
                        self.transfer_state.state = ControlTransferState::ExpectStatusStageTrb;
                    }

                    if self.transfer_state.direction {
                        self.collect_transfer_length(normal);
                    } else {
                        self.collect_transfer_data(normal);
                    }

                    self.submission_state.trbs.push(supported_trb);
                    self.submission_state.state = ControlSubmissionState::CollectingTd;
                }
                SupportedControlEndpointTrb::EventData(event) => {
                    if event.chain {
                        self.transfer_state.state = ControlTransferState::MoreData;
                    } else {
                        self.transfer_state.state = ControlTransferState::ExpectStatusStageTrb;
                    }

                    self.submission_state.trbs.push(supported_trb);
                    self.submission_state.state = ControlSubmissionState::CollectingTd;
                }
                _ => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        supported_trb
                    );

                    self.transfer_state.state = ControlTransferState::ExpectSetupStageTrb;
                    self.submission_state.state = ControlSubmissionState::UnexpectedTrb(
                        supported_trb.addr,
                        supported_trb.variant.into(),
                    );
                }
            },

            ControlTransferState::ExpectStatusStageTrb => match &supported_trb.variant {
                SupportedControlEndpointTrb::SetupStage(setup) => {
                    info!(
                        "received Setup Stage TRB, abort ongoing control transfer in ControlTransferState::ExpectStatusStageTrb in favour of this new one"
                    );

                    self.clean_current_trbs()?;

                    self.instantiate_setup_trb(supported_trb.addr, setup);
                    self.submission_state.trbs.push(supported_trb);
                    self.transfer_state.state = ControlTransferState::MaybeDataStageTrb;
                    self.submission_state.state = ControlSubmissionState::CollectingTd;
                }
                SupportedControlEndpointTrb::StatusStage(status) => {
                    if status.chain {
                        self.transfer_state.state = ControlTransferState::ExpectFinalEventDataTrb;
                        self.submission_state.state = ControlSubmissionState::CollectingTd;
                    } else {
                        let usb_request = &self.transfer_state.usb_request;
                        pcap::control_submission(self.pcap_meta, usb_request);
                        self.real_ep.submit_control_request(usb_request.clone())?;
                        self.submission_state.state =
                            ControlSubmissionState::AwaitingControlRequest;

                        self.transfer_state.state = ControlTransferState::ExpectSetupStageTrb;
                    }

                    self.submission_state.trbs.push(supported_trb);
                }
                _ => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        supported_trb
                    );

                    self.transfer_state.state = ControlTransferState::ExpectSetupStageTrb;
                    self.submission_state.state = ControlSubmissionState::UnexpectedTrb(
                        supported_trb.addr,
                        supported_trb.variant.into(),
                    );
                }
            },
            ControlTransferState::ExpectFinalEventDataTrb => match &supported_trb.variant {
                SupportedControlEndpointTrb::SetupStage(setup) => {
                    info!(
                        "received Setup Stage TRB, abort ongoing control transfer in ControlTransferState::ExpectStatusStageTrb in favour of this new one"
                    );

                    self.clean_current_trbs()?;

                    self.instantiate_setup_trb(supported_trb.addr, setup);
                    self.submission_state.trbs.push(supported_trb);
                    self.transfer_state.state = ControlTransferState::MaybeDataStageTrb;
                    self.submission_state.state = ControlSubmissionState::CollectingTd;
                }
                SupportedControlEndpointTrb::EventData(_event) => {
                    let usb_request = &self.transfer_state.usb_request;
                    pcap::control_submission(self.pcap_meta, usb_request);
                    self.real_ep.submit_control_request(usb_request.clone())?;

                    self.submission_state.trbs.push(supported_trb);
                    self.transfer_state.state = ControlTransferState::ExpectSetupStageTrb;
                    self.submission_state.state = ControlSubmissionState::AwaitingControlRequest;
                }
                _ => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        supported_trb
                    );

                    self.transfer_state.state = ControlTransferState::ExpectSetupStageTrb;
                    self.submission_state.state = ControlSubmissionState::UnexpectedTrb(
                        supported_trb.addr,
                        TransferTrbVariant::from(supported_trb.variant),
                    );
                }
            },
        }

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let result = match &self.submission_state.state {
                ControlSubmissionState::NoTrbSubmitted => {
                    unreachable!("internal error: Always set a different ControlSubmissionState in submit_trb().")
                }
                ControlSubmissionState::CollectingTd => TrbProcessingResult::Ok,
                ControlSubmissionState::ParserError(address) => {
                    pcap::trb_error(self.pcap_meta, *address);

                    // Construct the event for the bad TRB first because of the borrow checker.
                    let event = EventTrb::new_transfer_event_trb(
                        *address,
                        0,
                        CompletionCode::TrbError,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );

                    // Backtrack to send healthy events and drop all collected trbs until...
                    self.clean_current_trbs()?;

                    // ...we hit the bad one and have to report the event for it.
                    self.event_sender.send(event)?;

                    TrbProcessingResult::TrbError
                }
                ControlSubmissionState::UnexpectedTrb(address, variant) => {
                    warn!("unexpected trb from address: {} as: {:?}", address, variant);

                    pcap::trb_error(self.pcap_meta, *address);

                    // Construct the event for the bad TRB first because of the borrow checker.
                    let event = EventTrb::new_transfer_event_trb(
                        *address,
                        0,
                        CompletionCode::TrbError,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );

                    // Backtrack to send healthy events and drop all collected trbs until...
                    self.clean_current_trbs()?;

                    // ...we hit the bad one and have to report the event for it.
                    self.event_sender.send(event)?;

                    self.transfer_state.state = ControlTransferState::ExpectSetupStageTrb;

                    TrbProcessingResult::TrbError
                }
                ControlSubmissionState::AwaitingControlRequest => {
                    let processing_result = self.real_ep.next_completion().await?;

                    match processing_result {
                        ControlRequestProcessingResult::SuccessfulControlIn(hardware_data) => {
                            let usb_request = &self.transfer_state.usb_request;
                            pcap::control_completion_in(
                                self.pcap_meta,
                                usb_request.address,
                                &hardware_data,
                            );

                            self.realize_control_chain(&mut Some(hardware_data))?;

                            TrbProcessingResult::Ok
                        }
                        ControlRequestProcessingResult::SuccessfulControlOut => {
                            let usb_request = &self.transfer_state.usb_request;
                            pcap::control_completion_out(
                                self.pcap_meta,
                                usb_request.address,
                                usb_request.length,
                            );

                            self.realize_control_chain(&mut None)?;

                            TrbProcessingResult::Ok
                        }
                        processing_error => {
                            let usb_request = &self.transfer_state.usb_request;
                            if self.transfer_state.direction {
                                pcap::control_in_error(
                                    self.pcap_meta,
                                    usb_request,
                                    &processing_error,
                                );
                            } else {
                                pcap::control_out_error(
                                    self.pcap_meta,
                                    usb_request,
                                    &processing_error,
                                );
                            }
                            self.handle_processing_error(processing_error, usb_request.address)?
                        }
                    }
                }
            };
            self.submission_state.state = ControlSubmissionState::NoTrbSubmitted;

            Ok(result)
        })
    }
}

impl<RCEH: RealControlEndpointHandle> BaseEndpointHandle for ControlEndpointHandle<RCEH> {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.cancel().await })
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.clear_halt().await })
    }
}

impl<RCEH: RealControlEndpointHandle> ControlEndpointHandle<RCEH> {
    fn handle_processing_error(
        &self,
        error: ControlRequestProcessingResult,
        request_address: u64,
    ) -> anyhow::Result<TrbProcessingResult> {
        let mapped = match error {
            ControlRequestProcessingResult::Disconnect => {
                // send transaction error event to driver
                // forward disconnect result, so that the hotplugendpointhandle can handle
                let event = EventTrb::new_transfer_event_trb(
                    request_address,
                    0,
                    CompletionCode::UsbTransactionError,
                    false,
                    self.endpoint_id,
                    self.slot_id,
                );
                self.event_sender.send(event)?;
                TrbProcessingResult::Disconnect
            }
            ControlRequestProcessingResult::Stall => {
                let event = EventTrb::new_transfer_event_trb(
                    request_address,
                    0,
                    CompletionCode::StallError,
                    false,
                    self.endpoint_id,
                    self.slot_id,
                );
                self.event_sender.send(event)?;
                TrbProcessingResult::Stall(None)
            }
            ControlRequestProcessingResult::TransactionError => {
                let event = EventTrb::new_transfer_event_trb(
                    request_address,
                    0,
                    CompletionCode::UsbTransactionError,
                    false,
                    self.endpoint_id,
                    self.slot_id,
                );
                self.event_sender.send(event)?;
                TrbProcessingResult::TransactionError(None)
            }
            ControlRequestProcessingResult::SuccessfulControlIn(_) => {
                unreachable!(
                    "internal error: Don't try processing an error with a successful ControlRequestProcessingResult."
                )
            }
            ControlRequestProcessingResult::SuccessfulControlOut => {
                unreachable!(
                    "internal error: Don't try processing an error with a successful ControlRequestProcessingResult."
                )
            }
        };
        Ok(mapped)
    }
}

fn handle_event_data_trb_normal_ep(
    event_data_trb: &EventDataTrb,
    event_meta: &mut EventDataTrbMetadata,
    endpoint_id: u8,
    slot_id: u8,
    event_sender: &EventSender,
) -> anyhow::Result<()> {
    trace!("EventData TRB on Normal Ep");

    if event_data_trb.interrupt_on_completion {
        let event = EventTrb::new_transfer_event_trb(
            event_data_trb.event_data,
            event_meta.edtla,
            event_meta.previous_completion_code,
            true,
            endpoint_id,
            slot_id,
        );
        event_sender.send(event)?;
    }

    event_meta.zero();

    Ok(())
}

#[derive(Debug, Default, Clone)]
enum NormalSubmissionState {
    #[default]
    NoTrbSubmitted,
    UnsupportedTrbType(RawTrb),
    AwaitingRealTransfer(u64, NormalTrb),
    ConsumedEventDataTrb,
}

#[derive(Debug)]
pub struct OutEndpointHandle<ROEH: RealOutEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    pcap_meta: EndpointPcapMeta,
    real_ep: ROEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: NormalSubmissionState,
    event_meta: EventDataTrbMetadata,
}

impl<ROEH: RealOutEndpointHandle> OutEndpointHandle<ROEH> {
    pub fn new(
        slot_id: u8,
        endpoint_id: u8,
        pcap_meta: EndpointPcapMeta,
        real_ep: ROEH,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) -> Self {
        Self {
            slot_id,
            endpoint_id,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
            submission_state: NormalSubmissionState::NoTrbSubmitted,
            event_meta: EventDataTrbMetadata::default(),
        }
    }

    fn handle_normal_trb_pre_hardware(
        &mut self,
        address: u64,
        trb: NormalTrb,
    ) -> anyhow::Result<()> {
        trace!("handle_normal_trb_pre_hardware Out");

        let data = if trb.immediate_data {
            if trb.transfer_length > 8 {
                todo!("using IDT with length > 8");
            }
            trb.data_pointer.to_le_bytes()[..trb.transfer_length as usize].to_vec()
        } else {
            let mut data = vec![0; trb.transfer_length as usize];
            self.dma_bus.read_bulk(trb.data_pointer, &mut data);
            data
        };

        self.real_ep.submit(data.clone())?;
        pcap::out_submission(self.pcap_meta, address, &data, trb.transfer_length);

        self.submission_state = NormalSubmissionState::AwaitingRealTransfer(address, trb);
        self.event_meta.previous_completion_code = CompletionCode::Success;

        Ok(())
    }

    fn handle_normal_trb_post_hardware(
        &mut self,
        address: u64,
        trb: NormalTrb,
    ) -> anyhow::Result<()> {
        trace!("handle_normal_trb_post_hardware Out");

        self.event_meta.add(trb.transfer_length);

        if trb.interrupt_on_completion {
            let event = EventTrb::new_transfer_event_trb(
                address,
                0,
                CompletionCode::Success,
                false,
                self.endpoint_id,
                self.slot_id,
            );
            self.event_sender.send(event)?;
        }

        pcap::out_completion(self.pcap_meta, address, trb.transfer_length);

        self.event_meta.previous_completion_code = CompletionCode::Success;

        // The TRB chain is done so we reset after just finishing it.
        if !trb.chain {
            self.event_meta = EventDataTrbMetadata::default();
        }

        Ok(())
    }

    fn handle_event_data_trb(&mut self, trb: EventDataTrb) -> anyhow::Result<()> {
        handle_event_data_trb_normal_ep(
            &trb,
            &mut self.event_meta,
            self.endpoint_id,
            self.slot_id,
            &self.event_sender,
        )?;

        self.submission_state = NormalSubmissionState::ConsumedEventDataTrb;
        Ok(())
    }
}

impl<ROEH: RealOutEndpointHandle> EndpointHandle for OutEndpointHandle<ROEH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        assert!(
            matches!(self.submission_state, NormalSubmissionState::NoTrbSubmitted),
            "submit_trb called twice without calling next_completion"
        );

        match TransferTrbVariant::parse(trb.buffer) {
            TransferTrbVariant::Normal(normal) => {
                self.handle_normal_trb_pre_hardware(trb.address, normal)?;
            }
            TransferTrbVariant::EventData(event_data) => {
                self.handle_event_data_trb(event_data)?;
            }
            _ => self.submission_state = NormalSubmissionState::UnsupportedTrbType(trb),
        }

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        assert!(
            !matches!(self.submission_state, NormalSubmissionState::NoTrbSubmitted),
            "next_completion called without prior submit_trb"
        );

        Box::pin(async {
            let result = match &self.submission_state {
                NormalSubmissionState::ConsumedEventDataTrb => {
                    trace!(
                        "Slot {} Endpoint {} Consumed Event Data Trb",
                        self.slot_id,
                        self.endpoint_id
                    );
                    TrbProcessingResult::Ok
                }
                NormalSubmissionState::UnsupportedTrbType(ref trb) => {
                    let transfer_event = EventTrb::new_transfer_event_trb(
                        trb.address,
                        0,
                        CompletionCode::TrbError,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(transfer_event)?;

                    TrbProcessingResult::TrbError
                }
                NormalSubmissionState::AwaitingRealTransfer(address, normal) => {
                    match &self.real_ep.next_completion().await? {
                        OutTrbProcessingResult::Disconnect => {
                            info!(
                                "Device has been disconnected. slot {} ep {}",
                                self.slot_id, self.endpoint_id
                            );
                            pcap::out_error(
                                self.pcap_meta,
                                *address,
                                &OutTrbProcessingResult::Disconnect,
                                &[],
                            );

                            let event = EventTrb::new_transfer_event_trb(
                                *address,
                                0,
                                CompletionCode::UsbTransactionError,
                                false,
                                self.endpoint_id,
                                self.slot_id,
                            );
                            self.event_sender.send(event)?;

                            TrbProcessingResult::Disconnect
                        }
                        OutTrbProcessingResult::Stall => {
                            debug!(
                                "Device Stall while waiting for hardware response. slot {} ep {}",
                                self.slot_id, self.endpoint_id
                            );
                            pcap::out_error(
                                self.pcap_meta,
                                *address,
                                &OutTrbProcessingResult::Stall,
                                &[],
                            );

                            let event = EventTrb::new_transfer_event_trb(
                                *address,
                                0,
                                CompletionCode::StallError,
                                false,
                                self.endpoint_id,
                                self.slot_id,
                            );
                            self.event_sender.send(event)?;

                            // We do not need to rewind the dequeue pointer (hence the None)
                            // here like the IN endpoint with TD aggregation would need to do.
                            TrbProcessingResult::Stall(None)
                        }
                        OutTrbProcessingResult::TransactionError => {
                            info!("Transaction Error while waiting for hardware response. slot {} ep {}",
                                self.slot_id, self.endpoint_id);
                            pcap::out_error(
                                self.pcap_meta,
                                *address,
                                &OutTrbProcessingResult::TransactionError,
                                &[],
                            );

                            let event = EventTrb::new_transfer_event_trb(
                                *address,
                                0,
                                CompletionCode::UsbTransactionError,
                                false,
                                self.endpoint_id,
                                self.slot_id,
                            );
                            self.event_sender.send(event)?;

                            TrbProcessingResult::TransactionError(None)
                        }
                        OutTrbProcessingResult::Success => {
                            self.handle_normal_trb_post_hardware(*address, normal.clone())?;
                            TrbProcessingResult::Ok
                        }
                    }
                }
                NormalSubmissionState::NoTrbSubmitted => {
                    unreachable!("internal error: Always set a different ControlSubmissionState in submit_trb().")
                }
            };
            self.submission_state = NormalSubmissionState::NoTrbSubmitted;

            Ok(result)
        })
    }
}

impl<ROEH: RealOutEndpointHandle> BaseEndpointHandle for OutEndpointHandle<ROEH> {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.cancel().await })
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.clear_halt().await })
    }
}

#[derive(Debug)]
enum SupportedNormalEndpointTrb {
    Normal(NormalTrb),
    EventData(EventDataTrb),
    NoOp(NoOpTrb),
}

impl TryFrom<TransferTrbVariant> for SupportedNormalEndpointTrb {
    type Error = TransferTrbVariant;

    fn try_from(trb: TransferTrbVariant) -> Result<Self, Self::Error> {
        match trb {
            TransferTrbVariant::Normal(t) => Ok(Self::Normal(t)),
            TransferTrbVariant::EventData(t) => Ok(Self::EventData(t)),
            TransferTrbVariant::NoOp(t) => Ok(Self::NoOp(t)),
            variant => Err(variant),
        }
    }
}

impl SupportedEndpointTrb<SupportedNormalEndpointTrb> {
    const fn chain(&self) -> bool {
        match &self.variant {
            SupportedNormalEndpointTrb::Normal(normal) => normal.chain,
            SupportedNormalEndpointTrb::EventData(event_data) => event_data.chain,
            SupportedNormalEndpointTrb::NoOp(noop) => noop.chain,
        }
    }

    const fn transfer_length(&self) -> usize {
        match &self.variant {
            SupportedNormalEndpointTrb::Normal(normal) => normal.transfer_length as usize,
            SupportedNormalEndpointTrb::EventData(_) => 0,
            SupportedNormalEndpointTrb::NoOp(_) => 0,
        }
    }
}

#[derive(Debug)]
enum TdBasedNormalSubmissionState {
    CollectingTd(Vec<SupportedEndpointTrb<SupportedNormalEndpointTrb>>),
    AwaitingRealTransfer(Vec<SupportedEndpointTrb<SupportedNormalEndpointTrb>>),
    UnsupportedTrb,
}

impl Default for TdBasedNormalSubmissionState {
    fn default() -> Self {
        Self::CollectingTd(vec![])
    }
}

#[derive(Debug)]
pub struct TdBasedInEndpointHandle<RIEH: RealInEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    pcap_meta: EndpointPcapMeta,
    real_ep: RIEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: TdBasedNormalSubmissionState,
}

impl<RIEH: RealInEndpointHandle> TdBasedInEndpointHandle<RIEH> {
    pub fn new(
        slot_id: u8,
        endpoint_id: u8,
        pcap_meta: EndpointPcapMeta,
        real_ep: RIEH,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) -> Self {
        Self {
            slot_id,
            endpoint_id,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
            submission_state: TdBasedNormalSubmissionState::default(),
        }
    }
}

impl<RIEH: RealInEndpointHandle> EndpointHandle for TdBasedInEndpointHandle<RIEH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        let trbs = match &mut self.submission_state {
            TdBasedNormalSubmissionState::CollectingTd(trbs) => trbs,
            state => {
                return Err(anyhow!(
                    "TdBasedInEndpointHandle called while in state {state:?}; there is a logic error somewhere"
                ));
            }
        };

        let supported_trb = match SupportedEndpointTrb::<SupportedNormalEndpointTrb>::new(
            trb.address,
            trb.buffer,
        ) {
            Ok(supported_trb) => supported_trb,
            Err(transfer_trb) => {
                warn!(
                    "Encountered unsupported TRB on In Endpoint (slot {}, ep {}): {transfer_trb:?}",
                    self.slot_id, self.endpoint_id
                );
                self.submission_state = TdBasedNormalSubmissionState::UnsupportedTrb;
                return Ok(());
            }
        };

        let end_of_td = !supported_trb.chain();
        trbs.push(supported_trb);
        if end_of_td {
            let td_request_length = trbs
                .iter()
                .map(SupportedEndpointTrb::transfer_length)
                .sum::<usize>();

            debug!(
                "Submitting on ep {} a real request for {td_request_length} bytes",
                self.endpoint_id
            );
            self.real_ep.submit(td_request_length)?;

            replace_with_or_abort(&mut self.submission_state, |old_state| {
                let TdBasedNormalSubmissionState::CollectingTd(trbs) = old_state else {
                    unreachable!("verified the state is CollectingTd at the start of the function");
                };
                TdBasedNormalSubmissionState::AwaitingRealTransfer(trbs)
            });
        }

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            match &mut self.submission_state {
                TdBasedNormalSubmissionState::CollectingTd(_) => Ok(TrbProcessingResult::Ok),
                TdBasedNormalSubmissionState::UnsupportedTrb => {
                    self.submission_state = TdBasedNormalSubmissionState::default();
                    Ok(TrbProcessingResult::TrbError)
                }
                TdBasedNormalSubmissionState::AwaitingRealTransfer(trbs) => {
                    let completion = self.real_ep.next_completion().await?;
                    let trbs = mem::take(trbs);
                    self.submission_state = TdBasedNormalSubmissionState::default();
                    let processing_result = process_real_transfer_response(
                        self.endpoint_id,
                        self.slot_id,
                        completion,
                        trbs,
                        &self.event_sender,
                        &self.dma_bus,
                        self.pcap_meta,
                    )?;
                    Ok(processing_result)
                }
            }
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn process_real_transfer_response(
    endpoint_id: u8,
    slot_id: u8,
    completion: InTrbProcessingResult,
    trbs: Vec<SupportedEndpointTrb<SupportedNormalEndpointTrb>>,
    event_sender: &EventSender,
    dma_bus: &BusDeviceRef,
    pcap_meta: EndpointPcapMeta,
) -> anyhow::Result<TrbProcessingResult> {
    debug!(
        "received device response with {} bytes",
        completion.data.len(),
    );

    let mut td_info = TdProcessingInfo {
        event_sender,
        dma_bus,
        pcap_meta,
        status: completion.status,
        state: TdProcessingState::Default,
        data: &completion.data,
        endpoint_id,
        slot_id,
        event_meta: &mut EventDataTrbMetadata::default(),
    };

    for trb in trbs {
        if let Some(early_return_result) = td_info.process_trb(trb)? {
            return Ok(early_return_result);
        }
    }

    if !td_info.data.is_empty() {
        warn!(
            "leftover data on IN TD (received: {} bytes, leftover: {}",
            completion.data.len(),
            td_info.data.len(),
        );
    }

    Ok(TrbProcessingResult::Ok)
}

struct TdProcessingInfo<'a> {
    // always the same
    endpoint_id: u8,
    slot_id: u8,
    event_sender: &'a EventSender,
    dma_bus: &'a BusDeviceRef,
    pcap_meta: EndpointPcapMeta,
    // per TD data
    status: InTrbProcessingStatus,
    // updated every TRB
    state: TdProcessingState,
    data: &'a [u8],
    /// Values we will need to handle an incoming Event Data Trb.
    event_meta: &'a mut EventDataTrbMetadata,
}

enum TdProcessingState {
    Default,
    // no more data, skip forward to next TD
    /// missing bytes
    ShortTransfer(usize),
}

impl<'a> TdProcessingInfo<'a> {
    fn process_trb(
        &mut self,
        trb: SupportedEndpointTrb<SupportedNormalEndpointTrb>,
    ) -> anyhow::Result<Option<TrbProcessingResult>> {
        match trb.variant {
            SupportedNormalEndpointTrb::Normal(normal) => {
                self.handle_normal_trb(trb.addr, trb.cycle_bit, normal)
            }
            SupportedNormalEndpointTrb::EventData(event_data) => {
                self.handle_event_data_trb(event_data)
            }
            SupportedNormalEndpointTrb::NoOp(noop) => {
                if noop.interrupt_on_completion {
                    let transfer_event = EventTrb::new_transfer_event_trb(
                        trb.addr,
                        0,
                        CompletionCode::Success,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(transfer_event)?;
                }
                Ok(None)
            }
        }
    }

    fn handle_normal_trb(
        &mut self,
        addr: u64,
        cs: bool,
        trb_data: NormalTrb,
    ) -> anyhow::Result<Option<TrbProcessingResult>> {
        match self.state {
            TdProcessingState::Default => {
                pcap::in_submission(self.pcap_meta, addr, trb_data.transfer_length);

                let bytes_requested = trb_data.transfer_length as usize;
                let bytes_available = self.data.len();
                let dma_byte_count = bytes_requested.min(bytes_available);
                let bytes = &self.data[..dma_byte_count];
                self.data = &self.data[dma_byte_count..];

                debug!(
                    "copying {dma_byte_count} bytes to {:#x}",
                    trb_data.data_pointer
                );
                self.dma_bus.write_bulk(trb_data.data_pointer, bytes);

                // SAFETY: The maximum value added is defined as 17bit.
                self.event_meta.add(dma_byte_count as u32);

                if bytes_available < bytes_requested {
                    let bytes_missing = bytes_requested - bytes_available;
                    match self.status {
                        InTrbProcessingStatus::Success => {
                            // short transfer
                            if trb_data.interrupt_on_completion || trb_data.interrupt_on_short {
                                let transfer_event = EventTrb::new_transfer_event_trb(
                                    addr,
                                    bytes_missing as u32,
                                    CompletionCode::ShortPacket,
                                    false,
                                    self.endpoint_id,
                                    self.slot_id,
                                );
                                self.event_sender.send(transfer_event)?;
                            }
                            self.state = TdProcessingState::ShortTransfer(bytes_missing);

                            pcap::in_completion(self.pcap_meta, addr, bytes);

                            return Ok(None);
                        }
                        _ => {
                            let (completion_code, processing_result) = match self.status {
                                InTrbProcessingStatus::Disconnect => (
                                    CompletionCode::UsbTransactionError,
                                    TrbProcessingResult::Disconnect,
                                ),
                                InTrbProcessingStatus::Stall => (
                                    CompletionCode::StallError,
                                    TrbProcessingResult::Stall(Some((addr, cs))),
                                ),
                                InTrbProcessingStatus::TransactionError => (
                                    CompletionCode::UsbTransactionError,
                                    TrbProcessingResult::TransactionError(Some((addr, cs))),
                                ),
                                InTrbProcessingStatus::Success => {
                                    unreachable!("handled by outer match")
                                }
                            };
                            let transfer_event = EventTrb::new_transfer_event_trb(
                                addr,
                                bytes_missing as u32,
                                completion_code,
                                false,
                                self.endpoint_id,
                                self.slot_id,
                            );
                            self.event_sender.send(transfer_event)?;

                            pcap::in_error(self.pcap_meta, addr, &self.status);

                            return Ok(Some(processing_result));
                        }
                    }
                }

                pcap::in_completion(self.pcap_meta, addr, bytes);

                // event sending only when IOC is set
                if trb_data.interrupt_on_completion {
                    let event = EventTrb::new_transfer_event_trb(
                        addr,
                        0,
                        CompletionCode::Success,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(event)?;
                }

                Ok(None)
            }
            TdProcessingState::ShortTransfer(bytes_missing) => {
                // Only react to IOC after a short packet has been encountered.
                if trb_data.interrupt_on_completion {
                    let transfer_event = EventTrb::new_transfer_event_trb(
                        addr,
                        bytes_missing as u32,
                        CompletionCode::ShortPacket,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(transfer_event)?;
                }

                Ok(None)
            }
        }
    }

    fn handle_event_data_trb(
        &mut self,
        trb_data: EventDataTrb,
    ) -> anyhow::Result<Option<TrbProcessingResult>> {
        handle_event_data_trb_normal_ep(
            &trb_data,
            self.event_meta,
            self.endpoint_id,
            self.slot_id,
            self.event_sender,
        )?;

        Ok(None)
    }
}

impl<RIEH: RealInEndpointHandle> BaseEndpointHandle for TdBasedInEndpointHandle<RIEH> {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async {
            self.submission_state = TdBasedNormalSubmissionState::default();
            self.real_ep.cancel().await
        })
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.clear_halt().await })
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    use crate::device::xhci::endpoint_handle::tests::testutils::{
        MockRealControlEndpointExpectDataPattern, MockRealControlEndpointHardwareError,
        MockRealControlEndpointReadStatic, MockRealInEndpoint, MockRealOutEndpoint,
    };
    use crate::device::xhci::interrupter::tests::testutils::MockInterrupter;
    use crate::device::{bus::testutils::TestBusDevice, xhci::trb::testutils::RawTrbBuilder};
    use crate::dynamic_bus::DynamicBus;

    use std::sync::Arc;

    const SLOT_ID: u8 = 1;
    const ENDPOINT_ID: u8 = 1;

    const FIRST_ADDRESS: u64 = 0x10;
    const SECOND_ADDRESS: u64 = 0x20;
    const THIRD_ADDRESS: u64 = 0x30;
    const FOURTH_ADDRESS: u64 = 0x40;
    const FIFTH_ADDRESS: u64 = 0x50;
    const SIXTH_ADDRESS: u64 = 0x60;

    const DMA_POINTER_1: u64 = 0x200;
    const DMA_POINTER_2: u64 = 0x400;
    const DMA_POINTER_3: u64 = 0x600;
    const DMA_POINTER_4: u64 = 0x800;

    const SETUP_WLENGTH: u16 = 512;
    const TRANSFER_LENGTH: u32 = SETUP_WLENGTH as u32;
    const EVENT_DATA_FIELD: u64 = 0xda7a;

    const TRB_TYPE_NORMAL: u8 = 0x1;
    const TRB_TYPE_SETUP_STAGE: u8 = 0x2;
    const TRB_TYPE_DATA_STAGE: u8 = 0x3;
    const TRB_TYPE_STATUS_STAGE: u8 = 0x4;
    const TRB_TYPE_EVENT_DATA: u8 = 0x7;

    const SETUP_BM_REQUEST_TYPE_IN: u8 = 0x80;
    const SETUP_BM_REQUEST_TYPE_OUT: u8 = 0;

    const SETUP_TRANSFER_TYPE_OUT_DATA: u8 = 0x2;
    const SETUP_TRANSFER_TYPE_IN_DATA: u8 = 0x3;

    pub mod testutils {
        use super::*;

        // will return `vec![42; requested length]`
        // if given a max_returned_data, it will return no more than that number of bytes
        #[derive(Debug)]
        pub struct MockRealControlEndpointReadStatic {
            data_length: u32,
            direction: bool,
            max_returned_data: Option<u32>,
        }

        impl MockRealControlEndpointReadStatic {
            pub fn new(max_returned_data: Option<u32>) -> Self {
                Self {
                    data_length: 0,
                    direction: false,
                    max_returned_data,
                }
            }
        }

        impl RealControlEndpointHandle for MockRealControlEndpointReadStatic {
            type TrbCompletionFuture<'a> = Pin<
                Box<
                    dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a,
                >,
            >;

            fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()> {
                // fake request is instantly submitted but we need to remember the direction for next_complete
                const IN: u8 = 0b10000000;
                self.direction = (request.request_type & IN) == IN;
                self.data_length = if let Some(max_returned_data) = self.max_returned_data {
                    // SAFETY: already checked is_some() above
                    min(max_returned_data, request.length)
                } else {
                    request.length
                };
                Ok(())
            }

            fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
                Box::pin(async {
                    let result = match self.direction {
                        true => {
                            let data = vec![42; self.data_length as usize];
                            ControlRequestProcessingResult::SuccessfulControlIn(data)
                        }
                        false => ControlRequestProcessingResult::SuccessfulControlOut,
                    };
                    Ok(result)
                })
            }
        }

        // expecting to receive 0xda7a via an out request
        #[derive(Debug)]
        pub struct MockRealControlEndpointExpectDataPattern {}

        impl MockRealControlEndpointExpectDataPattern {
            pub fn new() -> Self {
                Self {}
            }
        }

        impl RealControlEndpointHandle for MockRealControlEndpointExpectDataPattern {
            type TrbCompletionFuture<'a> = Pin<
                Box<
                    dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a,
                >,
            >;

            fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()> {
                assert_eq!(request.data, 0xda7a_u64.to_le_bytes()[..2]);
                Ok(())
            }

            fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
                Box::pin(async {
                    let result = ControlRequestProcessingResult::SuccessfulControlOut;
                    Ok(result)
                })
            }
        }

        impl BaseEndpointHandle for MockRealControlEndpointExpectDataPattern {
            type CompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

            fn cancel(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }

            fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }
        }

        // expecting to receive 0xda7a via an out request
        #[derive(Debug)]
        pub struct MockRealControlEndpointHardwareError {
            error: ControlRequestProcessingResult,
        }

        impl MockRealControlEndpointHardwareError {
            pub fn new(error: ControlRequestProcessingResult) -> Self {
                Self { error }
            }
        }

        impl RealControlEndpointHandle for MockRealControlEndpointHardwareError {
            type TrbCompletionFuture<'a> = Pin<
                Box<
                    dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a,
                >,
            >;

            fn submit_control_request(&mut self, _request: UsbRequest) -> anyhow::Result<()> {
                Ok(())
            }

            fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
                Box::pin(async { Ok(self.error.clone()) })
            }
        }

        impl BaseEndpointHandle for MockRealControlEndpointHardwareError {
            type CompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

            fn cancel(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }

            fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }
        }

        impl BaseEndpointHandle for MockRealControlEndpointReadStatic {
            type CompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

            fn cancel(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }

            fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }
        }

        // will return `vec![42; requested length]`
        #[derive(Debug)]
        pub struct MockRealInEndpoint {
            data_length: usize,
        }

        impl MockRealInEndpoint {
            pub fn new() -> Self {
                Self { data_length: 0 }
            }
        }

        impl RealInEndpointHandle for MockRealInEndpoint {
            type TrbCompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<InTrbProcessingResult>> + Send + 'a>>;

            fn submit(&mut self, data: usize) -> anyhow::Result<()> {
                self.data_length = data;
                Ok(())
            }

            fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
                Box::pin(async {
                    let data = vec![42; self.data_length];
                    let result = InTrbProcessingResult {
                        status: InTrbProcessingStatus::Success,
                        data,
                    };
                    Ok(result)
                })
            }
        }

        impl BaseEndpointHandle for MockRealInEndpoint {
            type CompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

            fn cancel(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }

            fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }
        }

        // mock for bulk out real endpoint returning success while discarding the data
        #[derive(Debug)]
        pub struct MockRealOutEndpoint {}
        impl MockRealOutEndpoint {
            pub fn new() -> Self {
                Self {}
            }
        }

        impl RealOutEndpointHandle for MockRealOutEndpoint {
            type TrbCompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<OutTrbProcessingResult>> + Send + 'a>>;

            fn submit(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
                println!("consumed data of length: {}", data.len());
                Ok(())
            }

            fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
                Box::pin(async {
                    let result = OutTrbProcessingResult::Success;
                    Ok(result)
                })
            }
        }

        impl BaseEndpointHandle for MockRealOutEndpoint {
            type CompletionFuture<'a> =
                Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

            fn cancel(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }

            fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
                // nothing we want to do
                Box::pin(async { Ok(()) })
            }
        }
    }

    // Initialize test environment using the MockRealControlEndpointReadStatic
    //
    // Use the ControlEndpointHandle to submit some TransferTrb.
    // Use the UnboundedReceiver to directly check events meant for a EventRing.
    fn init_control_endpoint_handle_test<T: RealControlEndpointHandle>(
        real_ep: T,
    ) -> (MockInterrupter, ControlEndpointHandle<T>) {
        let pcap_usb_bus_number = 1;
        let pcap_meta = EndpointPcapMeta::control(pcap_usb_bus_number, SLOT_ID, ENDPOINT_ID);

        let dma_bus = Arc::new(DynamicBus::new());
        let dma_backing = vec![99; 2048];
        dma_bus
            .add(0x0, Arc::new(TestBusDevice::new(&dma_backing[..])))
            .expect("Adding Memory to the DynamicBus should never fail.");

        let (event_sender, interrupter) = MockInterrupter::new();

        let control_endpoint = ControlEndpointHandle::new(
            SLOT_ID,
            ENDPOINT_ID,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
        );
        (interrupter, control_endpoint)
    }

    /// Wrapper to simplify creating a successful expected EventTrb for comparison.
    fn expected_event(trb_pointer: u64, trb_transfer_length: u32, event_data: bool) -> EventTrb {
        EventTrb::new_transfer_event_trb(
            trb_pointer,
            trb_transfer_length,
            CompletionCode::Success,
            event_data,
            ENDPOINT_ID,
            SLOT_ID,
        )
    }

    #[tokio::test]
    async fn submit_shortest_possible_control_in_request() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        let input_trb = vec![setup_stage, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn submit_shortest_possible_control_in_request_with_data_stage() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_direction()
            .build();
        let status_stage = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        let input_trb = vec![setup_stage, data_stage, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn submit_control_in_with_empty_wlength_but_have_transferred_data() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            // use vendor specific theoretically spec violating data in wLength
            .with_setup_wlength(0x0)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_direction()
            .build();
        let status_stage = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        let input_trb = vec![setup_stage, data_stage, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false)),
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn submit_control_out_with_empty_wlength_but_have_transferred_data() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_OUT)
            // use vendor specific theoretically spec violating data in wLength
            .with_setup_wlength(0x0)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .build();

        let input_trb = vec![setup_stage, data_stage, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false)),
            "second trb"
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn submit_control_in_with_less_wlength_than_expected_data() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            // system software made a mistake; should be SETUP_WLENGTH*3 or TRANSFER_LENGTH*3
            .with_setup_wlength(SETUP_WLENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_direction()
            .build();
        let normal_1 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_pointer(DMA_POINTER_2)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let normal_2 = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_pointer(DMA_POINTER_3)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let status_stage = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        let input_trb = vec![setup_stage, data_stage, normal_1, normal_2, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FOURTH_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIFTH_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn submit_control_in_with_more_wlength_than_expected_data() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            // system software made a mistake; should be TRANSFER_LENGTH*3
            .with_setup_wlength(SETUP_WLENGTH * 3)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_direction()
            .build();
        let status_stage = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        let input_trb = vec![setup_stage, data_stage, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn submit_second_illegal_data_stage_trb() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage_1 = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_direction()
            .build();
        let data_stage_2 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_direction()
            .build();
        let status_stage = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        let input_trb = vec![setup_stage, data_stage_1, data_stage_2, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            control_endpoint.next_completion().await.ok();
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                THIRD_ADDRESS,
                0,
                CompletionCode::TrbError,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        // Usually an endpoint should halt after encountering an error.

        // This test does not include a full endpoint and thus can not stop.

        // If we do not halt we expect to encounter the rest of the Control Request
        // to be unexpected/out of order TRB's and send a CompletionCode::TrbError.

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                FOURTH_ADDRESS,
                0,
                CompletionCode::TrbError,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn submit_control_out_request_with_data_stage_using_immediate_data() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointExpectDataPattern::new());

        const DMA_POINTER: u64 = 0xeb8bda7a;
        const TRANSFER_LENGTH: u32 = 2;

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_OUT)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_OUT_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_interrupt_on_completion()
            .with_immediate_data()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .build();

        let input_trb = vec![setup_stage, data_stage, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn submit_control_in_request_with_event_data_at_the_end_of_the_data_stage() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(0x2)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_trb_type(0x3)
            .with_direction()
            .build();
        let event_data = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_pointer(EVENT_DATA_FIELD)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_direction()
            .build();
        let status_stage = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(0x4)
            .with_direction()
            .build();

        let input_trb = vec![setup_stage, data_stage, event_data, status_stage];

        for trb in input_trb {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(EVENT_DATA_FIELD, 512, true))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FOURTH_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn submit_control_in_request_with_interleaved_event_data_in_the_data_td() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_wlength(SETUP_WLENGTH * 2)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_direction()
            .build();
        let event_data_1 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_pointer(EVENT_DATA_FIELD + 1)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_direction()
            .build();
        let normal = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_pointer(DMA_POINTER_2)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let event_data_2 = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_data_pointer(EVENT_DATA_FIELD + 2)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_direction()
            .build();
        let status_stage = RawTrbBuilder::new(SIXTH_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        let input_trb = vec![
            setup_stage,
            data_stage,
            event_data_1,
            normal,
            event_data_2,
            status_stage,
        ];

        for trb in input_trb {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint
                    .next_completion()
                    .await
                    .expect("this mock hardware request should never fail"),
                TrbProcessingResult::Ok
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(EVENT_DATA_FIELD + 1, 512, true))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(EVENT_DATA_FIELD + 2, 512, true))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SIXTH_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn submit_control_in_request_with_event_data_after_status_stage_trb() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_chain()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();
        let event_data = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_pointer(EVENT_DATA_FIELD)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_direction()
            .build();

        let input_trb = vec![setup_stage, status_stage, event_data];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(EVENT_DATA_FIELD, 0, true))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn submitting_out_of_order_or_unfinished_sequence_does_not_prevent_the_following_valid_sequence_of_trb(
    ) {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let status_stage_out_of_order = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();
        let setup_stage_incomplete_sequence = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let setup_stage = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        let input_trb = vec![
            status_stage_out_of_order,
            setup_stage_incomplete_sequence,
            setup_stage,
            status_stage,
        ];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            control_endpoint.next_completion().await.ok();
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                FIRST_ADDRESS,
                0,
                CompletionCode::TrbError,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        // Usually an endpoint should halt after encountering an error.

        // This test does not include a full endpoint and thus can not stop.

        // If we do not halt we expect to encounter the rest of the Control Request
        // to be unexpected/out of order TRB's and send a CompletionCode::TrbError.

        // If a new Control Request Initializes we expect it to work regardless
        // of previous aggregation state.

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );

        // Repeating the spontaneous new control request chain again:

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FOURTH_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn control_request_returns_hardware_disconnect() {
        let (mut interrupter, mut control_endpoint) = init_control_endpoint_handle_test(
            MockRealControlEndpointHardwareError::new(ControlRequestProcessingResult::Disconnect),
        );

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        control_endpoint
            .submit_trb(setup_stage)
            .expect("this mock hardware request should never fail");
        control_endpoint.next_completion().await.ok();

        control_endpoint
            .submit_trb(status_stage)
            .expect("this mock hardware request should never fail");
        // We can assert here because it is the last TRB of the control
        // request chain, when the actual request happens and the error code is
        // available.
        assert_eq!(
            control_endpoint.next_completion().await.ok(),
            Some(TrbProcessingResult::Disconnect)
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                FIRST_ADDRESS,
                0,
                CompletionCode::UsbTransactionError,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn control_request_returns_hardware_stall() {
        let (mut interrupter, mut control_endpoint) = init_control_endpoint_handle_test(
            MockRealControlEndpointHardwareError::new(ControlRequestProcessingResult::Stall),
        );

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        control_endpoint
            .submit_trb(setup_stage)
            .expect("this mock hardware request should never fail");
        control_endpoint.next_completion().await.ok();

        control_endpoint
            .submit_trb(status_stage)
            .expect("this mock hardware request should never fail");
        // We can assert here because it is the last TRB of the control
        // request chain, when the actual request happens and the error code is
        // available.
        assert_eq!(
            control_endpoint.next_completion().await.ok(),
            Some(TrbProcessingResult::Stall(None))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                FIRST_ADDRESS,
                0,
                CompletionCode::StallError,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn control_request_returns_hardware_transaction_error() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointHardwareError::new(
                ControlRequestProcessingResult::TransactionError,
            ));

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        control_endpoint
            .submit_trb(setup_stage)
            .expect("this mock hardware request should never fail");
        control_endpoint.next_completion().await.ok();

        control_endpoint
            .submit_trb(status_stage)
            .expect("this mock hardware request should never fail");
        // We can assert here because it is the last TRB of the control
        // request chain, when the actual request happens and the error code is
        // available.
        assert_eq!(
            control_endpoint.next_completion().await.ok(),
            Some(TrbProcessingResult::TransactionError(None))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                FIRST_ADDRESS,
                0,
                CompletionCode::UsbTransactionError,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn tolerate_wrong_status_stage_direction_mapping_short_out() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let setup_stage_out = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_OUT)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let status_stage_out = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .build();

        // xhci specification Table 4-7: USB SETUP Data to Data Stage TRB and Status Stage TRB mapping:
        let bad_input_trb = vec![setup_stage_out, status_stage_out];

        for trb in bad_input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn tolerate_wrong_status_stage_direction_mapping_long_out() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let setup_stage_in = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage_in = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_direction()
            .build();
        let status_stage_in = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        // xhci specification Table 4-7: USB SETUP Data to Data Stage TRB and Status Stage TRB mapping:
        let bad_input_trb = vec![setup_stage_in, data_stage_in, status_stage_in];

        for trb in bad_input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn tolerate_wrong_status_stage_direction_mapping_long_in() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(MockRealControlEndpointReadStatic::new(None));

        let setup_stage_out = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_OUT)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage_out = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .build();
        let status_stage_out = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .build();

        // xhci specification Table 4-7: USB SETUP Data to Data Stage TRB and Status Stage TRB mapping:
        let bad_input_trb = vec![setup_stage_out, data_stage_out, status_stage_out];

        for trb in bad_input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn control_request_returns_short_packet() {
        const SHORT_AFTER_BYTES: u32 = 300;

        let (mut interrupter, mut control_endpoint) = init_control_endpoint_handle_test(
            MockRealControlEndpointReadStatic::new(Some(SHORT_AFTER_BYTES)),
        );

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_direction()
            .build();
        let status_stage = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        let input_trb = vec![setup_stage, data_stage, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                SECOND_ADDRESS,
                TRANSFER_LENGTH - SHORT_AFTER_BYTES,
                CompletionCode::ShortPacket,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn long_control_request_returns_short_packet() {
        const SHORT_AFTER_BYTES: u32 = 512 * 2 + 300;
        let (mut interrupter, mut control_endpoint) = init_control_endpoint_handle_test(
            MockRealControlEndpointReadStatic::new(Some(SHORT_AFTER_BYTES)),
        );

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_direction()
            .build();
        let normal_1 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_pointer(DMA_POINTER_2)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_NORMAL)
            .with_direction()
            .build();
        let normal_2 = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_pointer(DMA_POINTER_3)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_NORMAL)
            .with_direction()
            .build();
        let status_stage = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        let input_trb = vec![setup_stage, data_stage, normal_1, normal_2, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                FOURTH_ADDRESS,
                TRANSFER_LENGTH * 3 - SHORT_AFTER_BYTES,
                CompletionCode::ShortPacket,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIFTH_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn long_control_request_returns_short_packet_and_has_subsequent_trb() {
        const SHORT_AFTER_BYTES: u32 = 512 + 300;
        let (mut interrupter, mut control_endpoint) = init_control_endpoint_handle_test(
            MockRealControlEndpointReadStatic::new(Some(SHORT_AFTER_BYTES)),
        );

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_direction()
            .build();
        let normal_1 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_pointer(DMA_POINTER_2)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_NORMAL)
            .with_direction()
            .build();
        let normal_2 = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_pointer(DMA_POINTER_3)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_NORMAL)
            .with_direction()
            .build();
        let normal_3 = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_data_pointer(DMA_POINTER_3)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_NORMAL)
            .with_direction()
            .build();
        let status_stage = RawTrbBuilder::new(SIXTH_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        let input_trb = vec![
            setup_stage,
            data_stage,
            normal_1,
            normal_2,
            normal_3,
            status_stage,
        ];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                THIRD_ADDRESS,
                TRANSFER_LENGTH * 2 - SHORT_AFTER_BYTES,
                CompletionCode::ShortPacket,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        // Partially echo the "TD Completion Event" on every IOC until the end of the TD.
        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                FOURTH_ADDRESS,
                TRANSFER_LENGTH * 2 - SHORT_AFTER_BYTES,
                CompletionCode::ShortPacket,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                FIFTH_ADDRESS,
                TRANSFER_LENGTH * 2 - SHORT_AFTER_BYTES,
                CompletionCode::ShortPacket,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SIXTH_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn long_control_request_returns_short_packet_and_has_subsequent_trb_including_event_data()
    {
        const SHORT_AFTER_BYTES: u32 = 300;
        let (mut interrupter, mut control_endpoint) = init_control_endpoint_handle_test(
            MockRealControlEndpointReadStatic::new(Some(SHORT_AFTER_BYTES)),
        );

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_direction()
            .build();
        let event_data_1 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_pointer(EVENT_DATA_FIELD + 1)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_direction()
            .build();
        let normal = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_pointer(DMA_POINTER_2)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_trb_type(TRB_TYPE_NORMAL)
            .with_direction()
            .build();
        let event_data_2 = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_data_pointer(EVENT_DATA_FIELD + 2)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_direction()
            .build();
        let status_stage = RawTrbBuilder::new(SIXTH_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        let input_trb = vec![
            setup_stage,
            data_stage, // triggers short packet
            event_data_1,
            normal, // subsequent after a short packet
            event_data_2,
            status_stage,
        ];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                EVENT_DATA_FIELD + 1,
                SHORT_AFTER_BYTES,
                CompletionCode::ShortPacket,
                true,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                EVENT_DATA_FIELD + 2,
                0,
                CompletionCode::ShortPacket,
                true,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SIXTH_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn long_control_request_returns_short_packet_and_has_subsequent_trb_with_ioc_including_event_data(
    ) {
        const SHORT_AFTER_BYTES: u32 = 300;
        let (mut interrupter, mut control_endpoint) = init_control_endpoint_handle_test(
            MockRealControlEndpointReadStatic::new(Some(SHORT_AFTER_BYTES)),
        );

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_wlength(SETUP_WLENGTH)
            .with_immediate_data()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_DATA_STAGE)
            .with_direction()
            .build();
        let event_data_1 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_pointer(EVENT_DATA_FIELD + 1)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_direction()
            .build();
        let normal = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_pointer(DMA_POINTER_2)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_NORMAL)
            .with_direction()
            .build();
        let event_data_2 = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_data_pointer(EVENT_DATA_FIELD + 2)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_direction()
            .build();
        let status_stage = RawTrbBuilder::new(SIXTH_ADDRESS)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_STATUS_STAGE)
            .with_direction()
            .build();

        let input_trb = vec![
            setup_stage,
            data_stage, // triggers short packet
            event_data_1,
            normal, // subsequent after a short packet
            event_data_2,
            status_stage,
        ];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                SECOND_ADDRESS,
                TRANSFER_LENGTH - SHORT_AFTER_BYTES,
                CompletionCode::ShortPacket,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                EVENT_DATA_FIELD + 1,
                SHORT_AFTER_BYTES,
                CompletionCode::ShortPacket,
                true,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                FOURTH_ADDRESS,
                TRANSFER_LENGTH - SHORT_AFTER_BYTES,
                CompletionCode::ShortPacket,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_transfer_event_trb(
                EVENT_DATA_FIELD + 2,
                0,
                CompletionCode::ShortPacket,
                true,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SIXTH_ADDRESS, 0, false))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn submit_multi_trb_bulk_in_transfer_with_event_data() {
        const SLOT_ID: u8 = 1;
        const ENDPOINT_ID: u8 = 1;

        let pcap_usb_bus_number = 1;
        let pcap_meta = EndpointPcapMeta::bulk(pcap_usb_bus_number, SLOT_ID, ENDPOINT_ID);

        let real_ep = MockRealInEndpoint::new();

        let dma_bus = Arc::new(DynamicBus::new());
        let dma_backing = vec![99; 2048];
        dma_bus
            .add(0x0, Arc::new(TestBusDevice::new(&dma_backing[..])))
            .expect("Adding Memory to the DynamicBus should never fail.");

        let (event_sender, mut interrupter) = MockInterrupter::new();

        let mut bulk_in_endpoint = TdBasedInEndpointHandle::new(
            SLOT_ID,
            ENDPOINT_ID,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
        );

        let normal_1 = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x6) // remaining TD Size: 1536
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let normal_2 = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_2)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x4) // remaining TD Size: 1024
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let normal_3 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_pointer(DMA_POINTER_3)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x2) // remaining TD Size: 512
            .with_chain()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let event_data_1 = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_pointer(EVENT_DATA_FIELD)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_direction()
            .build();
        let normal_4 = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_data_pointer(DMA_POINTER_4)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let event_data_2 = RawTrbBuilder::new(SIXTH_ADDRESS)
            .with_data_pointer(EVENT_DATA_FIELD)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_direction()
            .build();

        let input_trb = vec![
            normal_1,
            normal_2,
            normal_3,
            event_data_1,
            normal_4,
            event_data_2,
        ];

        for trb in input_trb {
            bulk_in_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                bulk_in_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(EVENT_DATA_FIELD, TRANSFER_LENGTH * 3, true))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(EVENT_DATA_FIELD, TRANSFER_LENGTH, true))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn submit_multi_trb_bulk_out_transfer_with_event_data() {
        const SLOT_ID: u8 = 1;
        const ENDPOINT_ID: u8 = 1;

        let pcap_usb_bus_number = 1;
        let pcap_meta = EndpointPcapMeta::bulk(pcap_usb_bus_number, SLOT_ID, ENDPOINT_ID);

        let real_ep = MockRealOutEndpoint::new();

        let dma_bus = Arc::new(DynamicBus::new());
        let dma_backing = vec![99; 2048];
        dma_bus
            .add(0x0, Arc::new(TestBusDevice::new(&dma_backing[..])))
            .expect("Adding Memory to the DynamicBus should never fail.");

        let (event_sender, mut interrupter) = MockInterrupter::new();

        let mut bulk_out_endpoint = OutEndpointHandle::new(
            SLOT_ID,
            ENDPOINT_ID,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
        );

        let normal_1 = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x6) // remaining TD Size: 1536
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let normal_2 = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_pointer(DMA_POINTER_2)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x4) // remaining TD Size: 1024
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let normal_3 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_pointer(DMA_POINTER_3)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_byte(11, 0x2) // remaining TD Size: 512
            .with_chain()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let event_data_1 = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_pointer(EVENT_DATA_FIELD)
            .with_chain()
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .build();
        let normal_4 = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_data_pointer(DMA_POINTER_4)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_chain()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let event_data_2 = RawTrbBuilder::new(SIXTH_ADDRESS)
            .with_data_pointer(EVENT_DATA_FIELD)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .build();

        let input_trb = vec![
            normal_1,
            normal_2,
            normal_3,
            event_data_1,
            normal_4,
            event_data_2,
        ];

        for trb in input_trb {
            bulk_out_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                bulk_out_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(EVENT_DATA_FIELD, TRANSFER_LENGTH * 3, true))
        );
        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(EVENT_DATA_FIELD, TRANSFER_LENGTH, true))
        );

        assert!(interrupter.is_empty());
    }

    #[tokio::test]
    async fn normal_in_td_followed_by_an_event_data_td() {
        const SLOT_ID: u8 = 1;
        const ENDPOINT_ID: u8 = 1;

        let pcap_usb_bus_number = 1;
        let pcap_meta = EndpointPcapMeta::bulk(pcap_usb_bus_number, SLOT_ID, ENDPOINT_ID);

        let real_ep = MockRealInEndpoint::new();

        let dma_bus = Arc::new(DynamicBus::new());
        let dma_backing = vec![99; 2048];
        dma_bus
            .add(0x0, Arc::new(TestBusDevice::new(&dma_backing[..])))
            .expect("Adding Memory to the DynamicBus should never fail.");

        let (event_sender, mut interrupter) = MockInterrupter::new();

        let mut bulk_in_endpoint = TdBasedInEndpointHandle::new(
            SLOT_ID,
            ENDPOINT_ID,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
        );

        let normal = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_data_pointer(DMA_POINTER_1)
            .with_trb_transfer_length(TRANSFER_LENGTH)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_NORMAL)
            .build();
        let event_data = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_pointer(EVENT_DATA_FIELD)
            .with_interrupt_on_completion()
            .with_trb_type(TRB_TYPE_EVENT_DATA)
            .with_direction()
            .build();

        let input_trb = vec![normal, event_data];

        for trb in input_trb {
            bulk_in_endpoint
                .submit_trb(trb)
                .expect("this mock hardware request should never fail");
            assert_eq!(
                bulk_in_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );

        assert_eq!(
            interrupter.await_event().await,
            Some(expected_event(EVENT_DATA_FIELD, 0, true))
        );

        assert!(interrupter.is_empty());
    }
}
