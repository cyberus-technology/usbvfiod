//! Abstraction of the Transfer Request Block of a USB3 Host (XHCI) controller.
//!
//! The specification is available
//! [here](https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf).

/// Represents a TRB that the XHCI controller can place on the event ring.
#[derive(Debug)]
pub enum EventTrb {
    //TransferEvent,
    CommandCompletionEvent(CommandCompletionEventTrbData),
    PortStatusChangeEvent(PortStatusChangeEventData),
    //BandwidthRequestEvent,
    //DoorbellEvent,
    //HostControllerEvent,
    //DeviceNotificationEvent,
    //MfIndexWrapEvent,
}

impl EventTrb {
    /// Generates the byte representation of the TRB.
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
            EventTrb::CommandCompletionEvent(data) => data.to_bytes(),
            EventTrb::PortStatusChangeEvent(data) => data.to_bytes(),
        };
        // set cycle bit
        trb_data[12] = (trb_data[12] & !0x1) | cycle_bit as u8;

        trb_data
    }
}

/// Stores the relevant data for a Command Completion Event.
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
    ) -> EventTrb {
        assert_ne!(
            0,
            command_trb_pointer & 0x0f,
            "command_trb_pointer has to be 16-byte-aligned."
        );
        assert_ne!(
            0,
            command_completion_parameter & 0xff000000,
            "command_completion_parameter has to be a 24-bit value."
        );
        EventTrb::CommandCompletionEvent(CommandCompletionEventTrbData {
            command_trb_pointer,
            command_completion_parameter,
            completion_code,
            slot_id,
        })
    }
}

impl CommandCompletionEventTrbData {
    fn to_bytes(&self) -> [u8; 16] {
        let command_completion_event_id = 33;
        let mut trb = [0; 16];

        trb[0..8].copy_from_slice(&self.command_trb_pointer.to_le_bytes());
        trb[8..11].copy_from_slice(&self.command_completion_parameter.to_le_bytes());
        trb[11] = self.completion_code as u8;
        trb[13] = command_completion_event_id << 2;
        trb[15] = self.slot_id;

        trb
    }
}

/// Stores the relevant data for a Port Status Change Event.
/// Do not use this struct directly, use EventTrb::new_port_status_change_event_trb
/// instead.
#[derive(Debug)]
pub struct PortStatusChangeEventData {
    port_id: u8,
}

impl EventTrb {
    /// Create a new Port Status Change Event TRB.
    /// The XHCI spec describes this structure in Section 6.4.2.3.
    ///
    /// # Parameters
    ///
    /// - `port_id`: The number of the root hub port that generated this
    ///   event.
    pub fn new_port_status_change_event_trb(port_id: u8) -> EventTrb {
        EventTrb::PortStatusChangeEvent(PortStatusChangeEventData { port_id })
    }
}

impl PortStatusChangeEventData {
    fn to_bytes(&self) -> [u8; 16] {
        let port_status_change_event_id = 34;
        let mut bytes = [0; 16];

        bytes[3] = self.port_id;
        bytes[11] = CompletionCode::Success as u8;
        bytes[13] = port_status_change_event_id << 2;

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
