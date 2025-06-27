#[derive(Debug)]
pub struct UsbRequest {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
    pub data: Option<u64>,
}

impl UsbRequest {
    /// Create a new instance without data.
    ///
    /// A request without data is packaged in two TRBs (a Setup Stage and a
    /// Status Stage).
    ///
    /// # Parameters
    ///
    /// For the parameters, see Section "9.3 USB Device Requests" in the USB
    /// 2.0 specification.
    pub const fn new(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Self {
        Self {
            request_type,
            request,
            value,
            index,
            length,
            data: None,
        }
    }

    /// Create a new instance data.
    ///
    /// A request with data is packaged in three TRBs (a Setup Stage, a Data
    /// Stage and a Status Stage). With data means that the request carries a
    /// pointer to a data buffer in guest memory.
    ///
    /// # Parameters
    ///
    /// For the parameters, see Section "9.3 USB Device Requests" in the USB
    /// 2.0 specification.
    pub const fn new_with_data(
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        length: u16,
        data: u64,
    ) -> Self {
        Self {
            request_type,
            request,
            value,
            index,
            length,
            data: Some(data),
        }
    }
}
