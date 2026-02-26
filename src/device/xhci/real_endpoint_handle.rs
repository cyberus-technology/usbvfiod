use std::{fmt::Debug, future::Future, pin::Pin};

use crate::device::pci::usbrequest::UsbRequest;

pub trait RealControlEndpointHandle: Debug + Send + Sync + 'static {
    fn submit_control_request(&mut self, request: UsbRequest);
    fn next_completion(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = ControlRequestProcessingResult> + Send + '_>>;
    fn cancel(&mut self);
    fn clear_halt(&mut self);
}

#[derive(Debug)]
pub enum ControlRequestProcessingResult {
    Disconnect,
    Stall,
    TransactionError,
    SuccessfulControlIn(Vec<u8>),
    SuccessfulControlOut,
}

pub trait RealOutEndpointHandle: Debug + Send + Sync + 'static {
    fn submit(&mut self, data: Vec<u8>);
    fn next_completion(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = OutTrbProcessingResult> + Send + '_>>;
    fn cancel(&mut self);
    fn clear_halt(&mut self);
}

#[derive(Debug)]
pub enum OutTrbProcessingResult {
    Disconnect,
    Stall,
    TransactionError,
    Success,
}

pub trait RealInEndpointHandle: Debug + Send + Sync + 'static {
    fn submit(&mut self, data: usize);
    fn next_completion(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = InTrbProcessingResult> + Send + '_>>;
    fn cancel(&mut self);
    fn clear_halt(&mut self);
}

#[derive(Debug)]
pub enum InTrbProcessingResult {
    Disconnect,
    Stall,
    TransactionError,
    Success(Vec<u8>),
}
