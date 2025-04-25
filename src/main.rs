mod cli;
mod xhci_backend;

pub(crate) use anyhow::{Context, Result};
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

    info!("We're up!");

    let mut backend = xhci_backend::XhciBackend::new();
    let s = Server::new(&args.socket_path, true, backend.irqs(), backend.regions())
        .context("Failed to create vfio-user server")?;

    s.run(&mut backend)
        .context("Failed to start vfio-user server")?;
    Ok(())
}
