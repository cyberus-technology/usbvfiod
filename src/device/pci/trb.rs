use core::fmt;

/// Represents a TRB that the driver can place on the command ring.
#[derive(Debug)]
pub enum CommandTrb {
    EnableSlotCommand,
    DisableSlotCommand,
    AddressDeviceCommand,
    ConfigureEndpointCommand,
    EvaluateContextCommand,
    ResetEndpointCommand,
    StopEndpointCommand,
    SetTrDequeuePointerCommand,
    ResetDeviceCommand,
    ForceHeaderCommand,
    NoOpCommand,
    Link,
}

impl TryFrom<&[u8]> for CommandTrb {
    type Error = CommandTrbParseError;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        let slice_size = bytes.len();
        if slice_size != 16 {
            return Err(CommandTrbParseError::IncorrectSliceSize(slice_size));
        }
        let trb_type = bytes[13] >> 2;
        let command_trb = match trb_type {
            6 => CommandTrb::Link,
            9 => CommandTrb::EnableSlotCommand,
            10 => CommandTrb::DisableSlotCommand,
            11 => CommandTrb::AddressDeviceCommand,
            12 => CommandTrb::ConfigureEndpointCommand,
            13 => CommandTrb::EvaluateContextCommand,
            14 => CommandTrb::ResetEndpointCommand,
            15 => CommandTrb::StopEndpointCommand,
            16 => CommandTrb::SetTrDequeuePointerCommand,
            17 => CommandTrb::ResetDeviceCommand,
            18 => {
                return Err(CommandTrbParseError::UnsupportedOptionalCommand(
                    18,
                    "Force Event Command".to_string(),
                ));
            }
            19 => {
                return Err(CommandTrbParseError::UnsupportedOptionalCommand(
                    19,
                    "Negotiate Bandwidth Command".to_string(),
                ));
            }
            20 => {
                return Err(CommandTrbParseError::UnsupportedOptionalCommand(
                    20,
                    "Set Latency Tolerance Value Command".to_string(),
                ));
            }
            21 => {
                return Err(CommandTrbParseError::UnsupportedOptionalCommand(
                    21,
                    "Get Port Bandwidth Command".to_string(),
                ))
            }

            22 => CommandTrb::ForceHeaderCommand,
            23 => CommandTrb::NoOpCommand,
            trb_type => return Err(CommandTrbParseError::UnknownTrbType(trb_type)),
        };
        Ok(command_trb)
    }
}

#[derive(Debug)]
pub enum CommandTrbParseError {
    IncorrectSliceSize(usize),
    UnsupportedOptionalCommand(u8, String),
    UnknownTrbType(u8),
}

impl std::error::Error for CommandTrbParseError {}

impl fmt::Display for CommandTrbParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandTrbParseError::IncorrectSliceSize(size) => {
                write!(
                            f,
                            "A TRB always has a size of 16 bytes. Cannot parse TRB from a slice of {} bytes.",
                            size
                        )
            }
            CommandTrbParseError::UnsupportedOptionalCommand(trb_type, cmd_name) => {
                write!(
                    f,
                    "TRB type {} refers to \"{}\", which is optional and not supported.",
                    trb_type, cmd_name
                )
            }
            CommandTrbParseError::UnknownTrbType(trb_type) => {
                write!(f, "TRB type {} does not refer to any command.", trb_type)
            }
        }
    }
}
