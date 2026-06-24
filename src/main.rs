//! Passthrough of individual USB devices using the vfio-user protocol

mod async_runtime;
mod cli;
mod device;
mod dynamic_bus;
mod hotplug_server;
mod memory_segment;
mod one_indexed_array;
mod oneshot_anyhow;
mod xhci_backend;

use std::{
    os::fd::{FromRawFd, OwnedFd},
    thread,
};

use anyhow::{Context, Result};
use async_runtime::init_runtime;
use clap::Parser;
use cli::Cli;
use device::pcap::UsbPcapManager;
use hotplug_server::run_hotplug_server;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;
use vfio_user::Server;

use crate::async_runtime::runtime;

fn main() -> Result<()> {
    let args = Cli::parse();

    let subscriber = FmtSubscriber::builder()
        .with_max_level(match args.verbose {
            0 => Level::INFO,
            1 => Level::DEBUG,
            _ => Level::TRACE,
        })
        .with_ansi(!args.no_color)
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .context("Failed to set global tracing subscriber")?;

    // Log messages from the log crate as well.
    tracing_log::LogTracer::init()?;

    UsbPcapManager::init(args.pcap_path.clone());

    init_runtime().context("Failed to initialize async runtime")?;
    let runtime = runtime();

    let mut backend = xhci_backend::XhciBackend::new(runtime.clone())
        .context("Failed to create virtual XHCI controller")?;
    for device in &args.devices {
        let path = device.as_path();
        // if initial device attachment fails, make it clear by panicking
        if let Err(err) = runtime.block_on(backend.add_device_from_path(path, runtime.clone())) {
            panic!("Device attachment failed for {path:?}: {err}");
        }
    }

    let server = match args.server_socket() {
        cli::ServerSocket::Path(socket_path) => {
            Server::new(socket_path, true, backend.irqs(), backend.regions())
                .context("Failed to create vfio-user server")?
        }
        cli::ServerSocket::Fd(fd) => {
            // SAFETY: we have to assume the given fd is valid, there is not much else we can do
            let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
            Server::from_owned_fd(owned_fd, true, backend.irqs(), backend.regions())
        }
    };

    if let Some(socket) = args.hotplug_socket() {
        let hotplug_control = backend.hotplug_control();
        thread::Builder::new()
            .name("hot-attach-socket listener".to_string())
            .spawn(move || run_hotplug_server(socket, hotplug_control, runtime.clone()))
            .unwrap();
    }

    info!("We're up!");

    server
        .run(&mut backend)
        .context("Failed to start vfio-user server")?;

    if let Some(hotplug_socket_path) = args.hotplug_socket_path {
        if hotplug_socket_path.exists() {
            let _ = std::fs::remove_file(&hotplug_socket_path);
        }
    }

    Ok(())
}
