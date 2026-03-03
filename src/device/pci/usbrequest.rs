//! Represents a USB control request.

/// Represents a USB control request.
///
/// For documentation of the fields other than `address`, see Section "9.3 USB
/// Device Requests" in the USB 2.0 specification.
///
/// A request without data is packaged in two TRBs (a Setup Stage and a
/// Status Stage). `data` should then be `None`.
///
/// A request with data is packaged in three TRBs (a Setup Stage, a Data
/// Stage and a Status Stage). `data` should then contain the pointer
/// from the Data Stage).
///
#[derive(Debug, PartialEq, Eq)]
pub struct UsbRequest {
    /// The guest address of the Status Stage of this request.
    pub address: u64,
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
    pub data: Option<u64>,
}
