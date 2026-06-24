//! Implements the CLI interface.
//!
//! The main external constraint here is that we need to be compatible
//! to the vfio-user [Backend Program
//! Conventions](https://www.qemu.org/docs/master/interop/vfio-user.html#backend-program-conventions).
use std::{
    os::{
        fd::{FromRawFd, OwnedFd, RawFd},
        unix::net::UnixListener,
    },
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
    #[arg(long, value_name = "FDNUM", conflicts_with = "socket_path")]
    fd: Option<RawFd>,

    /// The path where to create a listening Unix domain socket.
    ///
    /// This is the path where Cloud Hypervisor will connect to
    /// usbvfiod. This option is mutually exclusive with --fd.
    #[arg(long, value_name = "PATH", required_unless_present = "fd")]
    socket_path: Option<PathBuf>,

    /// Path to a USB device to be attached from VM boot. Can be
    /// specified multiple times to attach more devices. The path must
    /// point to a device in: /dev/bus/usb
    ///
    /// See the documentation for how to identify devices.
    #[arg(long = "device", value_name = "PATH")]
    pub devices: Vec<PathBuf>,

    /// The path where to create a listening Unix domain socket and listen
    /// for hotplug commands.
    #[arg(long, value_name = "PATH")]
    pub hotplug_socket_path: Option<PathBuf>,

    #[arg(long, value_name = "FDNUM", conflicts_with = "hotplug_socket_path")]
    pub hotplug_fd: Option<RawFd>,

    /// Enable PCAP logging and write captured USB traffic to this file.
    /// The file will be created when the first packet is logged.
    #[arg(long, value_name = "PATH")]
    pub pcap_path: Option<PathBuf>,

    /// Do not use ANSI color codes in the output.
    #[arg(long)]
    pub no_color: bool,
}

/// The location of the server socket for the vfio-user client connection.
#[derive(Debug)]
pub enum ServerSocket<'a> {
    /// The socket is already open.
    Fd(RawFd),

    /// We need to create the socket at this path.
    Path(&'a Path),
}

impl Cli {
    pub fn server_socket(&self) -> ServerSocket<'_> {
        // The clap configuration above ensures always only one of those two options is Some().
        self.fd.map_or_else(
            || {
                let path = self.socket_path.as_ref().unwrap().as_path();
                ServerSocket::Path(path)
            },
            ServerSocket::Fd,
        )
    }

    pub fn hotplug_socket(&self) -> Option<UnixListener> {
        // The clap configuration above ensures that maximum of one of the two hotplug options is used.
        self.hotplug_fd.map_or_else(
            || {
                self.hotplug_socket_path.as_ref().map_or_else(
                    || None,
                    |path| {
                        Some(
                            UnixListener::bind(path.as_path())
                                .expect("failed to use provided hotplug socket path"),
                        )
                    },
                )
            },
            |hotplug_fd| {
                // SAFETY: we have to assume the given fd is valid, there is not much else we can do
                let owned_fd_hotplug: OwnedFd = unsafe { OwnedFd::from_raw_fd(hotplug_fd) };
                Some(UnixListener::from(owned_fd_hotplug))
            },
        )
    }
}
