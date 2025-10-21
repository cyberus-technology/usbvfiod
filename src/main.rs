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

mod cli;
mod device;
mod dynamic_bus;
mod memory_segment;
mod xhci_backend;

use std::{os::unix::net::UnixListener, thread};

use anyhow::{Context, Result};
use clap::Parser;
use cli::Cli;
use device::pci::nusb::NusbDeviceWrapper;
use nusb::MaybeFuture;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;
use vfio_user::Server;
use vmm_sys_util::sock_ctrl_msg::ScmSocket;

fn main() -> Result<()> {
    let args = Cli::parse();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(match args.verbose {
            0 => Level::INFO,
            1 => Level::DEBUG,
            _ => Level::TRACE,
        })
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .context("Failed to set global tracing subscriber")?;

    // Log messages from the log crate as well.
    tracing_log::LogTracer::init()?;

    let mut backend = xhci_backend::XhciBackend::new(&args.devices)
        .context("Failed to create virtual XHCI controller")?;

    let server = if let cli::ServerSocket::Path(socket_path) = args.server_socket() {
        Server::new(socket_path, true, backend.irqs(), backend.regions())
            .context("Failed to create vfio-user server")?
    } else {
        unimplemented!("Using a file descriptor as vfio-user connection is not implemented")
    };

    // listen on socket for hot-attach fds
    let controller = backend.get_controller();
    let socket = UnixListener::bind("/tmp/usbvfiod-hot-attach").unwrap();
    thread::Builder::new()
        .name("hot-attach-socket listener".to_string())
        .spawn(move || {
            let mut buf = [0u8; 1];
            loop {
                let (stream, _addr) = socket.accept().unwrap();
                let (_byte_count, file) = stream.recv_with_fd(&mut buf).unwrap();
                let fd = file.unwrap();
                let device = nusb::Device::from_fd(fd.into()).wait().unwrap();
                let wrapped_device = Box::new(NusbDeviceWrapper::new(device));
                controller.lock().unwrap().set_device(wrapped_device);
            }
        })
        .unwrap();

    info!("We're up!");

    server
        .run(&mut backend)
        .context("Failed to start vfio-user server")?;
    Ok(())
}
