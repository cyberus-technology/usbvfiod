//! Abstraction of the Transfer Request Block of a USB3 Host (XHCI) controller.
//!
//! The specification is available
//! [here](https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf).

use super::constants::xhci::rings::{
    trb_types::{self, *},
    TRB_SIZE,
};
use core::fmt;

/// Represents a TRB that the XHCI controller can place on the event ring.
#[derive(Debug)]
pub enum EventTrb {
    //TransferEvent,
    CommandCompletionEvent(CommandCompletionEventTrbData),
    PortStatusChangeEvent(PortStatusChangeEventTrbData),
    //BandwidthRequestEvent,
    //DoorbellEvent,
    //HostControllerEvent,
    //DeviceNotificationEvent,
    //MfIndexWrapEvent,
}

impl EventTrb {
    /// Generates the byte representation of the TRB.
    ///
    /// The cycle bit's value does not depend on the TRB but on the ring that
    /// the TRB will be placed on.
    ///
    /// # Parameters
    ///
    /// - `cycle_bit`: value to set the cycle bit to. Has to match the ring
    ///   where the caller will write the TRB on.
    pub fn to_bytes(&self, cycle_bit: bool) -> [u8; 16] {
        // layout the event-type-specific data
        let mut trb_data = match self {
            Self::CommandCompletionEvent(data) => data.to_bytes(),
            Self::PortStatusChangeEvent(data) => data.to_bytes(),
        };
        // set cycle bit
        trb_data[12] = (trb_data[12] & !0x1) | cycle_bit as u8;

        trb_data
    }
}

/// Stores the relevant data for a Command Completion Event.
///
/// Do not use this struct directly, use EventTrb::new_command_completion_event_trb
/// instead.
#[derive(Debug)]
pub struct CommandCompletionEventTrbData {
    command_trb_pointer: u64,
    command_completion_parameter: u32,
    completion_code: CompletionCode,
    slot_id: u8,
}

impl EventTrb {
    /// Create a new Command Completion Event TRB.
    ///
    /// The XHCI spec describes this structure in Section 6.4.2.2.
    ///
    /// # Parameters
    ///
    /// - `command_trb_pointer`: 64-bit address of the Command TRB that
    ///   generated this event. The address has to be 16-byte-aligned, so the
    ///   lowest four bit have to be 0.
    /// - `command_completion_parameter`: Depends on the associated command.
    ///   This is a 24-bit value, so the highest eight bit are ignored.
    /// - `completion_code`: Encodes the completion status of the associated
    ///   command.
    /// - `slot_id`: The slot associated with command that generated this
    ///   event.
    #[allow(unused)]
    pub fn new_command_completion_event_trb(
        command_trb_pointer: u64,
        command_completion_parameter: u32,
        completion_code: CompletionCode,
        slot_id: u8,
    ) -> Self {
        assert_eq!(
            0,
            command_trb_pointer & 0x0f,
            "command_trb_pointer has to be 16-byte-aligned."
        );
        assert_eq!(
            0,
            command_completion_parameter & 0xff000000,
            "command_completion_parameter has to be a 24-bit value."
        );
        Self::CommandCompletionEvent(CommandCompletionEventTrbData {
            command_trb_pointer,
            command_completion_parameter,
            completion_code,
            slot_id,
        })
    }
}

impl CommandCompletionEventTrbData {
    fn to_bytes(&self) -> [u8; 16] {
        let mut trb = [0; 16];

        trb[0..8].copy_from_slice(&self.command_trb_pointer.to_le_bytes());
        trb[8..11].copy_from_slice(&self.command_completion_parameter.to_le_bytes()[0..3]);
        trb[11] = self.completion_code as u8;
        trb[13] = COMMAND_COMPLETION_EVENT << 2;
        trb[15] = self.slot_id;

        trb
    }
}

/// Stores the relevant data for a Port Status Change Event.
///
/// Do not use this struct directly, use EventTrb::new_port_status_change_event_trb
/// instead.
#[derive(Debug)]
pub struct PortStatusChangeEventTrbData {
    port_id: u8,
}

