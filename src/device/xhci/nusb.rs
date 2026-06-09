use std::{
    cmp::min, fmt::Debug, future::Future, pin::Pin, sync::Arc, thread::sleep, time::Duration,
};

use anyhow::{anyhow, Error};
use nusb::{
    transfer::{
        Buffer, Bulk, BulkOrInterrupt, ControlIn, ControlOut, ControlType, EndpointDirection,
        EndpointType, In, Interrupt, Out, Recipient, TransferError,
    },
    Endpoint, Interface, MaybeFuture,
};
use tokio::{io::AsyncReadExt, runtime, select, sync::mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

use crate::device::xhci::{
    hotplug_endpoint_handle::BaseEndpointHandle,
    real_device::{RealDevice, Speed},
    real_endpoint_handle::{
        ControlRequestProcessingResult, InTrbProcessingResult, RealControlEndpointHandle,
        RealInEndpointHandle, RealOutEndpointHandle,
    },
    usbrequest::UsbRequest,
};

use super::real_endpoint_handle::OutTrbProcessingResult;

struct NusbDeviceWrapper {
    device: nusb::Device,
    interfaces: Vec<Interface>,
}

impl Debug for NusbDeviceWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The active configuration is either cached or not available
        // for unconfigured devices. There is no I/O for this.
        f.debug_struct("NusbDeviceWrapper")
            .field("device", &self.device.active_configuration())
            .finish()
    }
}

impl TryFrom<nusb::Device> for NusbDeviceWrapper {
    type Error = Error;

    fn try_from(device: nusb::Device) -> Result<Self, Error> {
        // Claim all interfaces
        let mut interfaces = vec![];
        let desc = device.active_configuration()?;
        for interface in desc.interfaces() {
            let interface_number = interface.interface_number();
            debug!("Claiming interface {}", interface_number);
            interfaces.push(device.detach_and_claim_interface(interface_number).wait()?);
        }

        Ok(Self { device, interfaces })
    }
}

impl NusbDeviceWrapper {
    fn get_interface_number_containing_endpoint(&self, endpoint_id: u8) -> Option<usize> {
        self.interfaces.iter().position(|interface| {
            interface
                .descriptor()
                .unwrap()
                .endpoints()
                .any(|ep| ep.address() == endpoint_id)
        })
    }

    fn open_endpoint<EpType: EndpointType, Dir: EndpointDirection>(
        &self,
        endpoint_id: u8,
    ) -> Result<Endpoint<EpType, Dir>, Error> {
        let endpoint_address = endpoint_id_to_address(endpoint_id);
        let interface_num = self
            .get_interface_number_containing_endpoint(endpoint_address)
            .ok_or_else(|| anyhow!("Endpoint with id {endpoint_id} is not part of an interface"))?;
        // TODO retry this because of race condition after sending a stop signal to a worker
        for n in 1..1000 {
            if let Ok(endpoint) = self.interfaces[interface_num].endpoint(endpoint_address) {
                return Ok(endpoint);
            }
            warn!("endpoint currently not available; this was try nr {n}");

            sleep(Duration::new(0, 100));
        }
        panic!("unable to open endpoint");
    }
}

const fn endpoint_id_to_address(endpoint_id: u8) -> u8 {
    // NOTE: This only applies to normal endpoints!
    // `endpoint_id / 2` yields the USB endpoint number, while the low bit of the
    // xHCI endpoint id distinguishes OUT (even) from IN (odd). Rotating right by
    // one moves that direction bit into the USB IN position (0x80) and leaves the
    // endpoint number in the low bits.
    endpoint_id.rotate_right(1)
}

#[derive(Debug)]
pub struct NusbRealDevice {
    device_wrapper: Arc<NusbDeviceWrapper>,
    async_runtime: runtime::Handle,
}

impl NusbRealDevice {
    pub fn try_new(device: nusb::Device, async_runtime: runtime::Handle) -> Result<Self, Error> {
        let device_wrapper = NusbDeviceWrapper::try_from(device)?;

        Ok(Self {
            device_wrapper: Arc::new(device_wrapper),
            async_runtime,
        })
    }
}

impl RealDevice for NusbRealDevice {
    type RCEH = ControlEndpointHandle;
    type RBIEH = NormalInEndpointHandle;
    type RBOEH = NormalEndpointHandle<Bulk, Out>;
    type RIIEH = NormalInEndpointHandle;
    type RIOEH = NormalEndpointHandle<Interrupt, Out>;

