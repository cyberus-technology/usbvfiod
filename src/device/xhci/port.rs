use std::{array, mem, sync::Arc};

use anyhow::{anyhow, Context};
use tokio::{
    runtime,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace, warn};
use usbvfiod::hotplug_protocol::response::Response;

use crate::{
    device::{
        pci::constants::xhci::{offset, operational::portsc, MAX_PORTS, NUM_USB3_PORTS},
        xhci::{
            interrupter::EventSender,
            real_device::{CompleteRealDevice, RealDevice, Speed},
            registers::PortscRegister,
            trb::EventTrb,
        },
    },
    one_indexed_array::OneIndexed,
    oneshot_anyhow::SendWithAnyhowError,
};

#[derive(Debug)]
pub struct PortArray<CRD: CompleteRealDevice> {
    portsc: Arc<OneIndexed<PortscRegister, { MAX_PORTS as usize }>>,
    pub msg_sender: mpsc::UnboundedSender<PortMessage<CRD>>,
}

impl<CRD: CompleteRealDevice> PortArray<CRD> {
    pub fn new(event_sender: EventSender, async_runtime: runtime::Handle) -> Self {
        let portsc: Arc<OneIndexed<PortscRegister, { MAX_PORTS as usize }>> =
            Arc::new(array::from_fn(|_| PortscRegister::default()).into());

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

        Self { portsc, msg_sender }
    }

    pub fn write_portsc(&self, port_id: usize, value: u64) {
        self.portsc[port_id].write(value);

        if value & portsc::PR != 0 {
            match PortWorker::<CRD>::port_version(port_id as u64) {
                UsbVersion::USB2 => {
                    trace!("driver attempted to write portsc::PR on USB 2");
                    let portsc_update_mask = portsc::PRC | portsc::PED | portsc::PLS;
                    self.portsc[port_id].update_with_mask(
                        portsc::value::PLS_U0 | portsc::PED | portsc::PRC,
                        portsc_update_mask,
                    );

                    let event = EventTrb::new_port_status_change_event_trb(port_id as u8);
                    let _ = self
                        .msg_sender
                        .send(PortMessage::Event(event))
                        .context("event channel closed");
                }
                UsbVersion::USB3 => {
                    self.portsc[port_id].update_with_mask(portsc::PRC, portsc::PRC);
                }
            }
        }
    }

    pub fn read_portsc(&self, port_id: usize) -> u64 {
        self.portsc[port_id].read()
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
}

#[derive(Debug)]
struct PortWorker<CRD: CompleteRealDevice> {
    devices: OneIndexed<Option<Arc<CRD>>, { MAX_PORTS as usize }>,
    portsc: Arc<OneIndexed<PortscRegister, { MAX_PORTS as usize }>>,
    event_sender: EventSender,
    // the worker does not use the sender itself but needs to pass clones of the sender to detach listeners
    msg_sender: mpsc::UnboundedSender<PortMessage<CRD>>,
    msg_recv: mpsc::UnboundedReceiver<PortMessage<CRD>>,
    async_runtime: runtime::Handle,
}

#[derive(Debug)]
pub enum PortMessage<CRD: CompleteRealDevice> {
    Attach(CRD, oneshot::Sender<Response>),
    Detach(CRD::ID, oneshot::Sender<Response>),
    ListAttached(oneshot::Sender<Vec<CRD::ID>>),
    // port id
    GetDevice(usize, oneshot::Sender<Option<Arc<CRD>>>),
    Event(EventTrb),
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
                PortMessage::Detach(identifier, responder) => {
                    responder.send_anyhow(self.detach(identifier)?)?;
                }
                PortMessage::ListAttached(responder) => {
                    responder.send_anyhow(self.attached_devices())?;
                }
                PortMessage::GetDevice(port_id, responder) => {
                    let device = self
                        .devices
                        .get(port_id)
                        .and_then(|opt| opt.as_ref().map(|dev| dev.clone()));
                    responder.send_anyhow(device)?;
                }
                PortMessage::Event(event) => {
                    self.event_sender.send(event)?;
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
            warn!("Failed to attach device: A device with the same identifier is already attached");
            return Ok(Response::AlreadyAttached);
        }

        let speed = match device.realdevice_ref().speed() {
            Some(speed) => speed,
            None => return Ok(Response::CouldNotDetermineSpeed),
        };
        let version = UsbVersion::from_speed(speed);

        let available_port_id = match (1..=MAX_PORTS as usize)
                .find(|&i| {
                    self.devices[i].is_none()
                        && Self::port_version(i as u64) == version
                }) // filter USB2/3
                {
                    Some(port) => port,
                    None => return Ok(Response::NoFreePort),
                };