impl EventTrb {
    /// Create a new Port Status Change Event TRB.
    ///
    /// The XHCI spec describes this structure in Section 6.4.2.3.
    ///
    /// # Parameters
    ///
    /// - `port_id`: The number of the root hub port that generated this
    ///   event.
    pub const fn new_port_status_change_event_trb(port_id: u8) -> Self {
        Self::PortStatusChangeEvent(PortStatusChangeEventTrbData { port_id })
    }
}

impl PortStatusChangeEventTrbData {
    const fn to_bytes(&self) -> [u8; 16] {
        let mut bytes = [0; 16];

        bytes[3] = self.port_id;
        bytes[11] = CompletionCode::Success as u8;
        bytes[13] = PORT_STATUS_CHANGE_EVENT << 2;

        bytes
    }
}

/// Encodes the completion code that some event TRBs contain.
#[allow(dead_code)]
#[derive(Debug, Copy, Clone)]
pub enum CompletionCode {
    Invalid = 0,
    Success,
    DataBufferError,
    BabbleDetectedError,
    UsbTransactionError,
    TrbError,
    StallError,
    ResourceError,
    BandwidthError,
    NoSlotsAvailableError,
    InvalidStreamTypeError,
    SlotNotEnabledError,
    EndpointNotEnabledError,
    ShortPacket,
    RingUnderrun,
    RingOverrun,
    VfEventRingFullError,
    ParameterError,
    BandwidthOverrunError,
    ContextStateError,
    NoPingResponseError,
    EventRingFullError,
    IncompatibleDeviceError,
    MissedServiceError,
    CommandRingStopped,
    CommandAborted,
    Stopped,
    StoppedLengthInvalid,
    StoppedShortedPacket,
    MaxExitLatencyTooLargeError,
    Reserved,
    IsochBufferOverrun,
    EventLostError,
    UndefinedError,
    InvalidStreamIdError,
    SecondaryBandwidthError,
    SplitTransactionError,
}

/// Represents a TRB that the driver can place on the command ring.
#[derive(Debug)]
pub enum CommandTrb {
    EnableSlotCommand,
    DisableSlotCommand,
    AddressDeviceCommand(AddressDeviceCommandTrbData),
    ConfigureEndpointCommand,
    EvaluateContextCommand,
    ResetEndpointCommand,
    StopEndpointCommand,
    SetTrDequeuePointerCommand,
    ResetDeviceCommand,
    ForceHeaderCommand,
    NoOpCommand,
    Link(LinkTrbData),
}

impl TryFrom<&[u8]> for CommandTrb {
    type Error = TrbParseError;

    /// Try to parse a TRB from a byte slice.
    ///
    /// # Limitations
    ///
    /// While this function can parse all available Command TRB types, it does
    /// not parse all of them in full detail. If the function returns only the
    /// enum variant without an associated struct, the parsing for the
    /// particular command is not yet implemented. EnableSlotCommand is an
    /// exception, because the TRB does not contain any additional information.
    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        let slice_size = bytes.len();
        if slice_size != TRB_SIZE {
            return Err(TrbParseError::IncorrectSliceSize(slice_size));
        }
        let trb_type = bytes[13] >> 2;
        let command_trb = match trb_type {
            trb_types::LINK => CommandTrb::Link(LinkTrbData::parse(bytes)?),
            // EnableSlotCommand does not contain information apart from the
            // type; thus, no further parsing is necessary and we can just
            // return the enum variant.
            trb_types::ENABLE_SLOT_COMMAND => CommandTrb::EnableSlotCommand,
            trb_types::DISABLE_SLOT_COMMAND => CommandTrb::DisableSlotCommand,
            trb_types::ADDRESS_DEVICE_COMMAND => {
                CommandTrb::AddressDeviceCommand(AddressDeviceCommandTrbData::parse(bytes)?)
            }
            trb_types::CONFIGURE_ENDPOINT_COMMAND => CommandTrb::ConfigureEndpointCommand,
            trb_types::EVALUATE_CONTEXT_COMMAND => CommandTrb::EvaluateContextCommand,
            trb_types::RESET_ENDPOINT_COMMAND => CommandTrb::ResetEndpointCommand,
            trb_types::STOP_ENDPOINT_COMMAND => CommandTrb::StopEndpointCommand,
            trb_types::SET_TR_DEQUEUE_POINTER_COMMAND => CommandTrb::SetTrDequeuePointerCommand,
            trb_types::RESET_DEVICE_COMMAND => CommandTrb::ResetDeviceCommand,
            trb_types::FORCE_EVENT_COMMAND => {
                return Err(TrbParseError::UnsupportedOptionalCommand(
                    18,
                    "Force Event Command".to_string(),
                ));
            }
            trb_types::NEGOTIATE_BANDWIDTH_COMMAND => {
                return Err(TrbParseError::UnsupportedOptionalCommand(
                    19,
                    "Negotiate Bandwidth Command".to_string(),
                ));
            }
            trb_types::SET_LATENCY_TOLERANCE_VALUE_COMMAND => {
                return Err(TrbParseError::UnsupportedOptionalCommand(
                    20,
                    "Set Latency Tolerance Value Command".to_string(),
                ));
            }
            trb_types::GET_PORT_BANDWIDTH_COMMAND => {
                return Err(TrbParseError::UnsupportedOptionalCommand(
                    21,
                    "Get Port Bandwidth Command".to_string(),
                ))
            }

            trb_types::FORCE_HEADER_COMMAND => CommandTrb::ForceHeaderCommand,
            trb_types::NO_OP_COMMAND => CommandTrb::NoOpCommand,
            trb_type => return Err(TrbParseError::UnknownTrbType(trb_type)),
        };
        Ok(command_trb)
    }
}

