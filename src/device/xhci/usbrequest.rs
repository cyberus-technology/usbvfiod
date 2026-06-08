//! Represents a USB control request.

/// Represents a USB control request.
///
/// See xhci specification chapter 6.4.1.2.
///
/// For additional documentation of the fields other than `address`, see Section "9.3 USB
/// Device Requests" in the USB 2.0 specification.
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct UsbRequest {
    /// The guest address of the Status Stage of this request.
    pub address: u64,
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
    pub data_pointer: Option<u64>,
    pub data: Option<Vec<u8>>,
}
