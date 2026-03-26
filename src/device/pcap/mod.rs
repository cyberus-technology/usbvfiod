pub mod meta;
pub mod writer;

pub use meta::EndpointPcapMeta;
pub use writer::{Timestamp, UsbDirection, UsbEventType, UsbPcapManager, UsbTransferType};

use crate::device::{pci::usbrequest::UsbRequest, xhci::endpoint_handle::TrbProcessingResult};

fn control_submission(meta: EndpointPcapMeta, req: &UsbRequest, event_timestamp: Timestamp) {
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

pub fn control_submission_with_req(
    base: EndpointPcapMeta,
    req: &UsbRequest,
    event_timestamp: Timestamp,
) {
    let direction = if req.request_type & 0x80 == 0 {
        UsbDirection::HostToDevice
    } else {
        UsbDirection::DeviceToHost
    };
    let meta = EndpointPcapMeta { direction, ..base };
    control_submission(meta, req, event_timestamp);
}

fn control_completion_in(
    meta: EndpointPcapMeta,
    urb_id: u64,
    data: &[u8],
    event_timestamp: Timestamp,
) {
    writer::log_completion(meta, urb_id, event_timestamp, 0, data.len() as u32, data);
}

pub fn control_completion_in_with_meta(
    base: EndpointPcapMeta,
    urb_id: u64,
    data: &[u8],
    event_timestamp: Timestamp,
) {
    let meta = EndpointPcapMeta {
        direction: UsbDirection::DeviceToHost,
        ..base
    };
    control_completion_in(meta, urb_id, data, event_timestamp);
}

fn control_completion_out(
    meta: EndpointPcapMeta,
    urb_id: u64,
    len: u32,
    event_timestamp: Timestamp,
) {
    writer::log_completion(meta, urb_id, event_timestamp, 0, len, &[]);
}

pub fn control_completion_out_with_meta(
    base: EndpointPcapMeta,
    urb_id: u64,
    len: u32,
    event_timestamp: Timestamp,
) {
    let meta = EndpointPcapMeta {
        direction: UsbDirection::HostToDevice,
        ..base
    };
    control_completion_out(meta, urb_id, len, event_timestamp);
}

pub fn in_submission(
    meta: EndpointPcapMeta,
    urb_id: u64,
    expected_len: u32,
    event_timestamp: Timestamp,
) {
    writer::log_submission(meta, urb_id, event_timestamp, expected_len, None, &[]);
}

pub fn in_submission_with_meta(
    base: EndpointPcapMeta,
    urb_id: u64,
    expected_len: u32,
    event_timestamp: Timestamp,
) {
    let meta = EndpointPcapMeta {
        direction: UsbDirection::DeviceToHost,
        ..base
    };
    in_submission(meta, urb_id, expected_len, event_timestamp);
}

pub fn in_completion(meta: EndpointPcapMeta, urb_id: u64, data: &[u8], event_timestamp: Timestamp) {
    writer::log_completion(meta, urb_id, event_timestamp, 0, data.len() as u32, data);
}

pub fn in_completion_with_meta(
    base: EndpointPcapMeta,
    urb_id: u64,
    data: &[u8],
    event_timestamp: Timestamp,
) {
    let meta = EndpointPcapMeta {
        direction: UsbDirection::DeviceToHost,
        ..base
    };
    in_completion(meta, urb_id, data, event_timestamp);
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

fn map_trb_error_status(result: TrbProcessingResult) -> i32 {
    match result {
        TrbProcessingResult::Stall => -32,            // EPIPE
        TrbProcessingResult::TransactionError => -71, // EPROTO
        TrbProcessingResult::Disconnect => -19,       // ENODEV
        TrbProcessingResult::TrbError => -5,          // EIO
        TrbProcessingResult::Ok => 0,
    }
}

pub fn error_with_meta(
    base: EndpointPcapMeta,
    urb_id: u64,
    result: TrbProcessingResult,
    event_timestamp: Timestamp,
) {
    let status = map_trb_error_status(result);
    if status == 0 {
        return;
    }
    error(base, urb_id, status, event_timestamp);
}