/// Custom error type to represent errors in TRB parsing.
#[derive(Debug)]
pub struct LinkTrbData {
    /// The address of the next ring segment.
    pub ring_segment_pointer: u64,
    /// The flag that indicates whether to toggle the cycle bit.
    pub toggle_cycle: bool,
}

impl LinkTrbData {
    /// Parse data of a Link TRB.
    ///
    /// Only `CommandTrb::try_from` should call this function. Thus, we make
    /// the following assumptions to avoid duplicate checks:
    ///
    /// - `value` is a slice of size 16.
    /// - The TRB type (upper 6 bit of byte 13) indicate a link TRB.
    ///
    /// # Limitations
    ///
    /// The function currently does not check if the slice respects all RsvdZ
    /// fields.
    fn parse(trb_bytes: &[u8]) -> Result<Self, TrbParseError> {
        let rsp_bytes: [u8; 8] = trb_bytes[0..8].try_into().unwrap();
        let ring_segment_pointer = u64::from_le_bytes(rsp_bytes);
        let toggle_cycle = trb_bytes[12] & 0x2 != 0;

        // the lowest for bit of the pointer are RsvdZ to ensure 16-byte
        // alignment.
        if ring_segment_pointer & 0xf != 0 {
            return Err(TrbParseError::RsvdZViolation);
        }

        Ok(LinkTrbData {
            ring_segment_pointer,
            toggle_cycle,
        })
    }
}

#[derive(Debug)]
pub struct AddressDeviceCommandTrbData {
    /// The address of the input context.
    pub input_context_pointer: u64,
    /// The flag that indicates whether to send a USB SET_ADDRESS request to the
    /// device.
    pub block_set_address_request: bool,
    /// The associated Slot ID
    pub slot_id: u8,
}

impl AddressDeviceCommandTrbData {
    /// Parse data of a Address Device Command TRB.
    ///
    /// Only `CommandTrb::try_from` should call this function. Thus, we make
    /// the following assumptions to avoid duplicate checks:
    ///
    /// - `value` is a slice of size 16.
    /// - The TRB type (upper 6 bit of byte 13) indicate a link TRB.
    ///
    /// # Limitations
    ///
    /// The function currently does not check if the slice respects all RsvdZ
    /// fields.
    fn parse(trb_bytes: &[u8]) -> Result<Self, TrbParseError> {
        let icp_bytes: [u8; 8] = trb_bytes[0..8].try_into().unwrap();
        let input_context_pointer = u64::from_le_bytes(icp_bytes);

        // the lowest for bit of the pointer are RsvdZ to ensure 16-byte
        // alignment.
        if input_context_pointer & 0xf != 0 {
            return Err(TrbParseError::RsvdZViolation);
        }

        let block_set_address_request = trb_bytes[13] & 0x2 != 0;
        let slot_id = trb_bytes[15];

        Ok(AddressDeviceCommandTrbData {
            input_context_pointer,
            block_set_address_request,
            slot_id,
        })
    }
}

