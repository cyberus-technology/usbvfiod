use std::time::SystemTime;

const LINKTYPE_USB_LINUX: u32 = 189;
const PCAP_MAGIC: u32 = 0xa1b2c3d4;
const PCAP_MAJOR: u16 = 2;
const PCAP_MINOR: u16 = 4;
const SNAPLEN: u32 = 65_535;

/// Timestamp of a packet in seconds and microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Timestamp {
    pub seconds: u32,
    pub microseconds: u32,
}

impl From<SystemTime> for Timestamp {
    fn from(value: SystemTime) -> Self {
        let duration = value
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        Self {
            seconds: duration.as_secs() as u32,
            microseconds: duration.subsec_micros(),
        }
    }
}

/// Linux USB PCAP per-packet header fields (linktype 189).
///
/// The overall structure comes from
/// [the official documentation](https://www.tcpdump.org/linktypes/LINKTYPE_USB_LINUX.html), while
/// the detailed field semantics are intentionally not duplicated here.
/// All fields are written in little-endian order by `header_bytes`.
pub struct UsbPacketLinktypeHeader {
    pub id: u64,
    pub event_type: u8,
    pub transfer_type: u8,
    pub endpoint_address: u8,
    pub device_address: u8,
    pub bus_number: u16,
    pub setup_flag: u8,
    pub data_flag: u8,
    pub status: i32,
    pub urb_len: u32,
    pub data_len: u32,
    pub setup: [u8; 8],
}

impl UsbPacketLinktypeHeader {
    pub fn header_bytes(&self, timestamp: Timestamp) -> [u8; 48] {
        let mut header = [0u8; 48];
        header[0..8].copy_from_slice(&self.id.to_le_bytes());
        header[8] = self.event_type;
        header[9] = self.transfer_type;
        header[10] = self.endpoint_address;
        header[11] = self.device_address;
        header[12..14].copy_from_slice(&self.bus_number.to_le_bytes());
        header[14] = self.setup_flag;
        header[15] = self.data_flag;
        header[16..24].copy_from_slice(&(timestamp.seconds as i64).to_le_bytes());
        header[24..28].copy_from_slice(&(timestamp.microseconds as i32).to_le_bytes());
        header[28..32].copy_from_slice(&self.status.to_le_bytes());
        header[32..36].copy_from_slice(&self.urb_len.to_le_bytes());
        header[36..40].copy_from_slice(&self.data_len.to_le_bytes());
        header[40..48].copy_from_slice(&self.setup);
        header
    }
}

/// Build the PCAP global header bytes.
///
/// This is the fixed header written once at the start of every PCAP file.
/// The global header layout follows [the official PCAP spec](https://datatracker.ietf.org/doc/id/draft-gharris-opsawg-pcap-00.html#name-file-header);
/// detailed field descriptions are not repeated here.
pub fn pcap_global_header_bytes() -> [u8; 24] {
    let mut header = [0u8; 24];
    header[0..4].copy_from_slice(&PCAP_MAGIC.to_le_bytes());
    header[4..6].copy_from_slice(&PCAP_MAJOR.to_le_bytes());
    header[6..8].copy_from_slice(&PCAP_MINOR.to_le_bytes());
    header[8..12].copy_from_slice(&0u32.to_le_bytes());
    header[12..16].copy_from_slice(&0u32.to_le_bytes());
    header[16..20].copy_from_slice(&SNAPLEN.to_le_bytes());
    header[20..24].copy_from_slice(&LINKTYPE_USB_LINUX.to_le_bytes());
    header
}

/// Build a full PCAP record (record header + linktype header + payload).
///
/// This produces the bytes for a single PCAP record, which is one complete
/// packet entry in the file.
/// The record header structure is defined by [the official PCAP spec](https://datatracker.ietf.org/doc/id/draft-gharris-opsawg-pcap-00.html#name-packet-record),
/// and those field details are intentionally omitted here.
pub fn pcap_record_bytes(
    timestamp: Timestamp,
    meta: &UsbPacketLinktypeHeader,
    payload: &[u8],
) -> Vec<u8> {
    let link_header = meta.header_bytes(timestamp);
    let incl_len = (link_header.len() + payload.len()) as u32;
    let mut record = Vec::with_capacity(16 + link_header.len() + payload.len());
    record.extend_from_slice(&timestamp.seconds.to_le_bytes());
    record.extend_from_slice(&timestamp.microseconds.to_le_bytes());
    record.extend_from_slice(&incl_len.to_le_bytes());
    record.extend_from_slice(&incl_len.to_le_bytes());
    record.extend_from_slice(&link_header);
    record.extend_from_slice(payload);
    record
}
