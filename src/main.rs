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

use anyhow::{Context, Result};
use clap::Parser;
use cli::Cli;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;
use vfio_user::Server;

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

    info!("We're up!");

    server
        .run(&mut backend)
        .context("Failed to start vfio-user server")?;
    Ok(())
}
