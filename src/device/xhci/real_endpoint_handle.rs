use std::{fmt::Debug, future::Future};

use crate::device::pci::usbrequest::UsbRequest;

pub trait RealControlEndpointHandle: Debug + Send + Sync + 'static {
    type TrbCompletionFuture<'a>: Future<Output = ControlRequestProcessingResult> + Send + 'a;
    type CompletionFuture<'a>: Future<Output = ()> + Send + 'a;

    fn submit_control_request(&mut self, request: UsbRequest);
    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_>;
    fn cancel(&mut self) -> Self::CompletionFuture<'_>;
    fn clear_halt(&mut self) -> Self::CompletionFuture<'_>;
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
    type TrbCompletionFuture<'a>: Future<Output = OutTrbProcessingResult> + Send + 'a;
    type CompletionFuture<'a>: Future<Output = ()> + Send + 'a;

    fn submit(&mut self, data: Vec<u8>);
    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_>;
    fn cancel(&mut self) -> Self::CompletionFuture<'_>;
    fn clear_halt(&mut self) -> Self::CompletionFuture<'_>;
}

#[derive(Debug)]
pub enum OutTrbProcessingResult {
    Disconnect,
    Stall,
    TransactionError,
    Success,
}

pub trait RealInEndpointHandle: Debug + Send + Sync + 'static {
    type TrbCompletionFuture<'a>: Future<Output = InTrbProcessingResult> + Send + 'a;
    type CompletionFuture<'a>: Future<Output = ()> + Send + 'a;

    fn submit(&mut self, data: usize);
    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_>;
    fn cancel(&mut self) -> Self::CompletionFuture<'_>;
    fn clear_halt(&mut self) -> Self::CompletionFuture<'_>;
}

#[derive(Debug)]
pub enum InTrbProcessingResult {
    Disconnect,
    Stall,
    TransactionError,
    Success(Vec<u8>),
}
