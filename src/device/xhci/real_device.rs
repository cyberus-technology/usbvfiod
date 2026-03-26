use tokio_util::sync::CancellationToken;

use crate::device::xhci::real_endpoint_handle::{
    RealControlEndpointHandle, RealInEndpointHandle, RealOutEndpointHandle,
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
        write!(f, "{name}")
    }
}

pub trait RealDevice: Debug + Send + Sync + 'static {
    type RCEH: RealControlEndpointHandle;
    type RBIEH: RealInEndpointHandle;
    type RBOEH: RealOutEndpointHandle;
    type RIIEH: RealInEndpointHandle;
    type RIOEH: RealOutEndpointHandle;

    fn speed(&self) -> Option<Speed>;
    fn control_endpoint_handle(&self) -> Self::RCEH;
    fn bulk_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RBIEH;
    fn bulk_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RBOEH;
    fn interrupt_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RIIEH;
    fn interrupt_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RIOEH;
}

pub trait Identifier: Debug + Copy + Eq + Send + Sync + 'static {
    fn bus_device_numbers(self) -> (u8, u8);
}

impl Identifier for (u8, u8) {
    fn bus_device_numbers(self) -> (u8, u8) {
        self
    }
}

// A RealDevice trait coupled with bus and device number for identification.
//
// A real device alone might not be able to identify itself: An nusb device can
// only query information from the device; if the device has no unique serial
// number, then fields such as vendor id and product id are the best bet for
// identification. However, with two identical devices, the approach fails to
// uniquely identify the devices. IdentifiableRealDevice allows distinction of
// devices by storing the unique bus-/device-number combination.
#[derive(Debug)]
pub struct CompleteRealDevice<RD: RealDevice, ID: Identifier> {
    pub identifier: ID,
    pub real_device: RD,
    pub cancel: CancellationToken,
}

impl<RD: RealDevice, ID: Identifier> CompleteRealDevice<RD, ID> {
    pub fn new(identifier: ID, real_device: RD) -> Self {
        Self {
            identifier,
            real_device,
            cancel: CancellationToken::new(),
        }
    }
}
