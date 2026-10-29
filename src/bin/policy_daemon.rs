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

//! A service executable to attach/detach USB devices to/from multiple
//! usbvfiod servers at runtime according to a JSON policy configuration.
//!
//! The service listens for SIGHUP to trigger a reload of the policy file.
use std::{
    mem,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use anyhow::Context;
use clap::Parser;
use futures::stream::StreamExt;
use nusb::{
    hotplug::{self, HotplugEvent},
    DeviceId, DeviceInfo, MaybeFuture,
};
use serde::de::Deserialize;
use signal_hook::consts::signal::SIGHUP;
use signal_hook_tokio::Signals;
use std::collections::HashMap;
use tokio::select;
use tracing::{debug, error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;
use usbvfiod::hotplug_protocol::{
    command::Command, device_paths::resolve_path, response::Response,
};

#[derive(Debug, serde::Deserialize)]
struct UsbptPolicy {
    socket_path: UsbvfiodSocket,
    #[serde(deserialize_with = "hex_u16")]
    vendor_id: u16,
    #[serde(deserialize_with = "hex_u16")]
    product_id: u16,
}

fn hex_u16<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let buf = String::deserialize(deserializer)?;
    u16::from_str_radix(buf.strip_prefix("0x").unwrap_or(&buf), 16)
        .map_err(serde::de::Error::custom)
}

impl UsbptPolicy {
    fn device_target(&self, device: &DeviceInfo) -> Option<UsbvfiodSocket> {
        (self.vendor_id == device.vendor_id() && self.product_id == device.product_id())
            .then_some(self.socket_path.clone())
    }
}

#[derive(Debug, Default)]
struct UsbptPolicySet {
    policies: Vec<UsbptPolicy>,
}

impl UsbptPolicySet {
    fn load_from_file<P: AsRef<Path>>(config_path: P) -> anyhow::Result<Self> {
        let path = config_path.as_ref();
        let policy_file = std::fs::File::open(path)
            .with_context(|| format!("Failed to open policies file {}", path.display()))?;
        let policies =
            serde_json::from_reader(policy_file).context("Failed to parse policies file")?;

        Ok(Self { policies })
    }

    fn device_target(&self, device: &DeviceInfo) -> Option<UsbvfiodSocket> {
        self.policies
            .iter()
            .find_map(|policy| policy.device_target(device))
    }
}

type UsbvfiodSocket = PathBuf;

struct Device {
    info: DeviceInfo,
    plugged: Option<UsbvfiodSocket>,
}

impl Device {
    const fn new(info: DeviceInfo) -> Self {
        Self {
            info,
            plugged: None,
        }
    }
    fn plug(&mut self, socket_path: UsbvfiodSocket) {
        match &self.plugged {
            Some(current_socket) if *current_socket == socket_path => {
                // Already attached.
                return;
            }
            Some(_) => {
                // Currently attached to another socket.
                self.unplug("policy change");
            }
            None => {}
        }

        // Attach to new target
        let device_str = format!(
            "/dev/bus/usb/{:03}/{:03}",
            self.info.busnum(),
            self.info.device_address()
        );
        let device_path = Path::new(&device_str);

        info!(
            "Attaching {} to {}",
            device_path.display(),
            socket_path.display()
        );

        match attach(device_path, &socket_path) {
            Ok(_) => self.plugged = Some(socket_path),
            Err(e) => error!(
                "Failed to attach {} to {}: {e:#}",
                device_path.display(),
                socket_path.display()
            ),
        }
    }

    fn unplug(&mut self, reason: &str) {
        if let Some(socket_path) = mem::take(&mut self.plugged) {
            let bus = self.info.busnum();
            let dev = self.info.device_address();
            info!(
                "Detaching {bus:03}:{dev:03} from {} -> {reason}",
                socket_path.display()
            );
            detach(bus, dev, &socket_path).unwrap_or_else(|e| {
                error!(
                    "Failed to detach {bus:03}:{dev:03} from {}: {e:#}",
                    socket_path.display()
                );
                // Nonetheless, we keep the state in self.plugged that we are no longer attached.
            });
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.unplug("device disconnect");
    }
}

struct PolicyEngine {
    config_path: PathBuf,
    config_reload: Signals,
    hotplug_watch: hotplug::HotplugWatch,
    policies: UsbptPolicySet,
    devices: HashMap<DeviceId, Device>,
}

impl PolicyEngine {
    async fn new(config_path: PathBuf) -> anyhow::Result<Self> {
        let config_reload =
            Signals::new([SIGHUP]).context("Failed to setup signal handler for config reloads")?;
        let hotplug_watch =
            nusb::watch_devices().context("Failed to register device-hotplug watcher")?;
        let devices = nusb::list_devices()
            .await
            .context("Failed to query USB devices")?
            .map(|d| (d.id(), Device::new(d)))
            .collect();
        // Fail hard when initial config is invalid.
        // Later, config-reload failures are acceptable.
        let policies = UsbptPolicySet::load_from_file(&config_path)
            .context("Failed to load initial policy configuration")?;

        let mut engine = Self {
            config_path,
            config_reload,
            hotplug_watch,
            policies: UsbptPolicySet::default(),
            devices,
        };
        engine.update_policies(policies);

        Ok(engine)
    }

    fn update_policies(&mut self, policies: UsbptPolicySet) {
        self.policies = policies;

        // Need to check all devices against the new policies.
        for device in self.devices.values_mut() {
            match self.policies.device_target(&device.info) {
                Some(target) => device.plug(target),
                None => device.unplug("policy change"),
            }
        }
    }

    fn config_reload(&mut self) {
        let policies = UsbptPolicySet::load_from_file(&self.config_path)
            .context("Failed to reload config on signal");
        match policies {
            Ok(policies) => self.update_policies(policies),
            Err(err) => {
                // Config-update errors keep the previous policies active.
                error!("{err:#}");
            }
        }
    }

    fn hotplug_connect(&mut self, info: DeviceInfo) {
        let mut device = Device::new(info);
        if let Some(target) = self.policies.device_target(&device.info) {
            device.plug(target);
        }
        self.devices.insert(device.info.id(), device);
    }

    fn hotplug_disconnect(&mut self, id: DeviceId) {
        let device = self.devices.remove(&id);
        if device.is_none() {
            warn!("Hotplug disconnect event for unknown device");
        }
    }

    async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            select! {
                // Signal for a configuration reload
                _ = self.config_reload.next() => self.config_reload(),
                // Device hotplug events
                Some(event) = self.hotplug_watch.next() => {
                    match event {
                        HotplugEvent::Connected(device) => self.hotplug_connect(device),
                        HotplugEvent::Disconnected(id) => self.hotplug_disconnect(id),
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

    let mut engine = PolicyEngine::new(args.config).await?;
    info!("Policy daemon ready");
    engine.run().await
}

fn attach(device_path: &Path, socket_path: &Path) -> anyhow::Result<()> {
    let (bus, dev, device_path) = resolve_path(device_path)
        .with_context(|| format!("Failed to resolve device path {device_path:?}"))?;

    debug!("Requesting attachment of device {bus:03}:{dev:03}");

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

    match response {
        Response::SuccessfulOperation => Ok(()),
        other => Err(anyhow::Error::msg(format!("attach failed: {other:?}"))),
    }
}

fn detach(bus: u8, dev: u8, socket_path: &Path) -> anyhow::Result<()> {
    debug!("Requesting detach of device {bus:03}:{dev:03}");

    let command = Command::Detach { bus, device: dev };
    let mut socket = UnixStream::connect(socket_path).context("Failed to open socket")?;
    command
        .send_over_socket(&socket)
        .context("Failed to send detach command over the socket")?;

    let response = Response::receive_from_socket(&mut socket)
        .context("Failed to receive response over the socket")?;

    match response {
        Response::SuccessfulOperation => Ok(()),
        other => Err(anyhow::Error::msg(format!("detach failed: {other:?}"))),
    }
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
    /// Enable verbose logging. Can be specified multiple times to
    /// increase verbosity.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Do not use ANSI color codes in the output.
    #[arg(long)]
    no_color: bool,

    /// Path to the policy configuration JSON.
    #[arg(long, value_name = "PATH")]
    config: PathBuf,
}
