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
    /// The Setup Stage TRB's wLength field is supposed to be used to communicate
    /// the sum of all transfer_length fields following in the Data TD. It is not
    /// guaranteed, so we cannot use it. We still use the wLength fields size
    /// of 16 bit to assume a maximum length for any control request.
    pub length: u16,
    pub data_pointer: Option<u64>,
    pub data: Vec<u8>,
}
