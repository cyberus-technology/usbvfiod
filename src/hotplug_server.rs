use std::{
    os::unix::net::UnixListener,
    sync::{Arc, Mutex},
};

use nusb::MaybeFuture;
use tracing::warn;
use usbvfiod::hotplug_protocol::command::Command;

use crate::device::pci::{
    nusb::NusbDeviceWrapper, realdevice::IdentifiableRealDevice, xhci::XhciController,
};

pub fn run_hotplug_server(socket: UnixListener, xhci_controller: Arc<Mutex<XhciController>>) {
    loop {
        let (mut stream, _addr) = socket.accept().unwrap();
        match Command::receive_from_socket(&stream) {
            Ok(Command::Attach {
                bus,
                device: dev,
                fd,
            }) => {
                let device = nusb::Device::from_fd(fd.into()).wait().unwrap();
                let wrapped_device = Box::new(NusbDeviceWrapper::new(device));
                let response = xhci_controller
                    .lock()
                    .unwrap()
                    .attach_device(IdentifiableRealDevice {
                        bus_number: bus,
                        device_number: dev,
                        real_device: wrapped_device,
                    })
                    .unwrap_or_else(|response| response);
                if let Err(e) = response.send_over_socket(&mut stream) {
                    warn!("Successfully performed hot-plug command, but failed to send the response {}", e);
                }
            }
            Ok(_) => todo!(),
            Err(e) => warn!("Error occurred while reading a hotplug command {}", e),
        }
    }
}
