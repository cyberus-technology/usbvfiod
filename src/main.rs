//! usbvfiod

mod async_runtime;
mod cli;
mod device;
mod dynamic_bus;
mod hotplug_server;
mod memory_segment;
mod xhci_backend;

use std::{os::unix::net::UnixListener, thread};

use crate::device::pci::pcap::UsbPcapManager;
use anyhow::{Context, Result};
use async_runtime::init_runtime;
use clap::Parser;
use cli::Cli;
use hotplug_server::run_hotplug_server;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;
use vfio_user::Server;

fn main() -> Result<()> {
    let args = Cli::parse();
    UsbPcapManager::init(args.pcap_path.clone());

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

    init_runtime().context("Failed to initialize async runtime")?;

    let mut backend = xhci_backend::XhciBackend::new(&args.devices)
        .context("Failed to create virtual XHCI controller")?;

    let server = if let cli::ServerSocket::Path(socket_path) = args.server_socket() {
        Server::new(socket_path, true, backend.irqs(), backend.regions())
            .context("Failed to create vfio-user server")?
    } else {
        unimplemented!("Using a file descriptor as vfio-user connection is not implemented")
    };

    // listen on socket for hot-attach fds
    if let Some(hotplug_socket_path) = args.hotplug_socket_path {
        let controller = backend.get_controller();
        let socket = UnixListener::bind(hotplug_socket_path.as_path()).unwrap();
        thread::Builder::new()
            .name("hot-attach-socket listener".to_string())
            .spawn(move || run_hotplug_server(socket, controller))
            .unwrap();
    }

    info!("We're up!");

    server
        .run(&mut backend)
        .context("Failed to start vfio-user server")?;
    Ok(())
}
