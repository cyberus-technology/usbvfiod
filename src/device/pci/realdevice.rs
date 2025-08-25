use crate::device::bus::BusDeviceRef;

use super::{
    trb::{CompletionCode, TransferTrb},
    usbrequest::UsbRequest,
};
use std::fmt::Debug;

pub trait RealDevice: Debug {
    fn control_transfer(&self, request: &UsbRequest, dma_bus: &BusDeviceRef);
    fn enable_endpoint(&mut self, endpoint_id: u8);
    fn transfer_out(&mut self, trb: &TransferTrb, dma_bus: &BusDeviceRef) -> (CompletionCode, u32);
    fn transfer_in(&mut self, trb: &TransferTrb, dma_bus: &BusDeviceRef) -> (CompletionCode, u32);
}
