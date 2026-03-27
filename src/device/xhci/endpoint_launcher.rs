use anyhow::anyhow;
use tokio::{
    runtime,
    sync::{mpsc, oneshot},
};
use tracing::{debug, info};

use crate::{
    device::{
        bus::BusDeviceRef,
        xhci::{
            endpoint::{EndpointSender, EndpointWorker},
            endpoint_handle::{ControlEndpointHandle, InEndpointHandle, OutEndpointHandle},
            hotplug_endpoint_handle::HotplugEndpointHandleImpl,
            interrupter::EventSender,
            port::DeviceRetriever,
            real_device::{Identifier, RealDevice},
            slot_manager::{EndpointContext, EndpointType},
        },
    },
    oneshot_anyhow::SendWithAnyhowError,
};

#[derive(Debug)]
pub struct EndpointLauncher<RD: RealDevice, ID: Identifier> {
    request_recv: mpsc::UnboundedReceiver<LaunchRequest>,
    device_retriever: DeviceRetriever<RD, ID>,
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

impl<RD: RealDevice, ID: Identifier> EndpointLauncher<RD, ID> {
    pub fn start(
        request_recv: mpsc::UnboundedReceiver<LaunchRequest>,
        device_retriever: DeviceRetriever<RD, ID>,
        async_runtime: runtime::Handle,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) {
        let launcher = Self {
            request_recv,
            device_retriever,
            async_runtime: async_runtime.clone(),
            dma_bus,
            event_sender,
        };
        async_runtime.spawn(launcher.run());
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
                        let real_endpoint = device.real_device.control_endpoint_handle();
                        let endpoint_handle = ControlEndpointHandle::new(
                            request.slot_id,
                            request.endpoint_id,
                            real_endpoint,
                            self.dma_bus.clone(),
                            self.event_sender.clone(),
                        );
                        let hotplug_endpoint_handle = HotplugEndpointHandleImpl::new(
                            request.slot_id,
                            request.endpoint_id,
                            endpoint_handle,
                            self.event_sender.clone(),
                            device.cancel.clone(),
                            &self.async_runtime,
                        );

                        EndpointWorker::launch(
                            &self.async_runtime,
                            self.dma_bus.clone(),
                            hotplug_endpoint_handle,
                            request.endpoint_context,
                        )
                    }
                    EndpointType::BulkIn => {
                        let real_endpoint = device
                            .real_device
                            .bulk_in_endpoint_handle(request.endpoint_id);
                        let endpoint_handle = InEndpointHandle::new(
                            request.slot_id,
                            request.endpoint_id,
                            real_endpoint,
                            self.dma_bus.clone(),
                            self.event_sender.clone(),
                        );
                        let hotplug_endpoint_handle = HotplugEndpointHandleImpl::new(
                            request.slot_id,
                            request.endpoint_id,
                            endpoint_handle,
                            self.event_sender.clone(),
                            device.cancel.clone(),
                            &self.async_runtime,
                        );

                        EndpointWorker::launch(
                            &self.async_runtime,
                            self.dma_bus.clone(),
                            hotplug_endpoint_handle,
                            request.endpoint_context,
                        )
                    }
                    EndpointType::BulkOut => {
                        let real_endpoint = device
                            .real_device
                            .bulk_out_endpoint_handle(request.endpoint_id);
                        let endpoint_handle = OutEndpointHandle::new(
                            request.slot_id,
                            request.endpoint_id,
                            real_endpoint,
                            self.dma_bus.clone(),
                            self.event_sender.clone(),
                        );
                        let hotplug_endpoint_handle = HotplugEndpointHandleImpl::new(
                            request.slot_id,
                            request.endpoint_id,
                            endpoint_handle,
                            self.event_sender.clone(),
                            device.cancel.clone(),
                            &self.async_runtime,
                        );

                        EndpointWorker::launch(
                            &self.async_runtime,
                            self.dma_bus.clone(),
                            hotplug_endpoint_handle,
                            request.endpoint_context,
                        )
                    }
                    EndpointType::InterruptIn => {
                        let real_endpoint = device
                            .real_device
                            .interrupt_in_endpoint_handle(request.endpoint_id);
                        let endpoint_handle = InEndpointHandle::new(
                            request.slot_id,
                            request.endpoint_id,
                            real_endpoint,
                            self.dma_bus.clone(),
                            self.event_sender.clone(),
                        );
                        let hotplug_endpoint_handle = HotplugEndpointHandleImpl::new(
                            request.slot_id,
                            request.endpoint_id,
                            endpoint_handle,
                            self.event_sender.clone(),
                            device.cancel.clone(),
                            &self.async_runtime,
                        );

                        EndpointWorker::launch(
                            &self.async_runtime,
                            self.dma_bus.clone(),
                            hotplug_endpoint_handle,
                            request.endpoint_context,
                        )
                    }
                    EndpointType::InterruptOut => {
                        let real_endpoint = device
                            .real_device
                            .interrupt_out_endpoint_handle(request.endpoint_id);
                        let endpoint_handle = OutEndpointHandle::new(
                            request.slot_id,
                            request.endpoint_id,
                            real_endpoint,
                            self.dma_bus.clone(),
                            self.event_sender.clone(),
                        );
                        let hotplug_endpoint_handle = HotplugEndpointHandleImpl::new(
                            request.slot_id,
                            request.endpoint_id,
                            endpoint_handle,
                            self.event_sender.clone(),
                            device.cancel.clone(),
                            &self.async_runtime,
                        );

                        EndpointWorker::launch(
                            &self.async_runtime,
                            self.dma_bus.clone(),
                            hotplug_endpoint_handle,
                            request.endpoint_context,
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
}
