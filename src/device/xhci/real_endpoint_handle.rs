use std::{fmt::Debug, future::Future};

use crate::device::xhci::{hotplug_endpoint_handle::BaseEndpointHandle, usbrequest::UsbRequest};

pub trait RealControlEndpointHandle: BaseEndpointHandle {
    type TrbCompletionFuture<'a>: Future<Output = anyhow::Result<ControlRequestProcessingResult>>
        + Send
        + 'a;

    fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()>;
    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_>;
}

#[derive(Debug)]
pub enum ControlRequestProcessingResult {
    Disconnect,
    Stall,
    TransactionError,
    SuccessfulControlIn(Vec<u8>),
    SuccessfulControlOut,
}

pub trait RealOutEndpointHandle: BaseEndpointHandle {
    type TrbCompletionFuture<'a>: Future<Output = anyhow::Result<OutTrbProcessingResult>>
        + Send
        + 'a;

    fn submit(&mut self, data: Vec<u8>) -> anyhow::Result<()>;
    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_>;
}

#[derive(Debug)]
pub enum OutTrbProcessingResult {
    Disconnect,
    Stall,
    TransactionError,
    Success,
}

pub trait RealInEndpointHandle: BaseEndpointHandle {
    type TrbCompletionFuture<'a>: Future<Output = anyhow::Result<InTrbProcessingResult>> + Send + 'a;

    fn submit(&mut self, data: usize) -> anyhow::Result<()>;
    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_>;
}

#[derive(Debug)]
pub enum InTrbProcessingResult {
    Disconnect,
    Stall,
    TransactionError,
    Success(Vec<u8>),
}
