use std::fmt;

use nusb::transfer::{ControlIn, ControlOut, ControlType, Recipient};

use super::request::Request;

#[derive(Clone)]
pub struct RealUsbDevice {
    device: nusb::Device,
}

impl fmt::Debug for RealUsbDevice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("nusb device").finish()
    }
}

impl RealUsbDevice {
    pub const fn new(device: nusb::Device) -> Self {
        Self { device }
    }

    pub fn send_control_request(&self, request: Request) {
        if request.request_type == 0x8000 {
            // IN
            let slice = unsafe {
                core::slice::from_raw_parts_mut(
                    request.data.unwrap() as *mut u8,
                    request.length as usize,
                );
            };
            self.device.control_in_blocking(
                ControlIn {
                    control_type: ControlType::Standard,
                    recipient: Recipient::Device,
                    request: request.request,
                    value: request.value,
                    index: request.index,
                    length: request.length,
                },
                slice,
                Duration::from_millis(100),
            );
        } else if request.request_type == 0x0 {
            // OUT
            let slice = unsafe {
                core::slice::from_raw_parts_mut(
                    request.data.unwrap() as *mut u8,
                    request.length as usize,
                );
            };
            self.device.control_out_blocking(
                ControlOut {
                    control_type: ControlType::Standard,
                    recipient: Recipient::Device,
                    request: request.request,
                    value: request.value,
                    index: request.index,
                    data: slice,
                },
                slice,
                Duration::from_millis(100),
            );
        }
        self.device.control_
    }
}
