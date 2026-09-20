//! Passthrough of individual USB devices using the vfio-user protocol

mod async_runtime;
mod cli;
mod device;
mod dynamic_bus;
mod hotplug_server;
mod memory_segment;
mod one_indexed_array;
mod oneshot_anyhow;
mod shared_backend;
mod xhci_backend;

use std::{
    os::fd::{FromRawFd, OwnedFd},
    sync::Arc,
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use async_runtime::init_runtime;
use clap::Parser;
use cli::Cli;
use device::{pcap::UsbPcapManager, xhci::real_device::CompleteRealDevice};
use hotplug_server::run_hotplug_server;
use shared_backend::SharedBackendState;
use tracing::{debug, error, info, Level};
use tracing_subscriber::FmtSubscriber;
use vfio_user::Server;
use xhci_backend::XhciBackend;

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

    let server = Arc::new(match args.server_socket() {
        cli::ServerSocket::Path(socket_path) => {
            // `resettable = false`: usbvfiod must never reset the device on
            // reconnect, which would make the guest re-enumerate (R4).
            Server::new(socket_path, false, backend.irqs(), backend.regions())
                .context("Failed to create vfio-user server")?
        }
        cli::ServerSocket::Fd(fd) => {
            // SAFETY: we have to assume the given fd is valid, there is not much else we can do
            let owned_fd = unsafe { OwnedFd::from_raw_fd(fd) };
            Server::from_owned_fd(owned_fd, false, backend.irqs(), backend.regions())
        }
    });

    if let Some(socket) = args.hotplug_socket() {
        let hotplug_control = backend.hotplug_control();
        thread::Builder::new()
            .name("hot-attach-socket listener".to_string())
            .spawn(move || run_hotplug_server(socket, hotplug_control, runtime.clone()))
            .unwrap();
    }

    info!("We're up!");

    if args.max_clients > 1 {
        run_multi_client(server, backend, args.max_clients)?;
    } else {
        server
            .run(&mut backend)
            .context("Failed to start vfio-user server")?;
    }

    if let Some(hotplug_socket_path) = args.hotplug_socket_path {
        if hotplug_socket_path.exists() {
            let _ = std::fs::remove_file(&hotplug_socket_path);
        }
    }

    Ok(())
}

/// Serve up to `max_clients` concurrent vfio-user clients.
///
/// The destination VMM of a same-host live migration connects while the source
/// VMM is still connected, so more than one client has to be served at a time.
/// Unlike the single-client path, the process stays alive after the last client
/// disconnects; stop it explicitly (or through systemd socket activation).
fn run_multi_client<CRD: CompleteRealDevice>(
    server: Arc<Server>,
    backend: XhciBackend<CRD>,
    max_clients: usize,
) -> Result<()> {
    let shared = Arc::new(SharedBackendState::new(backend));
    let mut handles = Vec::with_capacity(max_clients);

    for index in 0..max_clients {
        let server = Arc::clone(&server);
        let shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name(format!("vfio-user-client-{index}"))
            .spawn(move || {
                loop {
                    let mut shared_backend = shared.connect();
                    if let Err(err) = server.run(&mut shared_backend) {
                        error!("vfio-user connection ended with error: {err}");
                        // Do not spin on a persistently failing listener.
                        thread::sleep(Duration::from_millis(100));
                    } else {
                        debug!("vfio-user client disconnected, accepting the next one");
                    }
                }
            })?;
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.join();
    }

    Ok(())
}
