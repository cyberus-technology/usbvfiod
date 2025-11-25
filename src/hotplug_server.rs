use std::{
    fs::File,
    os::unix::net::{UnixListener, UnixStream},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use nusb::MaybeFuture;
use tracing::{debug, warn};
use usbvfiod::hotplug_protocol::{command::Command, response::Response};

use crate::device::pci::{
    nusb::NusbDeviceWrapper, realdevice::IdentifiableRealDevice, xhci::XhciController,
};

pub fn run_hotplug_server(socket: UnixListener, xhci_controller: Arc<Mutex<XhciController>>) {
    loop {
        if let Ok((mut stream, _addr)) = socket.accept() {
            match Command::receive_from_socket(&stream) {
                Ok(command) => {
                    debug!("Received command {:?} on hotplug socket", command);
                    if let Err(e) = handle_command(command, &mut stream, xhci_controller.clone()) {
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
    xhci_controller: Arc<Mutex<XhciController>>,
) -> Result<()> {
    match command {
        Command::Attach {
            bus,
            device: dev,
            fd,
        } => handle_attach(bus, dev, fd, socket, xhci_controller)
            .context("Failed to handle attach command")?,
        Command::List => {
            let devices = xhci_controller.lock().unwrap().attached_devices();
            Response::ListFollowing
                .send_device_list(devices, socket)
                .context("Failed to handle list command")?;
        }
        _ => todo!(),
    };

    Ok(())
}

fn handle_attach(
    bus: u8,
    dev: u8,
    fd: File,
    socket: &mut UnixStream,
    controller: Arc<Mutex<XhciController>>,
) -> Result<()> {
    let device = nusb::Device::from_fd(fd.into())
        .wait()
        .context("Failed to open nusb device from the supplied file descriptor")?;
    let wrapped_device = Box::new(NusbDeviceWrapper::new(device));
    let response = controller
        .lock()
        .unwrap()
        .attach_device(IdentifiableRealDevice {
            bus_number: bus,
            device_number: dev,
            real_device: wrapped_device,
        })
        .unwrap_or_else(|response| response);
    response
        .send_over_socket(socket)
        .context("Successfully performed hot-plug command, but failed to send the response")?;

    Ok(())
}
