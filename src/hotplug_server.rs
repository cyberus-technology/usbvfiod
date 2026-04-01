//! Receive runtime commands

use std::{
    fs::File,
    os::unix::net::{UnixListener, UnixStream},
};

use anyhow::{Context, Result};
use nusb::MaybeFuture;
use tokio::runtime;
use tracing::{debug, warn};
use usbvfiod::hotplug_protocol::{command::Command, response::Response};

use crate::device::xhci::{
    nusb::NusbRealDevice, port::HotplugControl, real_device::CompleteRealDeviceImpl,
};

pub fn run_hotplug_server(
    socket: UnixListener,
    hotplug_control: HotplugControl<CompleteRealDeviceImpl<NusbRealDevice, (u8, u8)>>,
    async_runtime: runtime::Handle,
) {
    loop {
        if let Ok((mut stream, _addr)) = socket.accept() {
            match Command::receive_from_socket(&stream) {
                Ok(command) => {
                    debug!("Received command {:?} on hotplug socket", command);
                    if let Err(e) =
                        handle_command(command, &mut stream, &hotplug_control, &async_runtime)
                    {
                        // The error contains all the necessary context
                        warn!("{:?}", e);
                    }
                }
                Err(e) => warn!("Error occurred while reading a hotplug command {}", e),
            }
        }
    }
}

fn handle_command(
    command: Command,
    socket: &mut UnixStream,
    hotplug_control: &HotplugControl<CompleteRealDeviceImpl<NusbRealDevice, (u8, u8)>>,
    async_runtime: &runtime::Handle,
) -> Result<()> {
    match command {
        Command::Attach {
            bus,
            device: dev,
            fd,
        } => handle_attach(bus, dev, fd, socket, hotplug_control, async_runtime)
            .context("Failed to handle attach command")?,
        Command::Detach { bus, device } => {
            handle_detach(bus, device, socket, hotplug_control, async_runtime)
                .context("Failed to handle detach command")?;
        }
        Command::List => {
            let devices = async_runtime.block_on(hotplug_control.list_devices());
            Response::ListFollowing
                .send_device_list(devices, socket)
                .context("Failed to handle list command")?;
        }
    }

    Ok(())
}

fn handle_attach(
    bus: u8,
    dev: u8,
    fd: File,
    socket: &mut UnixStream,
    hotplug_control: &HotplugControl<CompleteRealDeviceImpl<NusbRealDevice, (u8, u8)>>,
    async_runtime: &runtime::Handle,
) -> Result<()> {
    let device = nusb::Device::from_fd(fd.into())
        .wait()
        .context("Failed to open nusb device from the supplied file descriptor")?;
    let real_device = NusbRealDevice::try_new(device, async_runtime.clone())?;
    let complete_device = CompleteRealDeviceImpl::new((bus, dev), real_device);
    let response = async_runtime.block_on(hotplug_control.attach(complete_device));
    response
        .send_over_socket(socket)
        .context("Successfully performed hot-plug command, but failed to send the response")?;

    Ok(())
}

fn handle_detach(
    bus: u8,
    dev: u8,
    socket: &mut UnixStream,
    hotplug_control: &HotplugControl<CompleteRealDeviceImpl<NusbRealDevice, (u8, u8)>>,
    async_runtime: &runtime::Handle,
) -> Result<()> {
    let response = async_runtime.block_on(hotplug_control.detach((bus, dev)));
    response
        .send_over_socket(socket)
        .context("Successfully performed detach command, but failed to send the response")?;

    Ok(())
}
