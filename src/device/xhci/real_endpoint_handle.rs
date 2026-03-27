use std::{fmt::Debug, future::Future};

use crate::device::xhci::usbrequest::UsbRequest;

pub trait RealControlEndpointHandle: Debug + Send + Sync + 'static {
    type TrbCompletionFuture<'a>: Future<Output = anyhow::Result<ControlRequestProcessingResult>>
        + Send
        + 'a;
    type CompletionFuture<'a>: Future<Output = anyhow::Result<()>> + Send + 'a;

    fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()>;
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
    type TrbCompletionFuture<'a>: Future<Output = anyhow::Result<OutTrbProcessingResult>>
        + Send
        + 'a;
    type CompletionFuture<'a>: Future<Output = anyhow::Result<()>> + Send + 'a;

    fn submit(&mut self, data: Vec<u8>) -> anyhow::Result<()>;
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
    type TrbCompletionFuture<'a>: Future<Output = anyhow::Result<InTrbProcessingResult>> + Send + 'a;
    type CompletionFuture<'a>: Future<Output = anyhow::Result<()>> + Send + 'a;

    fn submit(&mut self, data: usize) -> anyhow::Result<()>;
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