    fn speed(&self) -> Option<super::real_device::Speed> {
        self.device_wrapper.device.speed().map(|speed| speed.into())
    }

    fn control_endpoint_handle(&self) -> Self::RCEH {
        ControlEndpointHandle::new(self.device_wrapper.device.clone(), &self.async_runtime)
    }

    fn bulk_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RBIEH {
        let endpoint: Endpoint<Bulk, In> = self.device_wrapper.open_endpoint(endpoint_id).expect("Failed to open endpoint on nusb device. We could handle this error and always return transaction errors. Panic is fine for now.");
        NormalInEndpointHandle::new(endpoint, &self.async_runtime)
    }

    fn bulk_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RBOEH {
        NormalEndpointHandle::new(endpoint_id, self.device_wrapper.clone())
    }

    fn interrupt_in_endpoint_handle(&self, endpoint_id: u8) -> Self::RIIEH {
        let endpoint: Endpoint<Interrupt, In> = self.device_wrapper.open_endpoint(endpoint_id).expect("Failed to open endpoint on nusb device. We could handle this error and always return transaction errors. Panic is fine for now.");
        NormalInEndpointHandle::new(endpoint, &self.async_runtime)
    }

    fn interrupt_out_endpoint_handle(&self, endpoint_id: u8) -> Self::RIOEH {
        NormalEndpointHandle::new(endpoint_id, self.device_wrapper.clone())
    }
}

#[derive(Debug)]
pub struct ControlEndpointHandle {
    // signal worker to stop
    cancel: CancellationToken,
    request_submitter: mpsc::UnboundedSender<UsbRequest>,
    response_receiver: mpsc::UnboundedReceiver<ControlRequestProcessingResult>,
}

impl Drop for ControlEndpointHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl RealControlEndpointHandle for ControlEndpointHandle {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<ControlRequestProcessingResult>> + Send + 'a>>;

    fn submit_control_request(&mut self, request: UsbRequest) -> anyhow::Result<()> {
        self.request_submitter.send(request)?;

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let result = (self.response_receiver.recv().await)
                // background worker is dead and has dropped the response sender
                // maybe we want to error here instead
                .map_or(ControlRequestProcessingResult::TransactionError, |res| res);

            Ok(result)
        })
    }
}

impl BaseEndpointHandle for ControlEndpointHandle {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        // nothing we can do
        Box::pin(async { Ok(()) })
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        // nusb handles clearing halts on control endpoints by itself
        Box::pin(async { Ok(()) })
    }
}

