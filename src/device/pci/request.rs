#[derive(Debug)]
pub struct Request {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
    pub data: Option<u64>,
}

impl Request {
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
