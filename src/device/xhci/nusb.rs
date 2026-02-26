use std::{fmt::Debug, future::Future, pin::Pin, time::Duration};

use nusb::{
    transfer::{
        Buffer, BulkOrInterrupt, ControlIn, ControlOut, ControlType, EndpointDirection,
        EndpointType, In, Out, Recipient, TransferError,
    },
    Endpoint, MaybeFuture,
};
use tokio::{runtime, select, sync::mpsc};
use tokio_util::sync::CancellationToken;

use crate::device::{
    pci::usbrequest::UsbRequest,
    xhci::real_endpoint_handle::{
        ControlRequestProcessingResult, InTrbProcessingResult, RealControlEndpointHandle,
        RealInEndpointHandle, RealOutEndpointHandle,
    },
};

use super::real_endpoint_handle::OutTrbProcessingResult;

#[derive(Debug)]
struct ControlEndpointHandle {
    cancel: CancellationToken,
    request_submitter: mpsc::Sender<UsbRequest>,
    response_receiver: mpsc::Receiver<ControlRequestProcessingResult>,
}

impl RealControlEndpointHandle for ControlEndpointHandle {
    fn submit_control_request(&mut self, request: UsbRequest) {
        self.request_submitter.send(request);
    }

    fn next_completion(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = ControlRequestProcessingResult> + Send + '_>> {
        Box::pin(async {
            match self.response_receiver.recv().await {
                Some(res) => res,
                None => ControlRequestProcessingResult::TransactionError,
            }
        })
    }

    fn cancel(&mut self) {
        // nothing we can do
    }

    fn clear_halt(&mut self) {
        // nusb handles clearing halts on control endpoints by itself
    }
}

impl ControlEndpointHandle {
    fn new(device: nusb::Device, async_runtime: runtime::Handle) -> Self {
        let (request_submitter, request_receiver) = mpsc::channel(10);
        let (response_submitter, response_receiver) = mpsc::channel(10);
        let cancel = CancellationToken::new();

        async_runtime.spawn(cancellable_control_endpoint_worker(
            device,
            request_receiver,
            response_submitter,
            cancel.clone(),
        ));

        Self {
            cancel,
            request_submitter,
            response_receiver,
        }
    }
}

async fn cancellable_control_endpoint_worker(
    device: nusb::Device,
    request_receiver: mpsc::Receiver<UsbRequest>,
    response_submitter: mpsc::Sender<ControlRequestProcessingResult>,
    cancel: CancellationToken,
) {
    select! {
        _ = control_endpoint_worker(device, request_receiver, response_submitter) => {},
        _ = cancel.cancelled() => {},
    }
}

async fn control_endpoint_worker(
    device: nusb::Device,
    mut request_receiver: mpsc::Receiver<UsbRequest>,
    response_submitter: mpsc::Sender<ControlRequestProcessingResult>,
) {
    loop {
        if let Some(request) = request_receiver.recv().await {
            let (recipient, control_type) = extract_recipient_and_type(request.request_type);
            let is_out_request = request.request_type & 0x80 == 0;

            let processing_result = match is_out_request {
                true => {
                    let data = request.data.unwrap_or(Vec::new());
                    let control = ControlOut {
                        control_type,
                        recipient,
                        request: request.request,
                        value: request.value,
                        index: request.index,
                        data: &data,
                    };
                    match device
                        .control_out(control, Duration::from_millis(2000))
                        .await
                    {
                        Ok(_) => ControlRequestProcessingResult::SuccessfulControlOut,
                        Err(err) => map_error(err),
                    }
                }
                false => {
                    let control = ControlIn {
                        control_type,
                        recipient,
                        request: request.request,
                        value: request.value,
                        index: request.index,
                        length: request.length,
                    };
                    match device
                        .control_in(control, Duration::from_millis(2000))
                        .await
                    {
                        Ok(data) => ControlRequestProcessingResult::SuccessfulControlIn(data),
                        Err(err) => map_error(err),
                    }
                }
            };

            response_submitter.send(processing_result);
        }
    }
}

fn map_error(error: TransferError) -> ControlRequestProcessingResult {
    match error {
        TransferError::Cancelled => ControlRequestProcessingResult::TransactionError,
        TransferError::Stall => ControlRequestProcessingResult::Stall,
        TransferError::Disconnected => ControlRequestProcessingResult::Disconnect,
        TransferError::Fault => ControlRequestProcessingResult::TransactionError,
        TransferError::InvalidArgument => ControlRequestProcessingResult::TransactionError,
        TransferError::Unknown(_) => ControlRequestProcessingResult::TransactionError,
    }
}