#[derive(Debug)]
pub enum TrbParseError {
    IncorrectSliceSize(usize),
    UnsupportedOptionalCommand(u8, String),
    UnknownTrbType(u8),
    RsvdZViolation,
}

impl std::error::Error for TrbParseError {}

impl fmt::Display for TrbParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrbParseError::IncorrectSliceSize(size) => {
                write!(
                            f,
                            "Cannot parse TRB from a slice of {} bytes. A TRB always has a size of 16 bytes.",
                            size
                        )
            }
            TrbParseError::UnsupportedOptionalCommand(trb_type, cmd_name) => {
                write!(
                    f,
                    "TRB type {} refers to \"{}\", which is optional and not supported.",
                    trb_type, cmd_name
                )
            }
            TrbParseError::UnknownTrbType(trb_type) => {
                write!(f, "TRB type {} does not refer to any command.", trb_type)
            }
            TrbParseError::RsvdZViolation => {
                write!(f, "Detected a non-zero value in a RsvdZ field")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_enable_slot_command_trb() {
        let trb_bytes = [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x24,
            0x00, 0x00,
        ];
        let trb_result = CommandTrb::try_from(&trb_bytes[..]);
        assert!(
            trb_result.is_ok(),
            "A valid TRB byte representation should be parsed successfully."
        );
        let trb = trb_result.unwrap();
        if let CommandTrb::EnableSlotCommand = trb {
        } else {
            panic!(
                "A TRB with TRB type 9 should result in a CommandTrb::EnableSlotCommand. Got instead: {:?}",
                trb
            );
        }
    }

    #[test]
    fn test_parse_link_trb() {
        let trb_bytes = [
            0x80, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0x00, 0x00, 0x00, 0x02, 0x18,
            0x00, 0x00,
        ];
        let trb_result = CommandTrb::try_from(&trb_bytes[..]);
        assert!(
            trb_result.is_ok(),
            "A valid TRB byte representation should be parsed successfully."
        );
        let trb = trb_result.unwrap();
        if let CommandTrb::Link(link_data) = trb {
            assert_eq!(
                0x1122334455667780, link_data.ring_segment_pointer,
                "link_segment_pointer was parsed incorrectly."
            );
            assert!(
                link_data.toggle_cycle,
                "toggle_cycle bit was parsed incorrectly."
            );
        } else {
            panic!(
                "A TRB with TRB type 6 should result in a CommandTrb::Link. Got instead: {:?}",
                trb
            );
        }
    }

    #[test]
    fn test_parse_address_device_command_trb() {
        let trb_bytes = [
            0x80, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0x00, 0x00, 0x00, 0x02, 0x2e,
            0x00, 0x13,
        ];
        let trb_result = CommandTrb::try_from(&trb_bytes[..]);
        assert!(
            trb_result.is_ok(),
            "A valid TRB byte representation should be parsed successfully."
        );
        let trb = trb_result.unwrap();
        if let CommandTrb::AddressDeviceCommand(data) = trb {
            assert_eq!(
                0x1122334455667780, data.input_context_pointer,
                "input_context_pointer was parsed incorrectly."
            );
            assert!(
                data.block_set_address_request,
                "BSR bit was parsed incorrectly."
            );
            assert_eq!(0x13, data.slot_id, "slot_id was parsed incorrectly.");
        } else {
            panic!(
                "A TRB with TRB type 11 should result in a CommandTrb::AddressDeviceCommand. Got instead: {:?}",
                trb
            );
        }
    }

    #[test]
    fn test_command_completion_event_trb() {
        let trb = EventTrb::new_command_completion_event_trb(
            0x1122334455667780,
            0xaabbcc,
            CompletionCode::Success,
            2,
        );
        assert_eq!(
            [
                0x80, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0xcc, 0xbb, 0xaa, 0x01, 0x01, 0x84,
                0x00, 0x02,
            ],
            trb.to_bytes(true),
        )
    }

    #[test]
    fn test_port_status_change_event_trb() {
        let trb = EventTrb::new_port_status_change_event_trb(2);
        assert_eq!(
            [
                0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x88,
                0x00, 0x00,
            ],
            trb.to_bytes(true),
        )
    }
}
