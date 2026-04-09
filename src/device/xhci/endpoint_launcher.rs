use anyhow::anyhow;
use tokio::{
    runtime,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::{
    device::{
        bus::BusDeviceRef,
        pcap::EndpointPcapMeta,
        xhci::{
            endpoint::{EndpointSender, EndpointWorker},
            endpoint_handle::{
                ControlEndpointHandle, EndpointHandle, InEndpointHandle, OutEndpointHandle,
            },
            hotplug_endpoint_handle::HotplugEndpointHandleImpl,
            interrupter::EventSender,
            port::DeviceRetriever,
            real_device::{CompleteRealDevice, RealDevice},
            slot_manager::{EndpointContext, EndpointType},
        },
    },
    oneshot_anyhow::SendWithAnyhowError,
};

#[derive(Debug)]
pub struct EndpointLauncher<CRD: CompleteRealDevice> {
    request_recv: mpsc::UnboundedReceiver<LaunchRequest>,
    device_retriever: DeviceRetriever<CRD>,
    async_runtime: runtime::Handle,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
}

#[derive(Debug)]
pub struct LaunchRequest {
    pub slot_id: u8,
    pub endpoint_id: u8,
    pub root_hub_port: u8,
    pub endpoint_context: EndpointContext,
    pub responder: oneshot::Sender<EndpointSender>,
}

#[derive(Debug, Clone)]
pub struct LaunchRequester {
    msg_send: mpsc::UnboundedSender<LaunchRequest>,
}

impl LaunchRequester {
    pub async fn request_launch(
        &self,
        slot_id: u8,
        endpoint_id: u8,
        root_hub_port: u8,
        endpoint_context: EndpointContext,
    ) -> anyhow::Result<EndpointSender> {
        let (send, recv) = oneshot::channel();
        let launch_request = LaunchRequest {
            slot_id,
            endpoint_id,
            root_hub_port,
            endpoint_context,
            responder: send,
        };
        self.msg_send.send(launch_request)?;
        let ep_sender = recv.await?;

        Ok(ep_sender)
    }
}

impl<CRD: CompleteRealDevice> EndpointLauncher<CRD> {
    pub fn start(
        device_retriever: DeviceRetriever<CRD>,
        async_runtime: runtime::Handle,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) -> LaunchRequester {
        let (send, recv) = mpsc::unbounded_channel();
        let launcher = Self {
            request_recv: recv,
            device_retriever,
            async_runtime: async_runtime.clone(),
            dma_bus,
            event_sender,
        };
        async_runtime.spawn(launcher.run());

        LaunchRequester { msg_send: send }
    }

    async fn run(mut self) {
        match self.run_loop().await {
            Ok(_) => unreachable!(),
            Err(err) => {
                info!("EndpointLauncher stopped {err}");
            }
        }
    }

    // function only returns on error, but cannot use ! in Result
    async fn run_loop(&mut self) -> anyhow::Result<()> {
        loop {
            let request = self.next_msg().await?;
            debug!(
                "endpoint launch request for slot {} endpoint {} with device at root hub port {}",
                request.slot_id, request.endpoint_id, request.root_hub_port
            );

            let device = self
                .device_retriever
                .get_device(request.root_hub_port)
                .await?;
            let endpoint_type = request.endpoint_context.get_endpoint_type();
            debug!("endpoint context specifies endpoint type {endpoint_type:?}");

            let endpoint_sender = match device {
                Some(device) => match endpoint_type {
                    EndpointType::Control => {
                        let pcap_meta = EndpointPcapMeta::control(
                            Self::pcap_usb_type(device.as_ref()),
                            request.slot_id,
                            request.endpoint_id,
                        );
                        let real_endpoint = device.realdevice_ref().control_endpoint_handle();
                        self.launch_helper(
                            ControlEndpointHandle::new,
                            request.slot_id,
                            request.endpoint_id,
                            pcap_meta,
                            real_endpoint,
                            request.endpoint_context,
                            device.detach_token(),
                        )
                    }
                    EndpointType::BulkIn => {
                        let pcap_meta = EndpointPcapMeta::bulk_in(
                            Self::pcap_usb_type(device.as_ref()),
                            request.slot_id,
                            request.endpoint_id,
                        );
                        let real_endpoint = device
                            .realdevice_ref()
                            .bulk_in_endpoint_handle(request.endpoint_id);
                        self.launch_helper(
                            InEndpointHandle::new,
                            request.slot_id,
                            request.endpoint_id,
                            pcap_meta,
                            real_endpoint,
                            request.endpoint_context,
                            device.detach_token(),
                        )
                    }
                    EndpointType::BulkOut => {
                        let pcap_meta = EndpointPcapMeta::bulk_out(
                            Self::pcap_usb_type(device.as_ref()),
                            request.slot_id,
                            request.endpoint_id,
                        );
                        let real_endpoint = device
                            .realdevice_ref()
                            .bulk_out_endpoint_handle(request.endpoint_id);
                        self.launch_helper(
                            OutEndpointHandle::new,
                            request.slot_id,
                            request.endpoint_id,
                            pcap_meta,
                            real_endpoint,
                            request.endpoint_context,
                            device.detach_token(),
                        )
                    }
                    EndpointType::InterruptIn => {
                        let pcap_meta = EndpointPcapMeta::interrupt_in(
                            Self::pcap_usb_type(device.as_ref()),
                            request.slot_id,
                            request.endpoint_id,
                        );
                        let real_endpoint = device
                            .realdevice_ref()
                            .interrupt_in_endpoint_handle(request.endpoint_id);
                        self.launch_helper(
                            InEndpointHandle::new,
                            request.slot_id,
                            request.endpoint_id,
                            pcap_meta,
                            real_endpoint,
                            request.endpoint_context,
                            device.detach_token(),
                        )
                    }
                    EndpointType::InterruptOut => {
                        let pcap_meta = EndpointPcapMeta::interrupt_out(
                            Self::pcap_usb_type(device.as_ref()),
                            request.slot_id,
                            request.endpoint_id,
                        );
                        let real_endpoint = device
                            .realdevice_ref()
                            .interrupt_out_endpoint_handle(request.endpoint_id);
                        self.launch_helper(
                            OutEndpointHandle::new,
                            request.slot_id,
                            request.endpoint_id,
                            pcap_meta,
                            real_endpoint,
                            request.endpoint_context,
                            device.detach_token(),
                        )
                    }
                    EndpointType::Unsupported => unreachable!(
                        "the slot should early-reject configure endpoint commands with unsupported endpoint types"
                    ),
                },
                // unlikely edge case: The device was very recently detached, we are now handling an address device/configure endpoint command
                // the endpoint does not depend on the real device, so we need the address device/configure endpoint to succeed (while also not
                // creating an invalid state)
                None => {
                    debug!("Could not get real device, using dummy endpoint handle");
                    let hotplug_endpoint_handle = HotplugEndpointHandleImpl::dummy(
                        request.slot_id, request.endpoint_id, self.event_sender.clone()
                    );

                    EndpointWorker::launch(
                        &self.async_runtime,
                        self.dma_bus.clone(),
                        hotplug_endpoint_handle,
                        request.endpoint_context,
                    )
                }
            };

            request.responder.send_anyhow(endpoint_sender)?;
        }
    }

    async fn next_msg(&mut self) -> anyhow::Result<LaunchRequest> {
        self.request_recv
            .recv()
            .await
            .ok_or_else(|| anyhow!("channel should never close"))
    }

    fn pcap_usb_type(device: &CRD) -> u16 {
        match device.realdevice_ref().speed() {
            Some(speed) if speed.is_usb2_speed() => 2,
            Some(_) => 3,
            None => 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn launch_helper<RealEndpoint, Endpoint, EndpointConstructor>(
        &self,
        constructor: EndpointConstructor,
        slot_id: u8,
        endpoint_id: u8,
        pcap_meta: EndpointPcapMeta,
        real_endpoint: RealEndpoint,
        endpoint_context: EndpointContext,
        detach_token: CancellationToken,
    ) -> EndpointSender
    where
        Endpoint: EndpointHandle,
        EndpointConstructor:
            FnOnce(u8, u8, EndpointPcapMeta, RealEndpoint, BusDeviceRef, EventSender) -> Endpoint,
    {
        let endpoint_handle = constructor(
            slot_id,
            endpoint_id,
            pcap_meta,
            real_endpoint,
            self.dma_bus.clone(),
            self.event_sender.clone(),
        );
        let hotplug_endpoint_handle = HotplugEndpointHandleImpl::new(
            slot_id,
            endpoint_id,
            endpoint_handle,
            self.event_sender.clone(),
            detach_token,
            &self.async_runtime,
        );

        EndpointWorker::launch(
            &self.async_runtime,
            self.dma_bus.clone(),
            hotplug_endpoint_handle,
            endpoint_context,
        )
    }
}
