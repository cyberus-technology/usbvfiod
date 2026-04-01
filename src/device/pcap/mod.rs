pub mod meta;
pub mod packet;

pub use meta::EndpointPcapMeta;
pub use packet::{Timestamp, UsbDirection, UsbEventType, UsbPcapManager, UsbTransferType};

use crate::device::xhci::usbrequest::UsbRequest;

pub fn control_submission(base: EndpointPcapMeta, req: &UsbRequest, event_timestamp: Timestamp) {
    let direction = if req.request_type & 0x80 == 0 {
        UsbDirection::HostToDevice
    } else {
        UsbDirection::DeviceToHost
    };
    let meta = EndpointPcapMeta { direction, ..base };
    packet::log_control_submission(
        meta,
        req,
        event_timestamp,
        req.data.as_deref().unwrap_or(&[]),
    );
}

pub fn control_completion_in(
    base: EndpointPcapMeta,
    urb_id: u64,
    data: &[u8],
    event_timestamp: Timestamp,
) {
    let meta = EndpointPcapMeta {
        direction: UsbDirection::DeviceToHost,
        ..base
    };
    packet::log_control_completion(meta, urb_id, event_timestamp, 0, data.len() as u32, data);
}

pub fn control_completion_out(
    base: EndpointPcapMeta,
    urb_id: u64,
    len: u32,
    event_timestamp: Timestamp,
) {
    let meta = EndpointPcapMeta {
        direction: UsbDirection::HostToDevice,
        ..base
    };
    packet::log_control_completion(meta, urb_id, event_timestamp, 0, len, &[]);
}

pub fn in_submission(
    base: EndpointPcapMeta,
    urb_id: u64,
    expected_len: u32,
    event_timestamp: Timestamp,
) {
    packet::log_submission(base, urb_id, event_timestamp, expected_len, None, &[]);
}

pub fn in_completion(base: EndpointPcapMeta, urb_id: u64, data: &[u8], event_timestamp: Timestamp) {
    packet::log_completion(base, urb_id, event_timestamp, 0, data.len() as u32, data);
}

pub fn out_submission(
    meta: EndpointPcapMeta,
    urb_id: u64,
    payload: &[u8],
    expected_len: u32,
    event_timestamp: Timestamp,
) {
    packet::log_submission(meta, urb_id, event_timestamp, expected_len, None, payload);
}

pub fn out_completion(meta: EndpointPcapMeta, urb_id: u64, len: u32, event_timestamp: Timestamp) {
    packet::log_completion(meta, urb_id, event_timestamp, 0, len, &[]);
}
