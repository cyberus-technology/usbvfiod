use std::{array, mem, sync::Arc};

use anyhow::anyhow;
use tokio::{
    runtime,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use usbvfiod::hotplug_protocol::response::Response;

use crate::{
    device::{
        pci::constants::xhci::{offset, operational::portsc, MAX_PORTS, NUM_USB3_PORTS},
        xhci::{
            interrupter::EventSender,
            real_device::{CompleteRealDevice, RealDevice, Speed},
            registers::{PortpmscRegister, PortscRegister},
            trb::EventTrb,
        },
    },
    one_indexed_array::OneIndexed,
    oneshot_anyhow::SendWithAnyhowError,
};

#[derive(Debug)]
pub struct PortArray<CRD: CompleteRealDevice> {
    portsc: Arc<OneIndexed<PortscRegister, { MAX_PORTS as usize }>>,
    portpmsc: Arc<OneIndexed<PortpmscRegister, { MAX_PORTS as usize }>>,
    pub msg_sender: mpsc::UnboundedSender<PortMessage<CRD>>,
}

impl<CRD: CompleteRealDevice> PortArray<CRD> {
    pub fn new(event_sender: EventSender, async_runtime: runtime::Handle) -> Self {
        let portsc: Arc<OneIndexed<PortscRegister, { MAX_PORTS as usize }>> = Arc::new(
            array::from_fn(|index| {
                // SAFETY: port_id is capped at 255 according to spec
                let port_id = index as u8 + 1;
                PortscRegister::new(event_sender.clone(), Self::port_version(port_id), port_id)
            })
            .into(),
        );

        let portpmsc: Arc<OneIndexed<PortpmscRegister, { MAX_PORTS as usize }>> =
            Arc::new(array::from_fn(|_| PortpmscRegister::default()).into());

        let (msg_sender, msg_recv) = mpsc::unbounded_channel();

        let worker = PortWorker {
            devices: [const { None }; MAX_PORTS as usize].into(),
            portsc: portsc.clone(),
            event_sender,
            msg_sender: msg_sender.clone(),
            msg_recv,
            async_runtime: async_runtime.clone(),
        };

        async_runtime.spawn(worker.run());

        Self {
            portsc,
            portpmsc,
            msg_sender,
        }
    }

    pub fn write_portsc(&self, port_id: usize, value: u64) -> anyhow::Result<()> {
        self.portsc[port_id].write(value)
    }

    pub fn read_portsc(&self, port_id: usize) -> u64 {
        self.portsc[port_id].read()
    }

    pub fn write_portpmsc(&self, port_id: usize, value: u32) {
        self.portpmsc[port_id].write(value);
    }

    pub fn read_portpmsc(&self, port_id: usize) -> u32 {
        self.portpmsc[port_id].read()
    }

    pub fn create_hotplug_control(&self) -> HotplugControl<CRD> {
        HotplugControl {
            msg_send: self.msg_sender.clone(),
        }
    }

    pub fn create_device_retriever(&self) -> DeviceRetriever<CRD> {
        DeviceRetriever {
            msg_send: self.msg_sender.clone(),
        }
    }

    fn port_version(port_id: u8) -> UsbVersion {
        match port_id as u64 {
            1..=NUM_USB3_PORTS => UsbVersion::USB3,
            id if id > NUM_USB3_PORTS && id <= MAX_PORTS => UsbVersion::USB2,
            id => panic!("asked for port version of non-existent port id {id}"),
        }
    }
}

#[derive(Debug)]
struct PortWorker<CRD: CompleteRealDevice> {
    devices: OneIndexed<Option<AttachedDevice<CRD>>, { MAX_PORTS as usize }>,
    portsc: Arc<OneIndexed<PortscRegister, { MAX_PORTS as usize }>>,
    event_sender: EventSender,
    // the worker does not use the sender itself but needs to pass clones of the sender to detach listeners
    msg_sender: mpsc::UnboundedSender<PortMessage<CRD>>,
    msg_recv: mpsc::UnboundedReceiver<PortMessage<CRD>>,
    async_runtime: runtime::Handle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInstanceId(CancellationToken);

#[derive(Debug)]
struct AttachedDevice<CRD: CompleteRealDevice> {
    device: Arc<CRD>,
    instance_id: DeviceInstanceId,
}

#[derive(Debug)]
pub enum PortMessage<CRD: CompleteRealDevice> {
    Attach(CRD, oneshot::Sender<Response>),
    Detach(CRD::ID, Option<DeviceInstanceId>, oneshot::Sender<Response>),
    ListAttached(oneshot::Sender<Vec<CRD::ID>>),
    // port id
    GetDevice(usize, oneshot::Sender<Option<Arc<CRD>>>),
}

impl<CRD: CompleteRealDevice> PortWorker<CRD> {
    async fn run(mut self) {
        match self.run_loop().await {
            Ok(_) => unreachable!(),
            Err(err) => info!("PortWorker stopped {err}"),
        }
    }

    // this function should only return with an error, but we cannot use ! in Result
    async fn run_loop(&mut self) -> anyhow::Result<()> {
        loop {
            match self.next_msg().await? {
                PortMessage::Attach(device, responder) => {
                    responder.send_anyhow(self.attach(device)?)?;
                }
                PortMessage::Detach(identifier, instance_id, responder) => {
                    responder.send_anyhow(self.detach(identifier, instance_id)?)?;
                }
                PortMessage::ListAttached(responder) => {
                    responder.send_anyhow(self.attached_devices())?;
                }
                PortMessage::GetDevice(port_id, responder) => {
                    let device = self
                        .devices
                        .get(port_id)
                        .and_then(|opt| opt.as_ref().map(|attached| attached.device.clone()));
                    responder.send_anyhow(device)?;
                }
            };
        }
    }

    async fn next_msg(&mut self) -> anyhow::Result<PortMessage<CRD>> {
        self.msg_recv
            .recv()
            .await
            .ok_or_else(|| anyhow!("port channel closed"))
    }

    fn attach(&mut self, device: CRD) -> anyhow::Result<Response> {
        if self.attached_devices().contains(&device.identifier()) {
            info!(
                "A device with the same identifier is already attached and will be detached first"
            );
            self.detach(device.identifier(), None)?;
        }

        let speed = match device.realdevice_ref().speed() {
            Some(speed) => speed,
            None => return Ok(Response::CouldNotDetermineSpeed),
        };
        let version = UsbVersion::from_speed(speed);

        let available_port_id = match (1..=MAX_PORTS as usize)
                .find(|&i| {
                    self.devices[i].is_none()
                        && self.portsc[i].usb_version() == version
                }) // filter USB2/3
                {
                    Some(port) => port,
                    None => return Ok(Response::NoFreePort),
        };

        let identifier = device.identifier();
        let cancel = device.detach_token();
        let instance_id = DeviceInstanceId(cancel.clone());
        self.async_runtime.spawn(detach_listener(
            cancel,
            identifier,
            instance_id.clone(),
            self.msg_sender.clone(),
        ));

        self.devices[available_port_id] = Some(AttachedDevice {
            device: Arc::new(device),
            instance_id,
        });

        let new_portsc = match version {
            UsbVersion::USB3 => {
                portsc::CCS
                    | portsc::PED
                    | portsc::PP
                    | portsc::CSC
                    | portsc::PEC
                    | portsc::PRC
                    | (speed as u64) << 10
            }
            UsbVersion::USB2 => {
                portsc::CCS
                    | portsc::value::PLS_POLLING
                    | portsc::PP
                    | (speed as u64) << 10
                    | portsc::CSC
            }
        };
        self.portsc[available_port_id].set(new_portsc);

        info!(
            "Attached {speed} device {identifier:?} to port {available_port_id} ({version:?} port)"
        );

        let event = EventTrb::new_port_status_change_event_trb(available_port_id as u8);
        self.event_sender.send(event)?;

        Ok(Response::SuccessfulOperation)
    }

    fn attached_devices(&self) -> Vec<CRD::ID> {
        self.devices
            .iter()
            .filter_map(|dev| dev.as_ref())
            .map(|attached| attached.device.identifier())
            .collect()
    }

    fn detach(
        &mut self,
        id: CRD::ID,
        requested_instance_id: Option<DeviceInstanceId>,
    ) -> anyhow::Result<Response> {
        // find out on which port the device is connected
        let port_id = match self
            .devices
            .enumerate()
            .filter_map(|(i, port)| {
                port.as_ref()
                    .map(|attached| (i, attached.device.identifier()))
            })
            .filter(|(_, dev_id)| *dev_id == id)
            .map(|(i, _)| i)
            .next()
        {
            Some(i) => {
                debug!("Device to detach is connected to port {i}");
                i
            }
            None => {
                // This message is expected once per soft detach:
                // - this handler runs
                // - cancels the detach token
                // - detach_listener_tasks notices the cancellation and calls this handler again
                // - second handler of course now cannot find this devices
                //
                // However, this message will also be printed when detach command for unknown identifier
                // is received.
                debug!("Could not find the device to detach");
                return Ok(Response::NoSuchDevice);
            }
        };

        if requested_instance_id
            .as_ref()
            .is_some_and(|requested_instance_id| {
                requested_instance_id != &self.devices[port_id].as_ref().unwrap().instance_id
            })
        {
            debug!("Device instance ID does not match; dropping detach request");
            return Ok(Response::NoSuchDevice);
        }

        // inform everybody else (endpoint handles) about the detach, so that they can drop
        // their reference of the device, too. This operation also removes the device from
        // the devices array.
        //
        // Safety: just determined that this port_id refers to the device we want to detach
        mem::take(&mut self.devices[port_id])
            .unwrap()
            .device
            .detach_token()
            .cancel();

        // update portsc register
        self.portsc[port_id].set(portsc::PP | portsc::CSC);

        // send port status change event
        let event = EventTrb::new_port_status_change_event_trb(port_id as u8);
        self.event_sender.send(event)?;

        info!("Detached device {id:?} from port {port_id}");

        Ok(Response::SuccessfulOperation)
    }
}

async fn detach_listener<CRD: CompleteRealDevice>(
    cancel: CancellationToken,
    identifier: CRD::ID,
    instance_id: DeviceInstanceId,
    msg_sender: mpsc::UnboundedSender<PortMessage<CRD>>,
) {
    let (send, recv) = oneshot::channel();
    cancel.cancelled().await;
    let _ = msg_sender.send(PortMessage::Detach(identifier, Some(instance_id), send));
    let _ = recv.await;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbVersion {
    USB2,
    USB3,
}

impl UsbVersion {
    const fn from_speed(speed: Speed) -> Self {
        if speed.is_usb2_speed() {
            Self::USB2
        } else {
            Self::USB3
        }
    }
}

// Helper function to get port id from MMIO address
const fn get_port_id_from_addr(
    addr: u64,
    base_addr: u64,
    port_count: u64,
    register_offset: u64,
) -> Option<usize> {
    if addr >= base_addr && addr < base_addr + (port_count * offset::PORT_STRIDE) {
        // Check if this is the correct register within the port's PORT_STRIDE byte range
        if (addr - base_addr) % offset::PORT_STRIDE == register_offset {
            Some(((addr - base_addr) / offset::PORT_STRIDE) as usize + 1)
        } else {
            None
        }
    } else {
        None
    }
}

pub const fn get_portsc_id(addr: u64) -> Option<usize> {
    get_port_id_from_addr(addr, offset::PORTSC, MAX_PORTS, 0)
}

pub const fn get_portpmsc_id(addr: u64) -> Option<usize> {
    get_port_id_from_addr(addr, offset::PORTSC, MAX_PORTS, 0x4)
}

pub const fn get_portli_id(addr: u64) -> Option<usize> {
    get_port_id_from_addr(addr, offset::PORTSC, MAX_PORTS, 0x8)
}

#[derive(Debug, Clone)]
pub struct HotplugControl<CRD: CompleteRealDevice> {
    msg_send: mpsc::UnboundedSender<PortMessage<CRD>>,
}

impl<CRD: CompleteRealDevice> HotplugControl<CRD> {
    pub async fn attach(&self, device: CRD) -> Response {
        let (responder, response_recv) = oneshot::channel();
        let msg = PortMessage::Attach(device, responder);
        self.msg_send.send(msg).expect("channel should never close");
        response_recv
            .await
            .expect("oneshot channel should always provide a message")
    }

    pub async fn detach(&self, identifier: CRD::ID) -> Response {
        let (responder, response_recv) = oneshot::channel();
        let msg = PortMessage::Detach(identifier, None, responder);
        self.msg_send.send(msg).expect("channel should never close");
        response_recv
            .await
            .expect("oneshot channel should always provide a message")
    }

    pub async fn list_devices(&self) -> Vec<CRD::ID> {
        let (responder, response_recv) = oneshot::channel();
        let msg = PortMessage::ListAttached(responder);
        self.msg_send.send(msg).expect("channel should never close");
        response_recv
            .await
            .expect("oneshot channel should always provide a message")
    }
}

#[derive(Debug, Clone)]
pub struct DeviceRetriever<CRD: CompleteRealDevice> {
    msg_send: mpsc::UnboundedSender<PortMessage<CRD>>,
}

impl<CRD: CompleteRealDevice> DeviceRetriever<CRD> {
    pub async fn get_device(&self, port_id: u8) -> anyhow::Result<Option<Arc<CRD>>> {
        let (send, recv) = oneshot::channel();
        self.msg_send
            .send(PortMessage::GetDevice(port_id as usize, send))?;
        let device = recv.await?;

        Ok(device)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::{runtime::Handle, time::timeout};

    use crate::device::xhci::{
        interrupter::tests::testutils::MockInterrupter,
        real_device::{tests::testutils::MockRealDevice, CompleteRealDeviceImpl},
    };

    use super::*;

    const ASYNC_TIMEOUT_SECS: u64 = 30;
    const PORT_ID: u8 = 1;
    const USB_BUS_NR: u8 = 1;
    const USB_DEV_NR: u8 = 1;
    const IDENTIFIER: (u8, u8) = (USB_BUS_NR, USB_DEV_NR);

    #[tokio::test]
    async fn port_array_hotplug_control_can_attach_list_and_detach() {
        let async_runtime = Handle::current();
        let (event_sender, mut interrupter) = MockInterrupter::new();

        let mock_real_device = CompleteRealDeviceImpl::new(IDENTIFIER, MockRealDevice::default());
        let port_array: PortArray<CompleteRealDeviceImpl<MockRealDevice, (u8, u8)>> =
            PortArray::new(event_sender, async_runtime);

        // attach a device
        let hotplug_control = port_array.create_hotplug_control();
        let response = timeout(
            Duration::from_secs(ASYNC_TIMEOUT_SECS),
            hotplug_control.attach(mock_real_device),
        )
        .await
        .expect("local timeout on await");

        assert_eq!(response, Response::SuccessfulOperation);
        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_port_status_change_event_trb(PORT_ID))
        );

        // list attached devices
        let response = timeout(
            Duration::from_secs(ASYNC_TIMEOUT_SECS),
            hotplug_control.list_devices(),
        )
        .await
        .expect("local timeout on await");

        assert_eq!(response, vec![IDENTIFIER]);
        assert!(interrupter.is_empty());

        // detach the device
        let response = timeout(
            Duration::from_secs(ASYNC_TIMEOUT_SECS),
            hotplug_control.detach(IDENTIFIER),
        )
        .await
        .expect("local timeout on await");

        // expect a successful response for the command and an event
        assert_eq!(response, Response::SuccessfulOperation);
        assert_eq!(
            interrupter.await_event().await,
            Some(EventTrb::new_port_status_change_event_trb(PORT_ID))
        );

        // list attached devices
        let response = timeout(
            Duration::from_secs(ASYNC_TIMEOUT_SECS),
            hotplug_control.list_devices(),
        )
        .await
        .expect("local timeout on await");

        assert_eq!(response, vec![]);
        assert!(interrupter.is_empty());
    }
}
