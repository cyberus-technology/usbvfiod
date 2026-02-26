use tokio::{
    runtime,
    sync::{mpsc, oneshot},
};

use crate::device::{
    bus::BusDeviceRef,
    xhci::{
        endpoint::{EndpointMessage, EndpointWorker},
        endpoint_handle::{ControlEndpointHandle, EndpointHandle, HotplugEndpointHandle},
        interrupter::EventSender,
        port::PortMessage,
        real_device::{Identifier, RealDevice},
        slot_manager::{EndpointContext, EndpointType},
    },
};

#[derive(Debug)]
pub struct EndpointLauncher<RD: RealDevice, ID: Identifier> {
    request_recv: mpsc::Receiver<LaunchRequest>,
    port_msg_sender: mpsc::Sender<PortMessage<RD, ID>>,
    async_runtime: runtime::Handle,
    dma_bus: BusDeviceRef,
    event_sender: EventSender,
}

#[derive(Debug)]
pub struct LaunchRequest {
    pub slot_id: u8,
    pub endpoint_id: u8,
    pub endpoint_context: EndpointContext,
    pub responder: oneshot::Sender<mpsc::Sender<EndpointMessage>>,
}

impl<RD: RealDevice, ID: Identifier> EndpointLauncher<RD, ID> {
    pub fn start(
        request_recv: mpsc::Receiver<LaunchRequest>,
        port_msg_sender: mpsc::Sender<PortMessage<RD, ID>>,
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

            let root_hub_port = request.endpoint_context.get_root_hub_port();
            let (send, recv) = oneshot::channel();
            self.port_msg_sender
                .send(PortMessage::GetDevice(root_hub_port as usize, send));
            let device = recv
                .await
                .expect("port worker should always send a response");
            let endpoint_type = request.endpoint_context.get_endpoint_type();

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
                        EndpointType::BulkIn => todo!(),
                        EndpointType::BulkOut => todo!(),
                        EndpointType::InterruptIn => todo!(),
                        EndpointType::InterruptOut => todo!(),
                        EndpointType::Unsupported => unreachable!("the slot should early-reject configure endpoint commands with unsupported endpoint types"),
                    };
                    HotplugEndpointHandle::new(
                        endpoint_handle,
                        device.cancel.clone(),
                        &self.async_runtime,
                    )
                }
                // unlikely edge case: The device was very recently detached, we are now handling an address device/configure endpoint command
                // the endpoint does not depend on the real device, so we need the address device/configure endpoint to succeed (while also not
                // creating an invalid state)
                None => HotplugEndpointHandle::dummy(),
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
