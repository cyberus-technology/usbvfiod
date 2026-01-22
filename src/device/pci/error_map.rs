use std::error::Error;

/// Map an error chain into a USB PCAP status value.
///
/// Returns a negative errno when possible:
/// - `nusb::transfer::TransferError::Disconnected` is mapped to `-ENODEV`.
/// - `std::io::Error` values yield `-raw_os_error()` when available.
///
/// Falls back to `-1` when no OS error code can be extracted.
pub fn status_from_error(error: &(dyn Error + 'static)) -> i32 {
    let mut current: Option<&(dyn Error + 'static)> = Some(error);
    while let Some(err) = current {
        if let Some(transfer_error) = err.downcast_ref::<nusb::transfer::TransferError>() {
            if matches!(transfer_error, nusb::transfer::TransferError::Disconnected) {
                return -libc::ENODEV;
            }
        }
        if let Some(io_error) = err.downcast_ref::<std::io::Error>() {
            if let Some(code) = io_error.raw_os_error() {
                return -code;
            }
        }
        current = err.source();
    }
    -1
}
