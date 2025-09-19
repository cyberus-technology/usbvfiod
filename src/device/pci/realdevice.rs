use crate::device::bus::BusDeviceRef;

use super::{
    trb::{CompletionCode, TransferTrb},
    usbrequest::UsbRequest,
};
use std::fmt::{self, Debug};

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Full = 1,
    Low = 2,
    High = 3,
    Super = 4,
    SuperPlus = 5,
}

impl Speed {
    pub const fn is_usb2_speed(self) -> bool {
        self as u8 <= 3
    }
}

impl fmt::Display for Speed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Low => "Low Speed (1.5 Mbps)",
            Self::Full => "Full Speed (12 Mbps)",
            Self::High => "High Speed (480 Mbps)",
            Self::Super => "SuperSpeed (5 Gbps)",
            Self::SuperPlus => "SuperSpeed+ (10/20 Gbps)",
        };
        write!(f, "{}", name)
    }
}

pub trait RealDevice: Debug {
    fn speed(&self) -> Option<Speed>;
    fn control_transfer(&self, request: &UsbRequest, dma_bus: &BusDeviceRef);
    fn enable_endpoint(&mut self, endpoint_id: u8);
    fn transfer_out(
        &mut self,
        endpoint_id: u8,
        trb: &TransferTrb,
        dma_bus: &BusDeviceRef,
    ) -> (CompletionCode, u32);
    fn transfer_in(
        &mut self,
        endpoint_id: u8,
        trb: &TransferTrb,
        dma_bus: &BusDeviceRef,
    ) -> (CompletionCode, u32);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointType {
    Control,
    BulkIn,
    BulkOut,
    InterruptIn,
}
