use crate::device::bus::BusDeviceRef;

use super::{
    trb::{EventTrb, TransferTrb},
    usbrequest::UsbRequest,
};
use std::fmt::Debug;

pub trait RealDevice: Debug {
    fn control_transfer(&self, request: &UsbRequest, dma_bus: &BusDeviceRef);
    fn out(&self, trb: &TransferTrb, dma_bus: &BusDeviceRef) -> Option<EventTrb>;
    fn in_(&self, trb: &TransferTrb, dma_bus: &BusDeviceRef) -> Option<EventTrb>;
}
