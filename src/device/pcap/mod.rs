pub mod meta;
pub mod writer;

pub use meta::EndpointPcapMeta;
pub use writer::{Timestamp, UsbDirection, UsbEventType, UsbPcapManager, UsbTransferType};

use crate::device::pci::usbrequest::UsbRequest;

pub fn control_submission(meta: EndpointPcapMeta, req: &UsbRequest, event_timestamp: Timestamp) {
    let payload = req.data.as_deref().unwrap_or(&[]);
    writer::log_submission(
        meta,
        req.address,
        event_timestamp,
        u32::from(req.length),
        Some(writer::build_setup_bytes(req)),
        payload,
    );
}

pub fn control_completion_in(
    meta: EndpointPcapMeta,
    urb_id: u64,
    data: &[u8],
    event_timestamp: Timestamp,
) {
    writer::log_completion(meta, urb_id, event_timestamp, 0, data.len() as u32, data);
}

pub fn control_completion_out(
    meta: EndpointPcapMeta,
    urb_id: u64,
    len: u32,
    event_timestamp: Timestamp,
) {
    writer::log_completion(meta, urb_id, event_timestamp, 0, len, &[]);
}

pub fn in_submission(
    meta: EndpointPcapMeta,
    urb_id: u64,
    expected_len: u32,
    event_timestamp: Timestamp,
) {
    writer::log_submission(meta, urb_id, event_timestamp, expected_len, None, &[]);
}

pub fn in_completion(meta: EndpointPcapMeta, urb_id: u64, data: &[u8], event_timestamp: Timestamp) {
    writer::log_completion(meta, urb_id, event_timestamp, 0, data.len() as u32, data);
}

pub fn out_submission(
    meta: EndpointPcapMeta,
    urb_id: u64,
    payload: &[u8],
    expected_len: u32,
    event_timestamp: Timestamp,
) {
    writer::log_submission(meta, urb_id, event_timestamp, expected_len, None, payload);
}

pub fn out_completion(meta: EndpointPcapMeta, urb_id: u64, len: u32, event_timestamp: Timestamp) {
    writer::log_completion(meta, urb_id, event_timestamp, 0, len, &[]);
}

pub fn error(meta: EndpointPcapMeta, urb_id: u64, status: i32, event_timestamp: Timestamp) {
    writer::log_error(
        meta,
        urb_id,
        UsbEventType::Error,
        event_timestamp,
        status,
        None,
        &[],
    );
}
