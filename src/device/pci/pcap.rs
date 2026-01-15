#![allow(dead_code)]

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use tracing::warn;

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

/// Opens the file and emits the global header on first use.
///
/// This keeps capture formatting pure while allowing optional file output.
/// On the first successful write, the parent directory is created (if needed),
/// the file is opened, and the PCAP global header is written. Any subsequent
/// I/O errors only disable PCAP logging and emit a warning; they do not stop
/// the overall process.
///
/// The file and header layout are based on the official PCAP specification,
/// so per-field details are not duplicated in this comment.
pub struct PcapManager {
    path: Option<PathBuf>,
    writer: Option<BufWriter<File>>,
    warned: bool,
}

impl PcapManager {
    pub const fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            writer: None,
            warned: false,
        }
    }

    fn ensure_writer(&mut self) -> Option<&mut BufWriter<File>> {
        let file_path = self.path.clone()?;

        if self.writer.is_some() {
            return self.writer.as_mut();
        }

        if let Some(parent) = file_path.parent() {
            if let Err(error) = fs::create_dir_all(parent) {
                if !self.warned {
                    warn!(
                        "Disabling USB PCAP logging after failing to create {}: {}",
                        parent.display(),
                        error
                    );
                    self.warned = true;
                }
                self.path = None;
                return None;
            }
        }

        let mut writer = match File::create(&file_path).map(BufWriter::new) {
            Ok(writer) => writer,
            Err(error) => {
                if !self.warned {
                    warn!(
                        "Disabling USB PCAP logging after failing to open {}: {}",
                        file_path.display(),
                        error
                    );
                    self.warned = true;
                }
                self.path = None;
                return None;
            }
        };

        if let Err(error) = writer.write_all(&pcap_global_header_bytes()) {
            if !self.warned {
                warn!(
                    "Disabling USB PCAP logging after failing to write header to {}: {}",
                    file_path.display(),
                    error
                );
                self.warned = true;
            }
            self.path = None;
            return None;
        }

        self.writer = Some(writer);
        self.writer.as_mut()
    }

    pub fn write_record(&mut self, record: &[u8]) {
        let writer = match self.ensure_writer() {
            Some(writer) => writer,
            None => return,
        };

        if let Err(error) = writer.write_all(record).and_then(|_| writer.flush()) {
            if !self.warned {
                warn!("Failed to write USB PCAP record: {}", error);
                self.warned = true;
            }
            self.path = None;
            self.writer = None;
        }
    }
}

static MANAGER: Mutex<Option<PcapManager>> = Mutex::new(None);

/// Global holder for an optional PCAP manager.
///
/// This provides a single synchronized entry point for PCAP output so callers
/// do not need to store writers themselves. The manager is optional to allow
/// USB PCAP logging to be enabled or disabled at runtime.
pub struct UsbPcapManager;

impl UsbPcapManager {
    pub fn init(path: Option<PathBuf>) {
        *MANAGER.lock().unwrap() = Some(PcapManager::new(path));
    }

    pub fn write_record(record: &[u8]) {
        if let Some(manager) = MANAGER.lock().unwrap().as_mut() {
            manager.write_record(record);
        }
    }
}
