use tokio::{
    runtime,
    sync::{mpsc, oneshot},
};
use tracing::debug;

use crate::device::{
    bus::BusDeviceRef,
    xhci::{
        endpoint::{EndpointMessage, EndpointWorker},
        endpoint_handle::{
            ControlEndpointHandle, EndpointHandle, HotplugEndpointHandle, InEndpointHandle,
            OutEndpointHandle,
        },
        interrupter::EventSender,
        nusb::NormalEndpointHandle,
        port::PortMessage,
        real_device::{Identifier, RealDevice},
        slot_manager::{EndpointContext, EndpointType},
    },
};

#[derive(Debug)]
pub struct EndpointLauncher<RD: RealDevice, ID: Identifier> {
    request_recv: mpsc::UnboundedReceiver<LaunchRequest>,
    port_msg_sender: mpsc::UnboundedSender<PortMessage<RD, ID>>,
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
    pub responder: oneshot::Sender<mpsc::UnboundedSender<EndpointMessage>>,
}

impl<RD: RealDevice, ID: Identifier> EndpointLauncher<RD, ID> {
    pub fn start(
        request_recv: mpsc::UnboundedReceiver<LaunchRequest>,
        port_msg_sender: mpsc::UnboundedSender<PortMessage<RD, ID>>,
        async_runtime: runtime::Handle,
        dma_bus: BusDeviceRef,
        event_sender: EventSender,
    ) {
        let launcher = Self {
            request_recv,
            port_msg_sender,
            async_runtime: async_runtime.clone(),
            dma_bus,
            event_sender,
        };
        async_runtime.spawn(launcher.run());
    }

    async fn run(mut self) -> ! {
        loop {
            let request = self
                .request_recv
                .recv()
                .await
                .expect("channel should never close");
            debug!(
                "endpoint launch request for slot {} endpoint {} with device at root hub port {}",
                request.slot_id, request.endpoint_id, request.root_hub_port
            );

            let (send, recv) = oneshot::channel();
            self.port_msg_sender
                .send(PortMessage::GetDevice(request.root_hub_port as usize, send));
            let device = recv
                .await
                .expect("port worker should always send a response");
            let endpoint_type = request.endpoint_context.get_endpoint_type();
            debug!("endpoint context specifies endpoint type {endpoint_type:?}");

            let hotplug_endpoint_handle = match device {
                Some(device) => {
                    let endpoint_handle: Box<dyn EndpointHandle> = match endpoint_type {
                        EndpointType::Control => {
                            let real_endpoint = device.real_device.control_endpoint_handle();
                            let endpoint_handle = ControlEndpointHandle::new(
                                request.slot_id,
                                request.endpoint_id,
                                real_endpoint,
                                self.dma_bus.clone(),
                                self.event_sender.clone(),
                            );
                            Box::new(endpoint_handle)
                        }
                        EndpointType::BulkIn => {
                            let real_endpoint = device.real_device.bulk_in_endpoint_handle(request.endpoint_id);
                            let endpoint_handle = InEndpointHandle::new(
                                request.slot_id,
                                request.endpoint_id,
                                real_endpoint,
                                self.dma_bus.clone(),
                                self.event_sender.clone()
                            );
                            Box::new(endpoint_handle)
                        },
                        EndpointType::BulkOut => {
                            let real_endpoint = device.real_device.bulk_out_endpoint_handle(request.endpoint_id);
                            let endpoint_handle = OutEndpointHandle::new(
                                request.slot_id,
                                request.endpoint_id,
                                real_endpoint,
                                self.dma_bus.clone(),
                                self.event_sender.clone()
                            );
                            Box::new(endpoint_handle)
                        },
                        EndpointType::InterruptIn => todo!(),
                        EndpointType::InterruptOut => todo!(),
                        EndpointType::Unsupported => unreachable!("the slot should early-reject configure endpoint commands with unsupported endpoint types"),
                    };
                    debug!("Created endpoint handle from real device");
                    HotplugEndpointHandle::new(
                        endpoint_handle,
                        device.cancel.clone(),
                        &self.async_runtime,
                    )
                }
                // unlikely edge case: The device was very recently detached, we are now handling an address device/configure endpoint command
                // the endpoint does not depend on the real device, so we need the address device/configure endpoint to succeed (while also not
                // creating an invalid state)
                None => {
                    debug!("Could not get real device, using dummy endpoint handle");
                    HotplugEndpointHandle::dummy()
                }
            };

            let sender = EndpointWorker::launch(
                &self.async_runtime,
                self.dma_bus.clone(),
                hotplug_endpoint_handle,
                request.endpoint_context,
            );
            request.responder.send(sender);
        }
    }
}