impl ControlEndpointHandle {
    fn new(device: nusb::Device, async_runtime: &runtime::Handle) -> Self {
        let (request_submitter, request_receiver) = mpsc::unbounded_channel();
        let (response_submitter, response_receiver) = mpsc::unbounded_channel();
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
    request_receiver: mpsc::UnboundedReceiver<UsbRequest>,
    response_submitter: mpsc::UnboundedSender<ControlRequestProcessingResult>,
    cancel: CancellationToken,
) {
    select! {
        _ = control_endpoint_worker(device, request_receiver, response_submitter) => {},
        _ = cancel.cancelled() => {},
    }
}

// this function can only return with an error, but ! cannot be used in Result
async fn control_endpoint_worker(
    device: nusb::Device,
    mut request_receiver: mpsc::UnboundedReceiver<UsbRequest>,
    response_submitter: mpsc::UnboundedSender<ControlRequestProcessingResult>,
) -> anyhow::Result<()> {
    loop {
        if let Some(request) = request_receiver.recv().await {
            let (recipient, control_type) = extract_recipient_and_type(request.request_type);
            let is_out_request = request.request_type & 0x80 == 0;

            let processing_result = match is_out_request {
                true => {
                    let data = request.data.unwrap_or(Vec::new());
                    trace!("nusb file: doing controlout");

                    let control = ControlOut {
                        control_type,
                        recipient,
                        request: request.request,
                        value: request.value,
                        index: request.index,
                        data: &data,
                    };
                    trace!("{:?}", control);
                    match device
                        .control_out(control, Duration::from_millis(2000))
                        .await
                    {
                        Ok(_) => ControlRequestProcessingResult::SuccessfulControlOut,
                        Err(err) => {
                            trace!("mapping {:?}", err);
                            map_error(err)
                        }
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

            response_submitter.send(processing_result)?;
        }
    }
}

const fn map_error(error: TransferError) -> ControlRequestProcessingResult {
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

pub struct NormalEndpointHandle<EpType: EndpointType + 'static, Dir: EndpointDirection + 'static> {
    id: u8,
    device_wrapper: Arc<NusbDeviceWrapper>,
    endpoint: Option<Endpoint<EpType, Dir>>,
}

impl<EpType: EndpointType, Dir: EndpointDirection> NormalEndpointHandle<EpType, Dir> {
    const fn new(id: u8, device_wrapper: Arc<NusbDeviceWrapper>) -> Self {
        Self {
            id,
            device_wrapper,
            endpoint: None,
        }
    }
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
                let ep: Endpoint<EpType, Dir> = self.device_wrapper.open_endpoint(self.id).expect("Failed to open endpoint on nusb device. We could handle this error and always return transaction errors. Panic is fine for now.");
                self.endpoint = Some(ep);
                self.endpoint.as_mut().unwrap()
            }
        }
    }
}

impl<EpType: BulkOrInterrupt> RealOutEndpointHandle for NormalEndpointHandle<EpType, Out> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<OutTrbProcessingResult>> + Send + 'a>>;

    fn submit(&mut self, data: Vec<u8>) -> anyhow::Result<()> {
        let buf = Buffer::from(data);
        self.endpoint().submit(buf);

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let completion = self.endpoint().next_complete().await;
            let result = match completion.status {
                Err(err) => match err {
                    TransferError::Cancelled => OutTrbProcessingResult::TransactionError,
                    TransferError::Stall => OutTrbProcessingResult::Stall,
                    TransferError::Disconnected => OutTrbProcessingResult::Disconnect,
                    TransferError::Fault => OutTrbProcessingResult::TransactionError,
                    TransferError::InvalidArgument => OutTrbProcessingResult::TransactionError,
                    TransferError::Unknown(_) => OutTrbProcessingResult::TransactionError,
                },
                Ok(_) => OutTrbProcessingResult::Success,
            };

            Ok(result)
        })
    }
}

impl<EpType: BulkOrInterrupt> RealInEndpointHandle for NormalEndpointHandle<EpType, In> {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<InTrbProcessingResult>> + Send + 'a>>;

    fn submit(&mut self, len: usize) -> anyhow::Result<()> {
        let endpoint = self.endpoint();
        let request_len = determine_buffer_size(len, endpoint.max_packet_size());
        let buf = Buffer::new(request_len);
        endpoint.submit(buf);

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let completion = self.endpoint().next_complete().await.into_result();
            let result = match completion {
                Ok(buf) => InTrbProcessingResult::Success(buf.into_vec()),
                Err(err) => match err {
                    TransferError::Cancelled => InTrbProcessingResult::TransactionError,
                    TransferError::Stall => InTrbProcessingResult::Stall,
                    TransferError::Disconnected => InTrbProcessingResult::Disconnect,
                    TransferError::Fault => InTrbProcessingResult::TransactionError,
                    TransferError::InvalidArgument => InTrbProcessingResult::TransactionError,
                    TransferError::Unknown(_) => InTrbProcessingResult::TransactionError,
                },
            };

            Ok(result)
        })
    }
}

impl<EpType: BulkOrInterrupt, Dir: EndpointDirection> BaseEndpointHandle
    for NormalEndpointHandle<EpType, Dir>
{
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async {
            let ep = self.endpoint();
            ep.cancel_all();

            // have to consume all cancelled TRBs (should be 0 or 1)
            if ep.pending() > 1 {
                warn!("while cancelling: saw more than one pending TRB");
            }
            while ep.pending() > 0 {
                let _ = ep.next_complete().await;
            }

            Ok(())
        })
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async {
            self.endpoint().clear_halt().await?;
            Ok(())
        })
    }
}

const fn determine_buffer_size(guest_transfer_length: usize, max_packet_size: usize) -> usize {
    if guest_transfer_length <= max_packet_size {
        max_packet_size
    } else {
        guest_transfer_length.div_ceil(max_packet_size) * max_packet_size
    }
}

impl From<nusb::Speed> for Speed {
    fn from(value: nusb::Speed) -> Self {
        match value {
            nusb::Speed::Low => Self::Low,
            nusb::Speed::Full => Self::Full,
            nusb::Speed::High => Self::High,
            nusb::Speed::Super => Self::Super,
            nusb::Speed::SuperPlus => Self::SuperPlus,
            _ => todo!("new USB speed was added to non-exhaustive enum"),
        }
    }
}