fn extract_recipient_and_type(request_type: u8) -> (Recipient, ControlType) {
    let recipient = match request_type & 0x1f {
        0 => Recipient::Device,
        1 => Recipient::Interface,
        2 => Recipient::Endpoint,
        val => panic!("invalid recipient {val}"),
    };
    let control_type = match (request_type >> 5) & 0x3 {
        0 => ControlType::Standard,
        1 => ControlType::Class,
        2 => ControlType::Vendor,
        val => panic!("invalid type {val}"),
    };
    (recipient, control_type)
}

struct NormalEndpointHandle<EpType: EndpointType + 'static, Dir: EndpointDirection + 'static> {
    id: u8,
    device: nusb::Device,
    endpoint: Option<Endpoint<EpType, Dir>>,
}

impl<EpType: EndpointType, Dir: EndpointDirection> Debug for NormalEndpointHandle<EpType, Dir> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("lol")
            //.field("device", &self.device.active_configuration())
            .finish()
    }
}

impl<EpType: EndpointType, Dir: EndpointDirection> NormalEndpointHandle<EpType, Dir> {
    fn endpoint(&mut self) -> &mut Endpoint<EpType, Dir> {
        match self.endpoint {
            Some(ref mut endpoint) => endpoint,
            None => {
                let ep: Endpoint<EpType, Dir> = open_endpoint(0, &mut self.device);
                self.endpoint = Some(ep);
                self.endpoint.as_mut().unwrap()
            }
        }
    }
}

impl<EpType: BulkOrInterrupt> RealOutEndpointHandle for NormalEndpointHandle<EpType, Out> {
    fn submit(&mut self, data: Vec<u8>) {
        let buf = Buffer::from(data);
        self.endpoint().submit(buf);
    }

    fn next_completion(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = OutTrbProcessingResult> + Send + '_>> {
        Box::pin(async {
            let completion = self.endpoint().next_complete().await;
            match completion.status {
                Err(err) => match err {
                    TransferError::Cancelled => OutTrbProcessingResult::TransactionError,
                    TransferError::Stall => OutTrbProcessingResult::Stall,
                    TransferError::Disconnected => OutTrbProcessingResult::Disconnect,
                    TransferError::Fault => OutTrbProcessingResult::TransactionError,
                    TransferError::InvalidArgument => OutTrbProcessingResult::TransactionError,
                    TransferError::Unknown(_) => OutTrbProcessingResult::TransactionError,
                },
                Ok(_) => OutTrbProcessingResult::Success,
            }
        })
    }

    fn cancel(&mut self) {
        self.endpoint().cancel_all();
    }

    fn clear_halt(&mut self) {
        self.endpoint().clear_halt();
    }
}

impl<EpType: BulkOrInterrupt> RealInEndpointHandle for NormalEndpointHandle<EpType, In> {
    fn submit(&mut self, len: usize) {
        let endpoint = self.endpoint();
        let request_len = determine_buffer_size(len, endpoint.max_packet_size());
        let buf = Buffer::new(request_len);
        endpoint.submit(buf);
    }

    fn next_completion(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = InTrbProcessingResult> + Send + '_>> {
        Box::pin(async {
            let completion = self.endpoint().next_complete().await.into_result();
            match completion {
                Ok(buf) => InTrbProcessingResult::Success(buf.into_vec()),
                Err(err) => match err {
                    TransferError::Cancelled => InTrbProcessingResult::TransactionError,
                    TransferError::Stall => InTrbProcessingResult::Stall,
                    TransferError::Disconnected => InTrbProcessingResult::Disconnect,
                    TransferError::Fault => InTrbProcessingResult::TransactionError,
                    TransferError::InvalidArgument => InTrbProcessingResult::TransactionError,
                    TransferError::Unknown(_) => InTrbProcessingResult::TransactionError,
                },
            }
        })
    }

    fn cancel(&mut self) {
        self.endpoint().cancel_all();
    }

    fn clear_halt(&mut self) {
        self.endpoint().clear_halt();
    }
}

fn open_endpoint<EpType: EndpointType, Dir: EndpointDirection>(
    endpoint_id: u8,
    device: &mut nusb::Device,
) -> Endpoint<EpType, Dir> {
    let endpoint_address = endpoint_id_to_address(endpoint_id);
    device
        .claim_interface(0)
        .wait()
        .unwrap()
        .endpoint(endpoint_address)
        .unwrap()
}

const fn endpoint_id_to_address(endpoint_id: u8) -> u8 {
    let endpoint_number = endpoint_id / 2;
    let is_out_endpoint = endpoint_id.is_multiple_of(2);

    match is_out_endpoint {
        true => endpoint_number,
        false => 0x80 | endpoint_number,
    }
}

const fn determine_buffer_size(guest_transfer_length: usize, max_packet_size: usize) -> usize {
    if guest_transfer_length <= max_packet_size {
        max_packet_size
    } else {
        guest_transfer_length.div_ceil(max_packet_size) * max_packet_size
    }
}
