#![deny(
    clippy::all,
    clippy::cargo,
    clippy::nursery,
    clippy::must_use_candidate
)]
// now allow a few rules which are denied by the above's statement
#![allow(clippy::multiple_crate_versions)]
#![deny(missing_debug_implementations)]
#![deny(rustdoc::all)]

//! usbvfiod

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use anyhow::{Context, Result};
use nusb::MaybeFuture;
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

fn main() -> Result<()> {
    let path = "/dev/bus/usb/008/009";
    let open_file = |err_msg| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("{}: {}", err_msg, path))
    };

    let file = open_file("Failed to open USB device file")?;
    let device = nusb::Device::from_fd(file.into()).wait()?;
    device.reset().wait()?;

    // After the reset, the device instance is no longer usable and we need
    // to reopen.
    let file = open_file("Failed to open USB device file after device reset")?;

    // write to socket for hot-attach fds
    let socket = UnixStream::connect("/tmp/usbvfiod-hot-attach").unwrap();
    let buf = [0u8; 1];
    let _byte_count = socket.send_with_fd(&buf[..], file.as_raw_fd()).unwrap();

    Ok(())
}