pub struct NormalInEndpointHandle {
    // signal worker to stop
    cancel: CancellationToken,
    request_submitter: mpsc::UnboundedSender<usize>,
    response_receiver: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl Drop for NormalInEndpointHandle {
    fn drop(&mut self) {
        trace!("called Drop for NormalInEndpointHandle");
        self.cancel.cancel();
    }
}

impl Debug for NormalInEndpointHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("lol v2").finish()
    }
}

impl RealInEndpointHandle for NormalInEndpointHandle {
    type TrbCompletionFuture<'a> =
        Pin<Box<dyn Future<Output = anyhow::Result<InTrbProcessingResult>> + Send + 'a>>;

    fn submit(&mut self, len: usize) -> anyhow::Result<()> {
        self.request_submitter.send(len)?;

        Ok(())
    }

    fn next_completion(&mut self) -> Self::TrbCompletionFuture<'_> {
        Box::pin(async {
            let result = (self.response_receiver.recv().await)
                // background worker is dead and has dropped the response sender
                // maybe we want to error here instead
                .map_or(InTrbProcessingResult::TransactionError, |res| {
                    InTrbProcessingResult::Success(res)
                });

            Ok(result)
        })
    }
}

impl BaseEndpointHandle for NormalInEndpointHandle {
    type CompletionFuture<'a> = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>>;

    fn cancel(&mut self) -> Self::CompletionFuture<'_> {
        // nothing we can do
        Box::pin(async { Ok(()) })
    }

    fn clear_halt(&mut self) -> Self::CompletionFuture<'_> {
        Box::pin(async { todo!("clear halt") })
    }
}

