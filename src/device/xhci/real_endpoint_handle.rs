use std::{fmt::Debug, future::Future, pin::Pin};

use crate::device::pci::usbrequest::UsbRequest;

pub trait RealControlEndpointHandle: Debug + Send + Sync + 'static {
    type CompletionFuture<'a>: Future<Output = ControlRequestProcessingResult> + Send + 'a;

    fn submit_control_request(&mut self, request: UsbRequest);
    fn next_completion(&mut self) -> Self::CompletionFuture<'_>;
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
    type CompletionFuture<'a>: Future<Output = OutTrbProcessingResult> + Send + 'a;

    fn submit(&mut self, data: Vec<u8>);
    fn next_completion(&mut self) -> Self::CompletionFuture<'_>;
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
    type CompletionFuture<'a>: Future<Output = InTrbProcessingResult> + Send + 'a;

    fn submit(&mut self, data: usize);
    fn next_completion(&mut self) -> Self::CompletionFuture<'_>;
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
