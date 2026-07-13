use core::panic;
use std::{cmp::Ordering, fmt::Debug, future::Future, pin::Pin};

use anyhow::Ok;
use tracing::{debug, error, info, trace};

use crate::device::{
    bus::BusDeviceRef,
    pcap::{self, EndpointPcapMeta},
    xhci::{
        hotplug_endpoint_handle::BaseEndpointHandle,
        interrupter::EventSender,
        real_endpoint_handle::{
            ControlRequestProcessingResult, InTrbProcessingResult, OutTrbProcessingResult,
            RealControlEndpointHandle, RealInEndpointHandle, RealOutEndpointHandle,
        },
        trb::{
            CompletionCode, DataStageTrb, EventDataTrb, EventTrb, NormalTrb, RawTrb, SetupStageTrb,
            StatusStageTrb, TransferTrbVariant, TrbDmaInfo,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrbProcessingResult {
    Ok,
    Stall,
    TrbError,
    TransactionError,
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
    /// a 24 Bit sized counter to track already transmitted bytes of the current TD
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

    const fn zero(&mut self) {
        self.edtla = 0;
    }
    /// input is 17 Bit
    const fn add(&mut self, addend: u32) {
        self.edtla = MAX_VALUE_U24 & (self.edtla.wrapping_add(addend));
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ControlTransferState {
    /// upcoming or current stage/TD of a control transfer to be handled
    pub state: ControlTransferStage,
    /// holding the UsbRequest and all associated data
    pub data: ControlTransferData,
    event_meta: EventDataTrbMetadata,
}
impl ControlTransferState {
    // previous_completion_code should never be used as is, thus the error as a default value
    const fn new(data: ControlTransferData) -> Self {
        Self {
            state: ControlTransferStage::ExpectSetupStageTrb,
            data,
            event_meta: EventDataTrbMetadata::default(),
        }
    }
}

fn interrupt_on_completion(
    address: u64,
    completion_code: CompletionCode,
    event_data: bool,
    endpoint_id: u8,
    slot_id: u8,
    event_sender: &EventSender,
) -> anyhow::Result<()> {
    trace!("interrupt_on_completion triggered for address {}", address);
    let event = EventTrb::new_transfer_event_trb(
        address,
        0,
        completion_code,
        event_data,
        endpoint_id,
        slot_id,
    );

    event_sender.send(event)?;
    Ok(())
}

#[cfg_attr(doc, aquamarine::aquamarine)]
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
pub enum ControlTransferStage {
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

/// The state machine provides the information partially as ControlSubmissionState::AwaitingControlIn(TransferTrb).
/// Track state between us and the guest for building the current control request.
#[derive(Debug, PartialEq, Eq)]
pub enum ControlTransferData {
    In(UsbRequest),
    Out(UsbRequest),
}

#[derive(Debug)]
pub struct ControlEndpointHandle<RCEH: RealControlEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    pcap_meta: EndpointPcapMeta,
    real_ep: RCEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    /// referring to usbvfiod to hardware communication
    submission_state: ControlSubmissionState,
    /// referring to usbvfiod to guest communication
    transfer_state: ControlTransferState,
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
            submission_state: ControlSubmissionState::NoTrbSubmitted,
            transfer_state: ControlTransferState::new(ControlTransferData::In(
                UsbRequest::default(),
            )),
        }
    }

    fn handle_setup_stage_trb(&mut self, address: u64, trb: SetupStageTrb) -> anyhow::Result<()> {
        let usb_request = UsbRequest {
            address,
            request_type: trb.request_type,
            request: trb.request,
            value: trb.value,
            index: trb.index,
            length: trb.length,
            data_pointer: None,
            data: vec![],
        };

        if trb.request_type & 0x80 != 0 {
            trace!("SetupStage TRB with ControlIn");

            self.transfer_state =
                ControlTransferState::new(ControlTransferData::In(usb_request.clone()));

            self.real_ep.submit_control_request(usb_request.clone())?;
            pcap::control_submission(self.pcap_meta, &usb_request);

            self.submission_state = ControlSubmissionState::AwaitingControlIn(
                address,
                TransferTrbVariant::SetupStage(trb),
            );
        } else {
            trace!("SetupStage TRB with ControlOut");

            self.transfer_state = ControlTransferState::new(ControlTransferData::Out(usb_request));

            // actual hardware request happens in status stage after consuming the data stage td

            if trb.interrupt_on_completion {
                interrupt_on_completion(
                    address,
                    CompletionCode::Success,
                    false,
                    self.endpoint_id,
                    self.slot_id,
                    &self.event_sender,
                )?;
            }

            self.transfer_state.state = ControlTransferStage::MaybeDataStageTrb;
            self.submission_state = ControlSubmissionState::ParserConsumedTrb(
                address,
                TransferTrbVariant::SetupStage(trb),
            );
        }

        Ok(())
    }

    fn handle_setup_stage_hardware_response(
        &mut self,
        address: u64,
        trb: SetupStageTrb,
        hardware_data: &mut Vec<u8>,
    ) -> anyhow::Result<()> {
        match &mut self.transfer_state.data {
            // collect hardware data
            ControlTransferData::In(request) => {
                trace!("control in data {:?}", hardware_data);

                pcap::control_completion_in(self.pcap_meta, request.address, hardware_data);

                request.data.append(hardware_data);

                request.data.resize(trb.length as usize, 0);

                self.transfer_state.event_meta.previous_completion_code = CompletionCode::Success;

                if trb.interrupt_on_completion {
                    interrupt_on_completion(
                        address,
                        CompletionCode::Success,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                        &self.event_sender,
                    )?;
                }

                self.transfer_state.state = ControlTransferStage::MaybeDataStageTrb;
            }
            ControlTransferData::Out(_) => {
                unreachable!("internal error: ControlOut SetupTrb have insufficient information to do the Hardware request; a submission state to arrive here should never be used");
            }
        }
        Ok(())
    }

    fn data_slices<T: TrbDmaInfo>(&mut self, address: u64, trb: &T) {
        let data_pointer = trb.data_pointer();
        let transfer_length = trb.transfer_length();
        let immediate_data = trb.has_immediate_data();

        match &mut self.transfer_state.data {
            ControlTransferData::In(usb_request) => {
                trace!("DMA for ControlIn");

                // From xhci specification chapter 4.11.2.2:
                //
                // System software is responsible for ensuring that the total data length defined by a
                // Data Stage TD (i.e. the sum of the Length fields of the Data Stage TRB and all Normal
                // TRBs) is equal to wLength. Note that communicating with some non-compliant
                // devices may require violating this rule.
                if usb_request.data.len() < transfer_length as usize {
                    self.transfer_state.state = ControlTransferStage::ExpectStatusStageTrb;
                    self.transfer_state.event_meta.zero();
                    self.submission_state = ControlSubmissionState::ParserError(address); // TODO maybe protocol error?
                    return;
                }

                // All transfers are done but to have the expected value in the
                // created Events we keep count of pretend transfers.
                self.transfer_state.event_meta.add(transfer_length);

                let byte_slice: Vec<u8> = usb_request
                    .data
                    .drain(0..transfer_length as usize)
                    .collect();

                trace!(
                    "DataStage TRB len: {} slice: {:?}",
                    byte_slice.len(),
                    byte_slice
                );
                self.dma_bus.write_bulk(data_pointer, &byte_slice);
            }
            ControlTransferData::Out(usb_request) => {
                trace!("DMA for ControlOut");

                if immediate_data {
                    // Only event data should follow when immediate data is used here
                    // but we do not check for that and allow multiple immediate data
                    // TRB in the data stage TD.

                    // SAFETY: is always set in the preceding setup stage
                    usb_request.data.append(
                        &mut data_pointer.to_le_bytes()[..transfer_length as usize].to_vec(),
                    );
                } else {
                    let mut tmp = vec![0u8; transfer_length as usize];
                    self.dma_bus.read_bulk(data_pointer, &mut tmp);

                    // SAFETY: is always set in the preceding setup stage
                    usb_request.data.append(&mut tmp);
                }

                // No hardware transfer happened yet but we have to track from
                // the guest "successful transmitted" byte count for maybe
                // created Events to show the expected edtla.
                self.transfer_state.event_meta.add(transfer_length);
            }
        }
    }

    fn handle_data_stage_trb(&mut self, address: u64, trb: DataStageTrb) -> anyhow::Result<()> {
        trace!("DataStage TRB");

        self.data_slices(address, &trb);

        if trb.interrupt_on_completion {
            interrupt_on_completion(
                address,
                CompletionCode::Success,
                false,
                self.endpoint_id,
                self.slot_id,
                &self.event_sender,
            )?;
        }

        self.transfer_state.event_meta.previous_completion_code = CompletionCode::Success;

        if trb.chain {
            self.transfer_state.state = ControlTransferStage::MoreData;
        } else {
            self.transfer_state.state = ControlTransferStage::ExpectStatusStageTrb;
        }

        self.submission_state =
            ControlSubmissionState::ParserConsumedTrb(address, TransferTrbVariant::DataStage(trb));
        Ok(())
    }

    fn handle_normal_trb(&mut self, address: u64, trb: NormalTrb) -> anyhow::Result<()> {
        trace!("Normal TRB");

        self.data_slices(address, &trb);

        if trb.interrupt_on_completion {
            interrupt_on_completion(
                address,
                CompletionCode::Success,
                false,
                self.endpoint_id,
                self.slot_id,
                &self.event_sender,
            )?;
        }

        if !trb.chain {
            self.transfer_state.state = ControlTransferStage::ExpectStatusStageTrb;
        }

        self.submission_state =
            ControlSubmissionState::ParserConsumedTrb(address, TransferTrbVariant::Normal(trb));
        Ok(())
    }

    fn handle_status_stage_trb(&mut self, address: u64, trb: StatusStageTrb) -> anyhow::Result<()> {
        match &mut self.transfer_state.data {
            ControlTransferData::In(_) => {
                trace!("StatusStage TRB with ControlIn");

                if trb.interrupt_on_completion {
                    interrupt_on_completion(
                        address,
                        CompletionCode::Success,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                        &self.event_sender,
                    )?;
                }

                if trb.chain {
                    self.transfer_state.state = ControlTransferStage::ExpectFinalEventDataTrb;
                } else {
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
                    self.transfer_state.event_meta.zero();
                }

                self.submission_state = ControlSubmissionState::ParserConsumedTrb(
                    address,
                    TransferTrbVariant::StatusStage(trb),
                );
            }
            ControlTransferData::Out(usb_request_out) => {
                trace!("StatusStage TRB with ControlOut");

                self.real_ep
                    .submit_control_request(usb_request_out.clone())?;
                pcap::control_submission(self.pcap_meta, usb_request_out);

                self.submission_state = ControlSubmissionState::AwaitingControlOut(
                    address,
                    TransferTrbVariant::StatusStage(trb),
                );
            }
        }
        Ok(())
    }

    fn handle_status_stage_trb_hardware_response(
        &mut self,
        address: u64,
        trb: StatusStageTrb,
    ) -> anyhow::Result<()> {
        match &mut self.transfer_state.data {
            ControlTransferData::In(_) => {
                unreachable!("internal error: ControlIn requests do the Hardware request in the SetupStage; a submission state to arrive here should never be used");
            }
            ControlTransferData::Out(usb_request) => {
                trace!("StatusStage TRB with ControlOut");

                pcap::control_completion_out(
                    self.pcap_meta,
                    usb_request.address,
                    u32::from(usb_request.length),
                );

                if trb.chain {
                    self.transfer_state.state = ControlTransferStage::ExpectFinalEventDataTrb;
                } else {
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
                    self.transfer_state.event_meta.zero();
                }

                if trb.interrupt_on_completion {
                    interrupt_on_completion(
                        address,
                        CompletionCode::Success,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                        &self.event_sender,
                    )?;
                }
                Ok(())
            }
        }
    }

    fn handle_event_data_trb(&mut self, address: u64, trb: EventDataTrb) -> anyhow::Result<()> {
        trace!("EventData TRB");

        let event = EventTrb::new_transfer_event_trb(
            trb.event_data,
            self.transfer_state.event_meta.edtla,
            self.transfer_state.event_meta.previous_completion_code,
            true,
            self.endpoint_id,
            self.slot_id,
        );

        self.event_sender.send(event)?;
        self.transfer_state.event_meta.zero();

        // According to the spec Event Data shall always have the IOC bit set.
        // We handle the IOC bit with the same generic function as we do on TransferTrb's.
        if trb.interrupt_on_completion {
            interrupt_on_completion(
                address,
                CompletionCode::Success,
                false,
                self.endpoint_id,
                self.slot_id,
                &self.event_sender,
            )?;
        }

        if !trb.chain {
            match self.transfer_state.state {
                ControlTransferStage::MoreData => {
                    self.transfer_state.state = ControlTransferStage::ExpectStatusStageTrb;
                    self.transfer_state.event_meta.zero();
                }
                ControlTransferStage::ExpectFinalEventDataTrb => {
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
                    self.transfer_state.event_meta.zero();
                }
                _ => {
                    error!("driver did not provide a spec ocmpliant control transfer trb chain");
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
                    self.transfer_state.event_meta.zero();
                }
            }
        }

        self.submission_state =
            ControlSubmissionState::ParserConsumedTrb(address, TransferTrbVariant::EventData(trb));
        Ok(())
    }
}

/// Track communication between us and the host hardware.
#[derive(Debug, Default, Clone)]
enum ControlSubmissionState {
    #[default]
    NoTrbSubmitted,
    ParserConsumedTrb(u64, TransferTrbVariant),
    ParserError(u64),
    AwaitingControlIn(u64, TransferTrbVariant),
    AwaitingControlOut(u64, TransferTrbVariant),
}

impl<RCEH: RealControlEndpointHandle> EndpointHandle for ControlEndpointHandle<RCEH> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<TrbProcessingResult>> + Send + 'a>>;

    fn submit_trb(&mut self, trb: RawTrb) -> anyhow::Result<()> {
        let variant = TransferTrbVariant::parse(trb.buffer);

        if let TransferTrbVariant::Unrecognized(_, _) = &variant {
            // logging this is happening in next_completion()
            self.submission_state = ControlSubmissionState::ParserError(trb.address);
        }

        match &self.transfer_state.state {
            ControlTransferStage::ExpectSetupStageTrb => match variant {
                TransferTrbVariant::SetupStage(setup_stage) => {
                    self.handle_setup_stage_trb(trb.address, setup_stage)?;
                }
                other_trb => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        other_trb
                    );
                    // TODO maybe new state as dropped a invalid trb?
                    self.submission_state =
                        ControlSubmissionState::ParserConsumedTrb(trb.address, other_trb);
                }
            },
            ControlTransferStage::MaybeDataStageTrb => match variant {
                TransferTrbVariant::SetupStage(setup_stage) => {
                    info!(
                        "received Setup Stage TRB abort ongoing control transfer in favour of this new one"
                    );
                    self.handle_setup_stage_trb(trb.address, setup_stage)?;
                }
                TransferTrbVariant::DataStage(data_stage) => {
                    self.handle_data_stage_trb(trb.address, data_stage)?;
                }
                TransferTrbVariant::StatusStage(status_stage) => {
                    self.handle_status_stage_trb(trb.address, status_stage)?;
                }
                other_trb => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        other_trb
                    );
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
                    // TODO maybe new state as dropped a invalid trb?
                    self.submission_state =
                        ControlSubmissionState::ParserConsumedTrb(trb.address, other_trb);
                }
            },
            ControlTransferStage::MoreData => match variant {
                TransferTrbVariant::SetupStage(setup_stage) => {
                    info!(
                        "received Setup Stage TRB abort ongoing control transfer in favour of this new one"
                    );
                    self.handle_setup_stage_trb(trb.address, setup_stage)?;
                }
                TransferTrbVariant::Normal(normal) => {
                    self.handle_normal_trb(trb.address, normal)?;
                }
                TransferTrbVariant::EventData(event_data) => {
                    self.handle_event_data_trb(trb.address, event_data)?;
                }
                other_trb => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        other_trb
                    );
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
                    // TODO maybe new state as dropped a invalid trb?
                    self.submission_state =
                        ControlSubmissionState::ParserConsumedTrb(trb.address, other_trb);
                }
            },

            ControlTransferStage::ExpectStatusStageTrb => match variant {
                TransferTrbVariant::SetupStage(setup_stage) => {
                    info!(
                        "received Setup Stage TRB abort ongoing control transfer in favour of this new one"
                    );
                    self.handle_setup_stage_trb(trb.address, setup_stage)?;
                }
                TransferTrbVariant::StatusStage(status_stage) => {
                    self.handle_status_stage_trb(trb.address, status_stage)?;
                }
                other_trb => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        other_trb
                    );
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
                    // TODO maybe new state as dropped a invalid trb?
                    self.submission_state =
                        ControlSubmissionState::ParserConsumedTrb(trb.address, other_trb);
                }
            },
            ControlTransferStage::ExpectFinalEventDataTrb => match variant {
                TransferTrbVariant::SetupStage(setup_stage) => {
                    info!(
                        "received Setup Stage TRB abort ongoing control transfer in favour of this new one"
                    );
                    self.handle_setup_stage_trb(trb.address, setup_stage)?;
                }
                TransferTrbVariant::EventData(event_data) => {
                    self.handle_event_data_trb(trb.address, event_data)?;
                }
                other_trb => {
                    info!(
                        "invalid control transfer sequence; expected Setup Stage Trb, got: {:?}",
                        other_trb
                    );
                    self.transfer_state.state = ControlTransferStage::ExpectSetupStageTrb;
                    // TODO maybe new state as dropped a invalid trb?
                    self.submission_state =
                        ControlSubmissionState::ParserConsumedTrb(trb.address, other_trb);
                }
            },
        }

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let result = match self.submission_state.clone() {
                ControlSubmissionState::ParserConsumedTrb(address, variant) => {
                    trace!("consumed trb from address: {} as: {:?}", address, variant);
                    TrbProcessingResult::Ok
                }
                ControlSubmissionState::ParserError(address) => {
                    info!(
                        "Failed to parse Transfer Trb on Control Endpoint. slot {}",
                        self.slot_id
                    );
                    pcap::trb_error(self.pcap_meta, address);
                    let event = EventTrb::new_transfer_event_trb(
                        address,
                        0,
                        CompletionCode::TrbError,
                        false,
                        self.endpoint_id,
                        self.slot_id,
                    );
                    self.event_sender.send(event)?;
                    TrbProcessingResult::TrbError
                }
                ControlSubmissionState::AwaitingControlIn(address, variant) => {
                    let processing_result = self.real_ep.next_completion().await?;
                    match processing_result {
                        ControlRequestProcessingResult::SuccessfulControlIn(mut data) => {
                            let trb = match variant {
                                TransferTrbVariant::SetupStage(setup_stage) => setup_stage,
                                _ => unreachable!("internal error: never set this ControlSubmissionState besides with a SetupStage."),
                            };
                            self.handle_setup_stage_hardware_response(address, trb, &mut data)?;
                            TrbProcessingResult::Ok
                        }
                        ControlRequestProcessingResult::SuccessfulControlOut => unreachable!(
                            "internal error: never set AwaitingControlIn and received a SuccessfulControlOut."
                        ),
                        processing_error => {
                            let usb_request = match &self.transfer_state.data {
                                ControlTransferData::In(usb_request) => usb_request,
                                _ => unreachable!("internal error: never set AwaitingControlIn without a UsbRequest containing a ControlIn"),
                            };
                            pcap::control_in_error(self.pcap_meta, usb_request, &processing_error);
                            self.handle_processing_error(processing_error, address)?
                        }
                    }
                }
                ControlSubmissionState::AwaitingControlOut(address, variant) => {
                    let processing_result = self.real_ep.next_completion().await?;
                    match processing_result {
                        ControlRequestProcessingResult::SuccessfulControlIn(_) => {
                            unreachable!("internal error: never set AwaitingControlOut and receive a SuccessfulControlIn.")
                        }
                        ControlRequestProcessingResult::SuccessfulControlOut => {
                            let trb = match variant {
                                TransferTrbVariant::StatusStage(status_stage) => status_stage,
                                _ => unreachable!("internal error: never set this ControlSubmissionState besides with a StatusStage."),
                            };
                            self.handle_status_stage_trb_hardware_response(address, trb)?;
                            TrbProcessingResult::Ok
                        }
                        processing_error => {
                            let usb_request = match &self.transfer_state.data {
                                ControlTransferData::Out(usb_request) => usb_request,
                                _ => unreachable!("internal error: never set AwaitingControlOut without a UsbRequest containing a ControlOut"),
                            };
                            pcap::control_out_error(self.pcap_meta, usb_request, &processing_error);
                            self.handle_processing_error(processing_error, address)?
                        }
                    }
                }
                ControlSubmissionState::NoTrbSubmitted => {
                    unreachable!("internal error: Always set a different ControlSubmissionState in submit_trb().")
                }
            };
            self.submission_state = ControlSubmissionState::NoTrbSubmitted;

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
                TrbProcessingResult::Stall
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
                TrbProcessingResult::TransactionError
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
    address: &u64,
    event_data_trb: &EventDataTrb,
    event_meta: &mut EventDataTrbMetadata,
    endpoint_id: u8,
    slot_id: u8,
    event_sender: &EventSender,
) -> anyhow::Result<()> {
    trace!("EventData TRB on Normal Ep");

    let event = EventTrb::new_transfer_event_trb(
        event_data_trb.event_data,
        event_meta.edtla,
        event_meta.previous_completion_code,
        true,
        endpoint_id,
        slot_id,
    );

    event_sender.send(event)?;
    event_meta.zero();

    // It was not clear from the specification alone if the IOC bit is
    // actually intended for the above event or as this separate one.
    if event_data_trb.interrupt_on_completion {
        interrupt_on_completion(
            *address,
            CompletionCode::Success,
            false,
            endpoint_id,
            slot_id,
            event_sender,
        )?;
    }

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

        if trb.immediate_data {
            todo!("immediate data in a Normal Trb on a Normal Out Endpoint")
        }

        if !trb.chain {
            self.event_meta = EventDataTrbMetadata::default();
        }

        let mut data = vec![0; trb.transfer_length as usize];
        self.dma_bus.read_bulk(trb.data_pointer, &mut data);

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
            interrupt_on_completion(
                address,
                CompletionCode::Success,
                false,
                self.endpoint_id,
                self.slot_id,
                &self.event_sender,
            )?;
        }

        pcap::out_completion(self.pcap_meta, address, trb.transfer_length);

        self.event_meta.previous_completion_code = CompletionCode::Success;
        Ok(())
    }

    fn handle_event_data_trb(&mut self, address: u64, trb: EventDataTrb) -> anyhow::Result<()> {
        handle_event_data_trb_normal_ep(
            &address,
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
                self.handle_event_data_trb(trb.address, event_data)?;
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

                            TrbProcessingResult::Stall
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

                            TrbProcessingResult::TransactionError
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
pub struct InEndpointHandle<RIEH: RealInEndpointHandle> {
    slot_id: u8,
    endpoint_id: u8,
    pcap_meta: EndpointPcapMeta,
    real_ep: RIEH,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
    submission_state: NormalSubmissionState,
    /// Values we will need to handle an incoming Event Data Trb.
    event_meta: EventDataTrbMetadata,
}

impl<RIEH: RealInEndpointHandle> InEndpointHandle<RIEH> {
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
            submission_state: NormalSubmissionState::NoTrbSubmitted,
            event_meta: EventDataTrbMetadata::default(),
        }
    }

    fn handle_normal_trb_pre_hardware(
        &mut self,
        address: u64,
        trb: NormalTrb,
    ) -> anyhow::Result<()> {
        trace!("handle_normal_trb_pre_hardware In");

        if trb.immediate_data {
            todo!("immediate data in a Normal Trb on a Normal In Endpoint")
        }

        if !trb.chain {
            self.event_meta = EventDataTrbMetadata::default();
        }

        self.real_ep.submit(trb.transfer_length as usize)?;
        pcap::in_submission(self.pcap_meta, address, trb.transfer_length);

        self.submission_state = NormalSubmissionState::AwaitingRealTransfer(address, trb);
        self.event_meta.previous_completion_code = CompletionCode::Success;

        Ok(())
    }

    fn handle_normal_trb_post_hardware(
        &mut self,
        address: u64,
        trb: NormalTrb,
        hardware_data: Vec<u8>,
    ) -> anyhow::Result<()> {
        trace!("handle_normal_trb_post_hardware In");

        let completion_code: CompletionCode;

        // SACETY: in case the hardware_data.len() is bigger than u32 we take the u32 value regardless
        let dma_length: u32 = match hardware_data.len().cmp(&(trb.transfer_length as usize)) {
            Ordering::Less => {
                debug!("received less than requested");
                completion_code = CompletionCode::ShortPacket;
                // SAFETY: comparison with a u32, less will always fit in a u32
                hardware_data.len().try_into().unwrap()
            }
            Ordering::Equal => {
                debug!("received exactly as requested");
                completion_code = CompletionCode::Success;
                // SAFETY: comparison with a u32, equal will always fit in a u32
                hardware_data.len().try_into().unwrap()
            }
            Ordering::Greater => {
                error!(
                    "received more than requested; likely losing this data: {:?}",
                    &hardware_data[trb.transfer_length as usize..]
                );
                completion_code = CompletionCode::Success;
                // device responded with more than requested
                // idk where the overhead goes but we track the requested amount
                trb.transfer_length
            }
        };

        self.event_meta.add(dma_length);
        self.dma_bus
            .write_bulk(trb.data_pointer, &hardware_data[..dma_length as usize]);

        if trb.interrupt_on_completion {
            interrupt_on_completion(
                address,
                CompletionCode::Success,
                false,
                self.endpoint_id,
                self.slot_id,
                &self.event_sender,
            )?;
        }

        pcap::in_completion(self.pcap_meta, address, &hardware_data);
        self.event_meta.previous_completion_code = completion_code;

        Ok(())
    }

    fn handle_event_data_trb(&mut self, address: u64, trb: EventDataTrb) -> anyhow::Result<()> {
        handle_event_data_trb_normal_ep(
            &address,
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

impl<RIEH: RealInEndpointHandle> EndpointHandle for InEndpointHandle<RIEH> {
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
                self.handle_event_data_trb(trb.address, event_data)?;
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
            let result = match self.submission_state.clone() {
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
                    match self.real_ep.next_completion().await? {
                        InTrbProcessingResult::Disconnect => {
                            info!(
                                "Device has been disconnected. slot {} ep {}",
                                self.slot_id, self.endpoint_id
                            );
                            pcap::in_error(
                                self.pcap_meta,
                                address,
                                &InTrbProcessingResult::Disconnect,
                            );

                            let event = EventTrb::new_transfer_event_trb(
                                address,
                                0,
                                CompletionCode::UsbTransactionError,
                                false,
                                self.endpoint_id,
                                self.slot_id,
                            );
                            self.event_sender.send(event)?;

                            TrbProcessingResult::Disconnect
                        }
                        InTrbProcessingResult::Stall => {
                            debug!(
                                "Device Stall while waiting for hardware response. slot {} ep {}",
                                self.slot_id, self.endpoint_id
                            );
                            pcap::in_error(self.pcap_meta, address, &InTrbProcessingResult::Stall);

                            let event = EventTrb::new_transfer_event_trb(
                                address,
                                0,
                                CompletionCode::StallError,
                                false,
                                self.endpoint_id,
                                self.slot_id,
                            );
                            self.event_sender.send(event)?;

                            TrbProcessingResult::Stall
                        }
                        InTrbProcessingResult::TransactionError => {
                            info!("Transaction Error while waiting for hardware response. slot {} ep {}",
                                self.slot_id, self.endpoint_id);
                            pcap::in_error(
                                self.pcap_meta,
                                address,
                                &InTrbProcessingResult::TransactionError,
                            );

                            let event = EventTrb::new_transfer_event_trb(
                                address,
                                0,
                                CompletionCode::UsbTransactionError,
                                false,
                                self.endpoint_id,
                                self.slot_id,
                            );
                            self.event_sender.send(event)?;

                            TrbProcessingResult::TransactionError
                        }
                        InTrbProcessingResult::Success(data) => {
                            self.handle_normal_trb_post_hardware(address, normal, data)?;
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

impl<RIEH: RealInEndpointHandle> BaseEndpointHandle for InEndpointHandle<RIEH> {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.cancel().await })
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { self.real_ep.clear_halt().await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::device::xhci::interrupter::tests::testutils::DummyInterrupter;
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

    // will return  the requested length of bytes with a value of 42
    #[derive(Debug)]
    pub struct DummyRealControlEndpointReadStatic {
        data_length: u16,
        direction: bool,
    }
    impl DummyRealControlEndpointReadStatic {
        fn new() -> Self {
            Self {
                data_length: 0,
                direction: false,
            }
        }
    }

    impl RealControlEndpointHandle for DummyRealControlEndpointReadStatic {
        type TrbCompletionFuture<'a> = Pin<
            Box<dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a>,
        >;

        fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()> {
            // fake request is instantly submitted but we need to remember the direction for next_complete
            const IN: u8 = 0b10000000;
            self.direction = (request.request_type & IN) == IN;
            self.data_length = request.length;

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

    impl BaseEndpointHandle for DummyRealControlEndpointReadStatic {
        type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

        fn cancel(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }

        fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }
    }

    // Initialize test environment using the DummyRealControlEndpointReadStatic
    //
    // Use the ControlEndpointHandle to submit some TransferTrb.
    // Use the UnboundedReceiver to directly check events meant for a EventRing.
    fn init_control_endpoint_handle_test<T: RealControlEndpointHandle>(
        real_ep: T,
    ) -> (DummyInterrupter, ControlEndpointHandle<T>) {
        let pcap_usb_bus_number = 1;
        let pcap_meta = EndpointPcapMeta::control(pcap_usb_bus_number, SLOT_ID, ENDPOINT_ID);

        let dma_bus = Arc::new(DynamicBus::new());
        let dma_backing = vec![99; 2048];
        dma_bus
            .add(0x0, Arc::new(TestBusDevice::new(&dma_backing[..])))
            .expect("");

        let (event_sender, interrupter) = DummyInterrupter::new();

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
            init_control_endpoint_handle_test(DummyRealControlEndpointReadStatic::new());

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_idt()
            .with_ioc()
            .with_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_ioc()
            .with_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
            .build();

        let input_trb = vec![setup_stage, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this dummy hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
    }

    #[tokio::test]
    async fn submit_shortest_possible_control_in_request_with_data_stage() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(DummyRealControlEndpointReadStatic::new());

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_length(SETUP_WLENGTH)
            .with_idt()
            .with_ioc()
            .with_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER_1)
            .with_length(TRANSFER_LENGTH)
            .with_ioc()
            .with_type(TRB_TYPE_DATA_STAGE)
            .with_dir()
            .build();
        let status_stage = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_ioc()
            .with_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
            .build();

        let input_trb = vec![setup_stage, data_stage, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this dummy hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );
    }

    #[tokio::test]
    async fn submit_second_illegal_data_stage_trb() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(DummyRealControlEndpointReadStatic::new());

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_length(SETUP_WLENGTH)
            .with_idt()
            .with_ioc()
            .with_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage_1 = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER_1)
            .with_length(TRANSFER_LENGTH)
            .with_ch()
            .with_ioc()
            .with_type(TRB_TYPE_DATA_STAGE)
            .with_dir()
            .build();
        let data_stage_2 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_field(DMA_POINTER_1)
            .with_length(TRANSFER_LENGTH)
            .with_ioc()
            .with_type(TRB_TYPE_DATA_STAGE)
            .with_dir()
            .build();
        let status_stage = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_ioc()
            .with_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
            .build();

        control_endpoint
            .submit_trb(setup_stage)
            .expect("this dummy hardware request should never fail");
        assert_eq!(
            control_endpoint.next_completion().await.ok(),
            Some(TrbProcessingResult::Ok)
        );
        control_endpoint
            .submit_trb(data_stage_1)
            .expect("this dummy hardware request should never fail");
        assert_eq!(
            control_endpoint.next_completion().await.ok(),
            Some(TrbProcessingResult::Ok)
        );

        // It is expected to error before submit_trb().
        // Now we can not call next_completion() since nothing was submitted.
        control_endpoint
            .submit_trb(data_stage_2)
            .expect("this dummy hardware request should never fail");
        control_endpoint
            .submit_trb(status_stage)
            .expect("this dummy hardware request should never fail");

        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(interrupter.next_event(), None);
        assert_eq!(interrupter.next_event(), None);
    }

    #[tokio::test]
    async fn submit_control_in_request_with_event_data_at_the_end_of_the_data_stage() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(DummyRealControlEndpointReadStatic::new());

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_length(SETUP_WLENGTH)
            .with_idt()
            .with_ioc()
            .with_type(0x2)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER_1)
            .with_length(TRANSFER_LENGTH)
            .with_ch()
            .with_type(0x3)
            .with_dir()
            .build();
        let event_data = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ioc()
            .with_type(0x7)
            .with_dir()
            .build();
        let status_stage = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_ioc()
            .with_type(0x4)
            .with_dir()
            .build();

        let input_trb = vec![setup_stage, data_stage, event_data, status_stage];

        for trb in input_trb {
            control_endpoint
                .submit_trb(trb)
                .expect("this dummy hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(EVENT_DATA_FIELD, 512, true))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FOURTH_ADDRESS, 0, false))
        );
    }

    #[tokio::test]
    async fn submit_control_in_request_with_event_data_between_two_trb_of_the_data_td() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(DummyRealControlEndpointReadStatic::new());

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_length(SETUP_WLENGTH * 2)
            .with_idt()
            .with_ioc()
            .with_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_IN_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER_1)
            .with_length(TRANSFER_LENGTH)
            .with_ch()
            .with_type(TRB_TYPE_DATA_STAGE)
            .with_dir()
            .build();
        let event_data = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ch()
            .with_ioc()
            .with_type(TRB_TYPE_EVENT_DATA)
            .with_dir()
            .build();
        let normal = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_field(DMA_POINTER_2)
            .with_length(TRANSFER_LENGTH)
            .with_ioc()
            .with_type(TRB_TYPE_NORMAL)
            .build();
        let status_stage = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_ioc()
            .with_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
            .build();

        let input_trb = vec![setup_stage, data_stage, event_data, normal, status_stage];

        for trb in input_trb {
            control_endpoint
                .submit_trb(trb)
                .expect("this dummy hardware request should never fail");
            assert_eq!(
                control_endpoint
                    .next_completion()
                    .await
                    .expect("this dummy hardware request should never fail"),
                TrbProcessingResult::Ok
            );
        }

        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(EVENT_DATA_FIELD, 512, true))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FOURTH_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FIFTH_ADDRESS, 0, false))
        );
    }

    #[tokio::test]
    async fn submit_control_in_request_with_event_data_after_status_stage_trb() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(DummyRealControlEndpointReadStatic::new());

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_idt()
            .with_ioc()
            .with_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_ch()
            .with_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
            .build();
        let event_data = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ioc()
            .with_type(TRB_TYPE_EVENT_DATA)
            .with_dir()
            .build();

        let input_trb = vec![setup_stage, status_stage, event_data];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this dummy hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(EVENT_DATA_FIELD, 0, true))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );
    }

    // expecting to receive 0xda7a via an out request
    #[derive(Debug)]
    pub struct DummyRealControlEndpointExpectDataPattern {}
    impl DummyRealControlEndpointExpectDataPattern {
        fn new() -> Self {
            Self {}
        }
    }

    impl RealControlEndpointHandle for DummyRealControlEndpointExpectDataPattern {
        type TrbCompletionFuture<'a> = Pin<
            Box<dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a>,
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

    impl BaseEndpointHandle for DummyRealControlEndpointExpectDataPattern {
        type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

        fn cancel(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }

        fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn submit_control_out_request_with_data_stage_using_immediate_data() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(DummyRealControlEndpointExpectDataPattern::new());

        const DMA_POINTER: u64 = 0xeb8bda7a;
        const TRANSFER_LENGTH: u32 = 2;

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_OUT)
            .with_setup_length(SETUP_WLENGTH)
            .with_idt()
            .with_ioc()
            .with_type(TRB_TYPE_SETUP_STAGE)
            .with_byte(14, SETUP_TRANSFER_TYPE_OUT_DATA)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER)
            .with_length(TRANSFER_LENGTH)
            .with_ioc()
            .with_idt()
            .with_type(TRB_TYPE_DATA_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_ioc()
            .with_type(TRB_TYPE_STATUS_STAGE)
            .build();

        let input_trb = vec![setup_stage, data_stage, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this dummy hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );
    }

    #[tokio::test]
    async fn submitting_out_of_order_or_unfinished_sequence_does_not_prevent_the_following_valid_sequence_of_trb(
    ) {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(DummyRealControlEndpointReadStatic::new());

        let status_stage_out_of_order = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_ioc()
            .with_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
            .build();
        let setup_stage_incomplete_sequence = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_idt()
            .with_ioc()
            .with_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let setup_stage = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_idt()
            .with_ioc()
            .with_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let status_stage = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_ioc()
            .with_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
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
                .expect("this dummy hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        // The incomplete sequence (the second trb; a lone setup stage) is valid
        // and we expect an event.
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );

        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FOURTH_ADDRESS, 0, false))
        );
    }

    #[tokio::test]
    async fn submit_setup_stage_with_wrong_wlength() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(DummyRealControlEndpointReadStatic::new());

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            // system software made a mistake; should be TRANSFER_LENGTH*3
            .with_setup_length(SETUP_WLENGTH)
            .with_ioc()
            .with_type(TRB_TYPE_SETUP_STAGE)
            .build();
        let data_stage = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER_1)
            .with_length(TRANSFER_LENGTH)
            .with_ch()
            .with_ioc()
            .with_type(TRB_TYPE_DATA_STAGE)
            .with_dir()
            .build();
        // with the above mistake this trb is will fail
        let normal_1 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_field(DMA_POINTER_2)
            .with_length(TRANSFER_LENGTH)
            .with_ch()
            .with_ioc()
            .with_type(TRB_TYPE_NORMAL)
            .build();
        let normal_2 = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_field(DMA_POINTER_3)
            .with_length(TRANSFER_LENGTH)
            .with_ioc()
            .with_type(TRB_TYPE_NORMAL)
            .build();
        let status_stage = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_ioc()
            .with_type(TRB_TYPE_STATUS_STAGE)
            .with_dir()
            .build();

        let input_trb = vec![setup_stage, data_stage, normal_1, normal_2, status_stage];

        for trb in input_trb.clone() {
            control_endpoint
                .submit_trb(trb)
                .expect("this dummy hardware request should never fail");
            assert_eq!(
                control_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(THIRD_ADDRESS, 0, false))
        );
        assert_eq!(interrupter.next_event(), None);
        assert_eq!(interrupter.next_event(), None);
    }

    // expecting to receive 0xda7a via an out request
    #[derive(Debug)]
    pub struct DummyRealControlEndpointHardwareError {
        error: ControlRequestProcessingResult,
    }
    impl DummyRealControlEndpointHardwareError {
        fn new(error: ControlRequestProcessingResult) -> Self {
            Self { error }
        }
    }

    impl RealControlEndpointHandle for DummyRealControlEndpointHardwareError {
        type TrbCompletionFuture<'a> = Pin<
            Box<dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a>,
        >;

        fn submit_control_request(&mut self, _request: UsbRequest) -> anyhow::Result<()> {
            Ok(())
        }

        fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
            Box::pin(async { Ok(self.error.clone()) })
        }
    }

    impl BaseEndpointHandle for DummyRealControlEndpointHardwareError {
        type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

        fn cancel(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }

        fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn control_request_returns_hardware_disconnect() {
        let (mut interrupter, mut control_endpoint) = init_control_endpoint_handle_test(
            DummyRealControlEndpointHardwareError::new(ControlRequestProcessingResult::Disconnect),
        );

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_length(SETUP_WLENGTH)
            .with_ioc()
            .with_type(TRB_TYPE_SETUP_STAGE)
            .build();

        control_endpoint
            .submit_trb(setup_stage)
            .expect("this dummy hardware request should never fail");
        assert_eq!(
            control_endpoint.next_completion().await.ok(),
            Some(TrbProcessingResult::Disconnect)
        );

        assert_eq!(
            interrupter.next_event(),
            Some(EventTrb::new_transfer_event_trb(
                FIRST_ADDRESS,
                0,
                CompletionCode::UsbTransactionError,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );
    }

    #[tokio::test]
    async fn control_request_returns_hardware_stall() {
        let (mut interrupter, mut control_endpoint) = init_control_endpoint_handle_test(
            DummyRealControlEndpointHardwareError::new(ControlRequestProcessingResult::Stall),
        );

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_length(SETUP_WLENGTH)
            .with_ioc()
            .with_type(TRB_TYPE_SETUP_STAGE)
            .build();

        control_endpoint
            .submit_trb(setup_stage)
            .expect("this dummy hardware request should never fail");
        assert_eq!(
            control_endpoint.next_completion().await.ok(),
            Some(TrbProcessingResult::Stall)
        );

        assert_eq!(
            interrupter.next_event(),
            Some(EventTrb::new_transfer_event_trb(
                FIRST_ADDRESS,
                0,
                CompletionCode::StallError,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );
    }

    #[tokio::test]
    async fn control_request_returns_hardware_transaction_error() {
        let (mut interrupter, mut control_endpoint) =
            init_control_endpoint_handle_test(DummyRealControlEndpointHardwareError::new(
                ControlRequestProcessingResult::TransactionError,
            ));

        let setup_stage = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_setup_type(SETUP_BM_REQUEST_TYPE_IN)
            .with_setup_length(SETUP_WLENGTH)
            .with_ioc()
            .with_type(TRB_TYPE_SETUP_STAGE)
            .build();

        control_endpoint
            .submit_trb(setup_stage)
            .expect("this dummy hardware request should never fail");
        assert_eq!(
            control_endpoint.next_completion().await.ok(),
            Some(TrbProcessingResult::TransactionError)
        );

        assert_eq!(
            interrupter.next_event(),
            Some(EventTrb::new_transfer_event_trb(
                FIRST_ADDRESS,
                0,
                CompletionCode::UsbTransactionError,
                false,
                ENDPOINT_ID,
                SLOT_ID,
            ))
        );
    }

    // dummy for bulk in real endpoint returning `vec![42; requested length]`
    #[derive(Debug)]
    struct DummyRealInEndpoint {
        data_length: usize,
    }
    impl DummyRealInEndpoint {
        fn new() -> Self {
            Self { data_length: 0 }
        }
    }
    impl RealInEndpointHandle for DummyRealInEndpoint {
        type TrbCompletionFuture<'a> =
            Pin<Box<dyn Future<Output = anyhow::Result<InTrbProcessingResult>> + Send + 'a>>;

        fn submit(&mut self, data: usize) -> anyhow::Result<()> {
            self.data_length = data;
            Ok(())
        }

        fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
            Box::pin(async {
                let data = vec![42; self.data_length];
                let result = InTrbProcessingResult::Success(data);
                Ok(result)
            })
        }
    }
    impl BaseEndpointHandle for DummyRealInEndpoint {
        type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

        fn cancel(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }

        fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn submit_multi_trb_bulk_in_transfer_with_event_data() {
        const SLOT_ID: u8 = 1;
        const ENDPOINT_ID: u8 = 1;

        let pcap_usb_bus_number = 1;
        let pcap_meta = EndpointPcapMeta::bulk(pcap_usb_bus_number, SLOT_ID, ENDPOINT_ID);

        let real_ep = DummyRealInEndpoint::new();

        let dma_bus = Arc::new(DynamicBus::new());
        let dma_backing = vec![99; 2048];
        dma_bus
            .add(0x0, Arc::new(TestBusDevice::new(&dma_backing[..])))
            .expect("");

        let (event_sender, mut interrupter) = DummyInterrupter::new();

        let mut bulk_in_endpoint = InEndpointHandle::new(
            SLOT_ID,
            ENDPOINT_ID,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
        );

        let normal_1 = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_data_field(DMA_POINTER_1)
            .with_length(TRANSFER_LENGTH)
            .with_byte(11, 0x8) // remaining TD Size: 2048
            .with_ch()
            .with_ioc()
            .with_type(TRB_TYPE_NORMAL)
            .build();
        let normal_2 = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER_2)
            .with_length(TRANSFER_LENGTH)
            .with_byte(11, 0x6) // remaining TD Size: 1536
            .with_ch()
            .with_ioc()
            .with_type(TRB_TYPE_NORMAL)
            .build();
        let normal_3 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_field(DMA_POINTER_3)
            .with_length(TRANSFER_LENGTH)
            .with_byte(11, 0x4) // remaining TD Size: 1024
            .with_ch()
            .with_type(TRB_TYPE_NORMAL)
            .build();
        let event_data_1 = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ch()
            .with_ioc()
            .with_type(TRB_TYPE_EVENT_DATA)
            .with_dir()
            .build();
        let normal_4 = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_data_field(DMA_POINTER_4)
            .with_length(TRANSFER_LENGTH)
            .with_byte(11, 0x2) // remaining TD Size: 512
            .with_ch()
            .with_type(TRB_TYPE_NORMAL)
            .build();
        let event_data_2 = RawTrbBuilder::new(SIXTH_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ioc()
            .with_type(TRB_TYPE_EVENT_DATA)
            .with_dir()
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
                .expect("this dummy hardware request should never fail");
            assert_eq!(
                bulk_in_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(EVENT_DATA_FIELD, TRANSFER_LENGTH * 3, true))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FOURTH_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(EVENT_DATA_FIELD, TRANSFER_LENGTH, true))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(SIXTH_ADDRESS, 0, false))
        );
    }

    // dummy for bulk out real endpoint returning success while discarding the data
    #[derive(Debug)]
    struct DummyRealOutEndpoint {}
    impl DummyRealOutEndpoint {
        fn new() -> Self {
            Self {}
        }
    }
    impl RealOutEndpointHandle for DummyRealOutEndpoint {
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
    impl BaseEndpointHandle for DummyRealOutEndpoint {
        type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

        fn cancel(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }

        fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
            // nothing we want to do
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn submit_multi_trb_bulk_out_transfer_with_event_data() {
        const SLOT_ID: u8 = 1;
        const ENDPOINT_ID: u8 = 1;

        let pcap_usb_bus_number = 1;
        let pcap_meta = EndpointPcapMeta::bulk(pcap_usb_bus_number, SLOT_ID, ENDPOINT_ID);

        let real_ep = DummyRealOutEndpoint::new();

        let dma_bus = Arc::new(DynamicBus::new());
        let dma_backing = vec![99; 2048];
        dma_bus
            .add(0x0, Arc::new(TestBusDevice::new(&dma_backing[..])))
            .expect("");

        let (event_sender, mut interrupter) = DummyInterrupter::new();

        let mut bulk_out_endpoint = OutEndpointHandle::new(
            SLOT_ID,
            ENDPOINT_ID,
            pcap_meta,
            real_ep,
            dma_bus,
            event_sender,
        );

        let normal_1 = RawTrbBuilder::new(FIRST_ADDRESS)
            .with_data_field(DMA_POINTER_1)
            .with_length(TRANSFER_LENGTH)
            .with_byte(11, 0x8) // remaining TD Size: 2048
            .with_ch()
            .with_ioc()
            .with_type(TRB_TYPE_NORMAL)
            .build();
        let normal_2 = RawTrbBuilder::new(SECOND_ADDRESS)
            .with_data_field(DMA_POINTER_2)
            .with_length(TRANSFER_LENGTH)
            .with_byte(11, 0x6) // remaining TD Size: 1536
            .with_ch()
            .with_ioc()
            .with_type(TRB_TYPE_NORMAL)
            .build();
        let normal_3 = RawTrbBuilder::new(THIRD_ADDRESS)
            .with_data_field(DMA_POINTER_3)
            .with_length(TRANSFER_LENGTH)
            .with_byte(11, 0x4) // remaining TD Size: 1024
            .with_ch()
            .with_type(TRB_TYPE_NORMAL)
            .build();
        let event_data_1 = RawTrbBuilder::new(FOURTH_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ch()
            .with_ioc()
            .with_type(TRB_TYPE_EVENT_DATA)
            .build();
        let normal_4 = RawTrbBuilder::new(FIFTH_ADDRESS)
            .with_data_field(DMA_POINTER_4)
            .with_length(TRANSFER_LENGTH)
            .with_byte(11, 0x2) // remaining TD Size: 512
            .with_ch()
            .with_type(TRB_TYPE_NORMAL)
            .build();
        let event_data_2 = RawTrbBuilder::new(SIXTH_ADDRESS)
            .with_data_field(EVENT_DATA_FIELD)
            .with_ioc()
            .with_type(TRB_TYPE_EVENT_DATA)
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
                .expect("this dummy hardware request should never fail");
            assert_eq!(
                bulk_out_endpoint.next_completion().await.ok(),
                Some(TrbProcessingResult::Ok)
            );
        }

        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FIRST_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(SECOND_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(EVENT_DATA_FIELD, TRANSFER_LENGTH * 3, true))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(FOURTH_ADDRESS, 0, false))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(EVENT_DATA_FIELD, TRANSFER_LENGTH, true))
        );
        assert_eq!(
            interrupter.next_event(),
            Some(expected_event(SIXTH_ADDRESS, 0, false))
        );
    }
}