impl NormalInEndpointHandle {
    fn new<EpType: BulkOrInterrupt + 'static>(
        endpoint: nusb::Endpoint<EpType, In>,
        async_runtime: &runtime::Handle,
    ) -> Self {
        let (request_submitter, request_receiver) = mpsc::unbounded_channel();
        let (response_submitter, response_receiver) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();

        async_runtime.spawn(cancellable_normal_in_endpoint_worker::<EpType>(
            endpoint,
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

async fn cancellable_normal_in_endpoint_worker<EpType: BulkOrInterrupt + 'static>(
    endpoint: nusb::Endpoint<EpType, In>,
    request_receiver: mpsc::UnboundedReceiver<usize>,
    response_submitter: mpsc::UnboundedSender<Vec<u8>>,
    cancel: CancellationToken,
) {
    select! {
        _ = normal_in_endpoint_worker(endpoint, request_receiver, response_submitter) => {},
        _ = cancel.cancelled() => {trace!("used the cancel token for an endpoint worker");},
    }
}

// this function can only return with an error, but ! cannot be used in Result
async fn normal_in_endpoint_worker<EpType: BulkOrInterrupt + 'static>(
    endpoint: nusb::Endpoint<EpType, In>,
    mut request_receiver: mpsc::UnboundedReceiver<usize>,
    response_submitter: mpsc::UnboundedSender<Vec<u8>>, // TODO into a processing result instead of a vec u8?
) -> anyhow::Result<()> {
    use nusb::transfer::{Bulk, Interrupt};
    use std::any::TypeId;

    // seconds * N
    const NUSB_TIMEOUT: u64 = 1000 * 200;

    let mut reader;
    let size;

    if TypeId::of::<EpType>() == TypeId::of::<Bulk>() {
        trace!("creating normal in worker for bulk");
        size = 512 * 2 * 5;
        reader = endpoint
            .reader(size)
            .with_num_transfers(1)
            .with_read_timeout(Duration::from_millis(NUSB_TIMEOUT));

        let mut pkt_reader = reader.until_short_packet();

        let mut buffer = Vec::new();

        let mut debug_counter = 0;

        loop {
            debug_counter += 1;
            trace!(
                "normal_in_endpoint_worker loop; counter at {}",
                debug_counter
            );

            if let Some(requested_length) = request_receiver.recv().await {
                trace!("original requested length: {requested_length}");
                trace!("current buffer length: {}", buffer.len());

                // do reading until end aka short or null package
                let read_length = if requested_length > buffer.len() && !pkt_reader.is_end() {
                    trace!("attempting a read");

                    match pkt_reader.read_to_end(&mut buffer).await {
                        Ok(read_length) => {
                            trace!("we have a return value from the reader {}", read_length);
                            Some(read_length)
                        }
                        Err(e) if e.kind() == tokio::io::ErrorKind::OutOfMemory => {
                            // TODO does a max length for a td exist? -> maybe the 16MB being the edtla limit
                            panic!("normal in buffer OOM: {e}");
                        }
                        Err(e) => panic!("in endpoint reader error: {e}"),
                    }
                } else {
                    debug!("skipping hardware request");
                    None
                };

                // collect data to return
                let requested_bytes: Vec<u8>;
                if let Some(length) = read_length {
                    // hardware read happened
                    if length < requested_length {
                        // got short packet
                        requested_bytes = buffer.drain(..length).collect();
                    } else {
                        // received at least full requested length
                        requested_bytes = buffer.drain(..requested_length).collect();
                    }
                } else {
                    // from buffer only
                    requested_bytes = buffer
                        .drain(..min(buffer.len(), requested_length))
                        .collect();
                }

                if buffer.is_empty() && pkt_reader.is_end() {
                    let _ = pkt_reader.consume_end();
                }

                debug!("the responded bytes len: {:?}", requested_bytes.len());
                debug!("remaining buffer length: {}", buffer.len());

                // ...and return it
                response_submitter.send(requested_bytes)?;
            } else {
                warn!("received a none from the request_receiver, this worker is dead");
                // kill task itself
                return anyhow::Ok(());
            }
            trace!("one loop done");
        }
    } else if TypeId::of::<EpType>() == TypeId::of::<Interrupt>() {
        trace!("creating normal in worker for interrupt");
        size = endpoint.max_packet_size();
        reader = endpoint
            .reader(size)
            .with_num_transfers(8)
            .with_read_timeout(Duration::from_millis(NUSB_TIMEOUT));
        let mut pkt_reader = reader.until_short_packet();

        let mut buffer = Vec::new();

        let mut debug_counter = 0;

        loop {
            debug_counter += 1;
            trace!(
                "normal_in_endpoint_worker loop; counter at {}",
                debug_counter
            );

            if let Some(requested_length) = request_receiver.recv().await {
                trace!("original requested length: {requested_length}");
                trace!("current buffer length: {}", buffer.len());

                // do reading until end aka short or null package
                let read_length = if requested_length > buffer.len() {
                    trace!("attempting a read");

                    let mut read_sum = 0;
                    loop {
                        trace!("reading loop");

                        // read
                        let mut hardware_used_buffer: Vec<u8> = vec![0; size];

                        let read_length = match pkt_reader.read(&mut hardware_used_buffer).await {
                            Ok(length) => length,
                            Err(err) => {
                                error!("read failed: {err:?}");
                                //processing_result = map_in_err(err);
                                //panic!("read failed: {err:?}");
                                0
                            }
                        };

                        trace!("read {read_length} characters: {hardware_used_buffer:?}");

                        if pkt_reader.is_end() {
                            trace!("reached EOF, consuming end to continue on next request");
                            let _ = pkt_reader.consume_end();
                            break;
                        }

                        trace!("buffer.append");
                        buffer.append(&mut hardware_used_buffer.drain(0..(read_length)).collect());
                        read_sum += read_length;

                        trace!("read sum is {}", read_sum);

                        // if requested_length full break and return
                        // necessary for interrupt ep with no EOF like usb 1.1
                        if read_sum >= requested_length {
                            trace!("read what we needed, rest until EOF is not yet read");
                            break;
                        }
                    }
                    Some(read_sum)
                } else {
                    debug!("skipping hardware request");
                    None
                };

                // collect data to return
                let requested_bytes: Vec<u8>;
                if let Some(length) = read_length {
                    // hardware read happened
                    if length < requested_length {
                        // got short packet
                        requested_bytes = buffer.drain(..length).collect();
                    } else {
                        // received at least full requested length
                        requested_bytes = buffer.drain(..requested_length).collect();
                    }
                } else {
                    // from buffer only
                    requested_bytes = buffer.drain(..requested_length).collect();
                }

                debug!("the responded bytes len: {:?}", requested_bytes.len());
                debug!("remaining buffer length: {}", buffer.len());

                // ...and return it
                response_submitter.send(requested_bytes)?;
            } else {
                warn!("received a none from the request_receiver, this worker is dead");
                // kill task itself
                return anyhow::Ok(());
            }
            trace!("one loop done");
        }
    } else {
        unreachable!("");
    }
}
