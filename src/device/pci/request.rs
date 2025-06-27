#[derive(Debug)]
pub struct Request {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    data: Option<u64>,
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
