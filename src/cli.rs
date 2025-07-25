//! This module implements the CLI interface.
//!
//! The main external constraint here is that we need to be compatible
//! to the vfio-user [Backend Program
//! Conventions](https://github.com/nutanix/libvfio-user/blob/master/docs/vfio-user.rst#backend-program-conventions).
use std::{
    os::fd::RawFd,
    path::{Path, PathBuf},
};

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = env!("CARGO_PKG_NAME"),
    version = env!("CARGO_PKG_VERSION"),
    author = env!("CARGO_PKG_AUTHORS"),
    about = env!("CARGO_PKG_DESCRIPTION"),
    long_about = None
)]
pub struct Cli {
    /// Enable verbose logging. Can be specified multiple times to
    /// increase verbosity.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Provide the vfio-user socket as file descriptor.
    ///
    /// This option is mutually exclusive with --socket-path.
    #[arg(long, conflicts_with = "socket_path")]
    fd: Option<RawFd>,

    /// The path where to create a listening Unix domain socket.
    ///
    /// This is the path where Cloud Hypervisor will connect to
    /// usbvfiod. This option is mutually exclusive with --fd.
    #[arg(long, required_unless_present = "fd")]
    socket_path: Option<PathBuf>,

    /// Path to a USB device to be attached from VM boot. Can be
    /// specified multiple times to attach more devices. The path must
    /// point to a device in: /dev/bus/usb
    ///
    /// See the documentation for how to identify devices.
    #[arg(long = "device", value_name = "PATH")]
    pub devices: Vec<PathBuf>,
}

/// The location of the server socket for the vfio-user client connection.
#[derive(Debug)]
pub enum ServerSocket<'a> {
    /// The socket is already open.
    #[allow(dead_code)]
    Fd(RawFd),

    /// We need to create the socket at this path.
    Path(&'a Path),
}

impl Cli {
    pub fn server_socket(&self) -> ServerSocket<'_> {
        self.socket_path.as_ref().map_or_else(
            || unreachable!(),
            |socket_path| ServerSocket::Path(socket_path),
        )
    }
}
