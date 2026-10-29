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
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::StreamExt;
use nusb::{
    hotplug::{self, HotplugEvent},
    DeviceId, DeviceInfo, MaybeFuture,
};
use serde::de::{Deserialize, Error};
use signal_hook::consts::signal::SIGHUP;
use signal_hook_tokio::Signals;
use std::collections::HashMap;
use tokio::select;
use usbvfiod::hotplug_protocol::{
    command::Command, device_paths::resolve_path, response::Response,
};

#[derive(Debug, serde::Deserialize)]
struct UsbptPolicy {
    socket_path: PathBuf,
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

struct PolicyEngine {
    config_path: PathBuf,
    config_reload: Signals,
    hotplug_watch: hotplug::HotplugWatch,
    policies: Option<Vec<UsbptPolicy>>,
    devices: HashMap<DeviceId, DeviceInfo>,
    plugged: HashMap<DeviceId, (PathBuf, u8, u8)>,
}

impl PolicyEngine {
    pub fn new(config_path: PathBuf) -> Result<Self> {
        // Register device watcher for hotplug events.
        let hotplug_watch = nusb::watch_devices()?;
        // Get current device list.
        let devices: HashMap<DeviceId, DeviceInfo> =
            nusb::list_devices().wait()?.map(|d| (d.id(), d)).collect();
        // Set up a signal handler for config reload.
        let config_reload = Signals::new([SIGHUP])?;

        let mut engine = Self {
            config_path,
            config_reload,
            hotplug_watch,
            policies: None,
            devices,
            plugged: HashMap::default(),
        };
        engine.reload_policies();

        Ok(engine)
    }

    fn reload_policies(&mut self) {
        println!("Reloading policies from {}", self.config_path.display());
        // Load and apply current configuration.
        self.policies = std::fs::File::open(self.config_path.clone())
            .map_or_else(
                |e| Err(serde_json::Error::custom(e)),
                serde_json::from_reader,
            )
            .inspect_err(|e| eprintln!("Failed to parse policy file: {e}"))
            .ok();

        if self.policies.is_some() {
            self.devices
                .values()
                .cloned()
                .collect::<Vec<_>>()
                .iter()
                .for_each(|d| self.apply_policies(d));
        }
    }

    fn apply_policies(&mut self, device: &DeviceInfo) {
        let busnum = device.busnum();
        let devnum = device.device_address();

        enum PolicyAction {
            Change(PathBuf),
            Attach(PathBuf),
            Detach,
        }

        // No action if policy loading failed.
        if let Some(ref policies) = self.policies {
            let action = if let Some(socket_path) = policies
                .iter()
                .find(|p| p.vendor_id == device.vendor_id() && p.product_id == device.product_id())
                .map(|p| p.socket_path.as_path())
            {
                // Check if already attached.
                if let Some((old_path, old_bus, old_dev)) = self.plugged.get(&device.id()) {
                    // Re-attach only if the socket path changed.
                    if old_path == socket_path {
                        debug_assert_eq!(busnum, *old_bus);
                        debug_assert_eq!(devnum, *old_dev);
                        // Nothing to do. Re-attaching would break the existing device.
                        // Keep the old plugged reference.
                        return;
                    }
                    PolicyAction::Change(socket_path.to_path_buf())
                } else {
                    PolicyAction::Attach(socket_path.to_path_buf())
                }
            } else {
                PolicyAction::Detach
            };

            match action {
                PolicyAction::Change(new_socket) => {
                    self.unplug_device(device.id(), "policy changed");
                    self.plug_device(device.id(), new_socket.as_path(), busnum, devnum);
                }
                PolicyAction::Attach(new_socket) => {
                    self.plug_device(device.id(), new_socket.as_path(), busnum, devnum);
                }
                PolicyAction::Detach => {
                    self.unplug_device(device.id(), "no policy matched");
                }
            }
        }
    }

    /// Plug a device into a hotplug socket.
    fn plug_device(&mut self, id: DeviceId, socket_path: &Path, busnum: u8, devnum: u8) {
        let device_str = format!("/dev/bus/usb/{busnum:03}/{devnum:03}");
        let device_path = Path::new(&device_str);
        println!(
            "Attaching {busnum:03}:{devnum:03} to {} -> policy matched",
            socket_path.display()
        );
        match attach(device_path, socket_path) {
            Ok(_) => {
                self.plugged
                    .insert(id, (socket_path.to_path_buf(), busnum, devnum));
            }
            Err(e) => eprintln!(
                "Failed to attach {} to {}: {e}",
                device_path.display(),
                socket_path.display()
            ),
        }
    }

    /// Unplug a device from its current hotplug socket.
    fn unplug_device(&mut self, id: DeviceId, reason: &str) {
        // Check if the device is currently passed-through.
        if let Some((path, bus, dev)) = self.plugged.remove(&id) {
            println!(
                "Detaching {bus:03}:{dev:03} from {} -> {reason}",
                path.display()
            );
            detach(bus, dev, path.as_path()).unwrap_or_else(|e| {
                eprintln!(
                    "Failed to detach {bus:03}:{dev:03} from {}: {e}",
                    path.display()
                );
                // No use tracking this device anymore. Detach will not succeed later.
            });
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        loop {
            select! {
                // Signal for a configuration reload
                _ = self.config_reload.next() => self.reload_policies(),
                // Device hotplug events
                Some(event) = self.hotplug_watch.next() =>
                    match event {
                        HotplugEvent::Connected(device) => {
                            self.apply_policies(&device);
                            self.devices.insert(device.id(), device);
                        }
                        HotplugEvent::Disconnected(id) => {
                            self.unplug_device(id, "device removed");
                            self.devices.remove(&id);
                        }
                    }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Cli::parse();

    let mut engine = PolicyEngine::new(args.config)?;
    engine.run().await
}

fn attach(device_path: &Path, socket_path: &Path) -> Result<()> {
    let (bus, dev, device_path) = resolve_path(device_path)
        .with_context(|| format!("Failed to resolve device path {device_path:?}"))?;

    println!("Requesting attachment of device {bus:03}:{dev:03}");

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

fn detach(bus: u8, dev: u8, socket_path: &Path) -> Result<()> {
    println!("Requesting detach of device {bus:03}:{dev:03}");

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
    /// Path to the policy configuration JSON.
    #[arg(long, value_name = "PATH")]
    config: PathBuf,
}