        let identifier = device.identifier();
        self.async_runtime.spawn(detach_listener(
            device.detach_token(),
            identifier,
            self.msg_sender.clone(),
        ));

        self.devices[available_port_id] = Some(Arc::new(device));
        self.portsc[available_port_id].set(
            portsc::CCS
                | portsc::PED
                | portsc::PP
                | portsc::CSC
                | portsc::PEC
                | portsc::PRC
                | (speed as u64) << 10,
        );

        match version {
            UsbVersion::USB3 => self.portsc[available_port_id].set(
                portsc::CCS
                    | portsc::PED
                    | portsc::PP
                    | portsc::CSC
                    | portsc::PEC
                    | portsc::PRC
                    | (speed as u64) << 10,
            ),
            UsbVersion::USB2 => self.portsc[available_port_id].set(
                portsc::CCS
                    | portsc::value::PLS_POLLING
                    | portsc::PP
                    | (speed as u64) << 10
                    | portsc::CSC,
            ),
        }

        info!(
            "Attached {speed} device {identifier:?} to port {available_port_id} ({version:?} port)"
        );

        let event = EventTrb::new_port_status_change_event_trb(available_port_id as u8);
        self.event_sender.send(event)?;

        Ok(Response::SuccessfulOperation)
    }

    fn port_version(port_id: u64) -> UsbVersion {
        match port_id {
            1..=NUM_USB3_PORTS => UsbVersion::USB3,
            id if id > NUM_USB3_PORTS && id <= MAX_PORTS => UsbVersion::USB2,
            id => panic!("asked for port version of non-existent port id {id}"),
        }
    }

    fn attached_devices(&self) -> Vec<CRD::ID> {
        self.devices
            .iter()
            .filter_map(|dev| dev.as_ref())
            .map(|dev| dev.identifier())
            .collect()
    }

    fn detach(&mut self, id: CRD::ID) -> anyhow::Result<Response> {
        // find out on which port the device is connected
        let port_id = match self
            .devices
            .enumerate()
            .filter_map(|(i, port)| port.as_ref().map(|d| (i, d.identifier())))
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

        // inform everybody else (endpoint handles) about the detach, so that they can drop
        // their reference of the device, too. This operation also removes the device from
        // the devices array.
        //
        // Safety: just determined that this port_id refers to the device we want to detach
        mem::take(&mut self.devices[port_id])
            .unwrap()
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
    msg_sender: mpsc::UnboundedSender<PortMessage<CRD>>,
) {
    let (send, recv) = oneshot::channel();
    cancel.cancelled().await;
    let _ = msg_sender.send(PortMessage::Detach(identifier, send));
    let _ = recv.await;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsbVersion {
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

// Helper function to get port index from MMIO address
const fn get_port_index_from_addr(
    addr: u64,
    base_addr: u64,
    port_count: u64,
    register_offset: u64,
) -> Option<usize> {
    if addr >= base_addr && addr < base_addr + (port_count * offset::PORT_STRIDE) {
        // Check if this is the correct register within the port's PORT_STRIDE byte range
        if (addr - base_addr) % offset::PORT_STRIDE == register_offset {
            Some(((addr - base_addr) / offset::PORT_STRIDE) as usize)
        } else {
            None
        }
    } else {
        None
    }
}

pub const fn get_portsc_index(addr: u64) -> Option<usize> {
    get_port_index_from_addr(addr, offset::PORTSC, MAX_PORTS, 0)
}

pub const fn get_portli_index(addr: u64) -> Option<usize> {
    get_port_index_from_addr(addr, offset::PORTSC, MAX_PORTS, 0x8)
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
        let msg = PortMessage::Detach(identifier, responder);
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
    use super::*;

    #[test]
    fn portsc_read_write() {
        let reg = PortscRegister::default();
        reg.set(0x00260203);
        assert_eq!(reg.read(), 0x00260203);

        reg.write(0x0);
        assert_eq!(
            reg.read(),
            0x00260203,
            "writing 0 should affect neither the read-only nor the RW1C bits."
        );

        reg.write(0x00200000);
        assert_eq!(
            reg.read(),
            0x00060203,
            "writing 1 to bit 21 should clear the bit."
        );

        reg.write(0x00040000);
        assert_eq!(
            reg.read(),
            0x00020203,
            "writing 1 to bit 18 should clear the bit."
        );

        reg.write(0x00020000);
        assert_eq!(
            reg.read(),
            0x00000203,
            "writing 1 to bit 17 should clear the bit."
        );
    }
}
