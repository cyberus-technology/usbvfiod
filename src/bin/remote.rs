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

//! remote
//!
//! Command-line tool to attach/detach/list USB devices to/from usbvfiod.

use std::{
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::{ArgAction, Parser};
use nusb::MaybeFuture;
use usbvfiod::hotplug_protocol::{
    command::Command, device_paths::resolve_path, response::Response,
};

fn main() -> Result<()> {
    let args = Cli::parse();

    if let Some(path) = args.attach {
        let response = attach(path.as_path(), args.socket.as_path())?;
        println!("{:?}", response);
    } else if let Some(vec) = args.detach {
        // Safety: clap ensures that vec.len() == 2.
        let bus = vec[0];
        let dev = vec[1];
        let response = detach(bus, dev, args.socket.as_path())?;
        println!("{:?}", response);
    } else if args.list {
        let devices = list_attached(args.socket.as_path())?;
        println!("Attached devices:");
        for (bus, dev) in devices {
            println!("{}:{}", bus, dev);
        }
    }

    Ok(())
}

fn attach(device_path: &Path, socket_path: &Path) -> Result<Response> {
    let (bus, dev, device_path) = resolve_path(device_path)
        .with_context(|| format!("Failed to resolve device path {:?}", device_path))?;

    let open_file = |err_msg: &str| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&device_path)
            .with_context(|| err_msg.to_string())
    };

    let file = open_file("Failed to open USB device file")?;
    let device = nusb::Device::from_fd(file.into())
        .wait()
        .context("Failed to open nusb device")?;
    device.reset().wait().context("Failed to reset device")?;

    // After the reset, the device instance is no longer usable and we need
    // to reopen.
    let file = open_file("Failed to open USB device file after device reset")?;

    // write to socket for hot-attach fds
    let command = Command::Attach {
        bus,
        device: dev,
        fd: file,
    };
    let mut socket = UnixStream::connect(socket_path).context("Failed to open socket")?;
    command
        .send_over_socket(&socket)
        .context("Failed to send attach command over the socket")?;

    let response = Response::receive_from_socket(&mut socket)
        .context("Failed to receive response over the socket")?;
    Ok(response)
}

fn detach(bus: u8, dev: u8, socket_path: &Path) -> Result<Response> {
    println!("detach {}:{} from {:?}", bus, dev, socket_path);
    todo!();
}

fn list_attached(socket_path: &Path) -> Result<Vec<(u8, u8)>> {
    println!("list attached from {:?}", socket_path);
    todo!();
}

#[derive(Parser, Debug)]
#[command(
    name = env!("CARGO_PKG_NAME"),
    version = env!("CARGO_PKG_VERSION"),
    author = env!("CARGO_PKG_AUTHORS"),
    about = env!("CARGO_PKG_DESCRIPTION"),
    long_about = None
)]
struct Cli {
    /// Path to the hot-attach socket that the usbvfiod instances exposes.
    #[arg(long, value_name = "PATH")]
    socket: PathBuf,

    /// Attach the USB device to usbvfiod. The path must point to a device in: /dev/bus/usb.
    /// This option is mutually exclusive with --detach and --list.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "detach",
        conflicts_with = "list"
    )]
    attach: Option<PathBuf>,

    /// Detach the USB device from usbvfiod. Specify the device with the bus number
    /// and the device number.
    ///
    /// This option is mutually exclusive with --attach and --list.
    #[arg(long, num_args = 2, conflicts_with = "attach", conflicts_with = "list")]
    detach: Option<Vec<u8>>,

    /// List the currently attached USB devices.
    ///
    /// This option is mutually exclusive with --attach and --detach.
    #[arg(long, action = ArgAction::SetTrue, conflicts_with = "attach", conflicts_with = "detach")]
    list: bool,
}
