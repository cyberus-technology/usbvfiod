use anyhow::anyhow;
use tokio::{
    runtime,
    sync::{mpsc, oneshot},
};
use tracing::{debug, info, trace, warn};

use crate::{
    device::{
        bus::{BusDeviceRef, Request, RequestSize},
        pci::{
            constants::xhci::{
                device_slots::{endpoint_state, slot_state},
                MAX_SLOTS,
            },
            registers::{ConfigureRegister, DcbaapRegister},
            trb::{
                AddressDeviceCommandTrbData, CompletionCode, ConfigureEndpointCommandTrbData,
                SetTrDequeuePointerCommandTrbData,
            },
        },
        xhci::{endpoint::EndpointSender, endpoint_launcher::LaunchRequest},
    },
    one_indexed_array::OneIndexed,
    oneshot_anyhow::SendWithAnyhowError,
};

#[derive(Debug)]
pub struct SlotManager {
    pub config_reg: ConfigureRegister,
    pub dcbaap: DcbaapRegister,
    msg_send: mpsc::UnboundedSender<SlotMessage>,
}

impl SlotManager {
    pub fn new(
        dma_bus: BusDeviceRef,
        async_runtime: &runtime::Handle,
        ep_launch_sender: mpsc::UnboundedSender<LaunchRequest>,
    ) -> Self {
        let config_reg = ConfigureRegister::default();
        let dcbaap = DcbaapRegister::default();
        let (msg_send, msg_recv) = mpsc::unbounded_channel();

        let worker = SlotWorker::new(
            dma_bus,
            config_reg.clone(),
            dcbaap.clone(),
            ep_launch_sender,
            msg_recv,
        );

        async_runtime.spawn(worker.run());

        Self {
            config_reg,
            dcbaap,
            msg_send,
        }
    }

    pub fn doorbell(&self, slot_id: u8, endpoint_id: u8) -> anyhow::Result<()> {
        trace!("Doorbell for slot {slot_id} endpoint {endpoint_id}");
        self.msg_send
            .send(SlotMessage::Doorbell(slot_id, endpoint_id))?;

        Ok(())
    }

    pub fn create_slot_worker_handle(&self) -> SlotWorkerHandle {
        SlotWorkerHandle {
            msg_send: self.msg_send.clone(),
        }
    }
}

#[derive(Debug)]
pub struct SlotWorker {
    slots: OneIndexed<Option<Slot>, { MAX_SLOTS as usize }>,
    config_reg: ConfigureRegister,
    dcbaap: DcbaapRegister,
    dma_bus: BusDeviceRef,
    ep_launch_sender: mpsc::UnboundedSender<LaunchRequest>,
    msg_recv: mpsc::UnboundedReceiver<SlotMessage>,
}

#[derive(Debug)]
pub enum SlotMessage {
    // slot_id, endpoint_id
    Doorbell(u8, u8),
    EnableSlot(oneshot::Sender<Result<u8, CompletionCode>>),
    DisableSlot(u8, oneshot::Sender<CompletionCode>),
    AddressDevice(AddressDeviceCommandTrbData, oneshot::Sender<CompletionCode>),
    ConfigureEndpoint(
        ConfigureEndpointCommandTrbData,
        oneshot::Sender<CompletionCode>,
    ),
    // slot_id, endpoint_id
    StopEndpoint(u8, u8, oneshot::Sender<CompletionCode>),
    ResetEndpoint(u8, u8, oneshot::Sender<CompletionCode>),
    SetTrDequeuePointer(
        SetTrDequeuePointerCommandTrbData,
        oneshot::Sender<CompletionCode>,
    ),
}

impl SlotWorker {
    fn new(
        dma_bus: BusDeviceRef,
        config_reg: ConfigureRegister,
        dcbaap: DcbaapRegister,
        ep_launch_sender: mpsc::UnboundedSender<LaunchRequest>,
        msg_recv: mpsc::UnboundedReceiver<SlotMessage>,
    ) -> Self {
        Self {
            slots: [const { None }; MAX_SLOTS as usize].into(),
            config_reg,
            dcbaap,
            dma_bus,
            ep_launch_sender,
            msg_recv,
        }
    }

    async fn next_msg(&mut self) -> anyhow::Result<SlotMessage> {
        self.msg_recv
            .recv()
            .await
            .ok_or_else(|| anyhow!("slot channel closed"))
    }

    // the function only returns with an error, but ! cannot be put in Result
    async fn run_loop(&mut self) -> anyhow::Result<()> {
        loop {
            match self.next_msg().await? {
                SlotMessage::Doorbell(slot_id, endpoint_id) => {
                    let slot = match self
                        .slots
                        .get(slot_id as usize)
                        .and_then(|opt| opt.as_ref())
                    {
                        Some(slot) => slot,
                        None => {
                            warn!("Doorbell for disabled slot {slot_id}");
                            continue;
                        }
                    };
                    let ep_sender = match slot
                        .endpoint_senders
                        .get(endpoint_id as usize)
                        .and_then(|opt| opt.as_ref())
                    {
                        Some(ep_sender) => ep_sender,
                        None => {
                            warn!(
                                "Doorbell for disabled endpoint {endpoint_id} (of slot {slot_id})"
                            );
                            continue;
                        }
                    };
                    ep_sender.doorbell()?;
                }
                SlotMessage::EnableSlot(sender) => {
                    let result = self.allocate_slot();
                    sender.send_anyhow(result)?;
                }
                SlotMessage::DisableSlot(slot_id, sender) => {
                    let result = self.free_slot(slot_id);
                    sender.send_anyhow(result)?;
                }
                SlotMessage::AddressDevice(trb_data, sender) => {
                    let slot = match self
                        .slots
                        .get_mut(trb_data.slot_id as usize)
                        .and_then(|opt| opt.as_mut())
                    {
                        Some(slot) => slot,
                        None => {
                            sender.send_anyhow(CompletionCode::SlotNotEnabledError)?;
                            continue;
                        }
                    };

                    let result = slot
                        .handle_address_device(
                            trb_data.input_context_pointer,
                            trb_data.block_set_address_request,
                        )
                        .await?;
                    sender.send_anyhow(result)?;
                }
                SlotMessage::ConfigureEndpoint(trb_data, sender) => {
                    let slot = match self
                        .slots
                        .get_mut(trb_data.slot_id as usize)
                        .and_then(|opt| opt.as_mut())
                    {
                        Some(slot) => slot,
                        None => {
                            sender.send_anyhow(CompletionCode::SlotNotEnabledError)?;
                            continue;
                        }
                    };

                    let result = slot
                        .handle_configure_endpoint(
                            trb_data.input_context_pointer,
                            trb_data.deconfigure,
                        )
                        .await?;
                    sender.send_anyhow(result)?;
                }
                SlotMessage::StopEndpoint(slot_id, endpoint_id, sender) => {
                    let slot = match self
                        .slots
                        .get(slot_id as usize)
                        .and_then(|opt| opt.as_ref())
                    {
                        Some(slot) => slot,
                        None => {
                            sender.send_anyhow(CompletionCode::SlotNotEnabledError)?;
                            continue;
                        }
                    };

                    let ep_sender = match slot
                        .endpoint_senders
                        .get(endpoint_id as usize)
                        .and_then(|opt| opt.as_ref())
                    {
                        Some(ep_sender) => ep_sender,
                        None => {
                            sender.send_anyhow(CompletionCode::EndpointNotEnabledError)?;
                            continue;
                        }
                    };

                    ep_sender.stop(sender)?;
                }
                SlotMessage::ResetEndpoint(slot_id, endpoint_id, sender) => {
                    let slot = match self
                        .slots
                        .get(slot_id as usize)
                        .and_then(|opt| opt.as_ref())
                    {
                        Some(slot) => slot,
                        None => {
                            sender.send_anyhow(CompletionCode::SlotNotEnabledError)?;
                            continue;
                        }
                    };

                    let ep_sender = match slot
                        .endpoint_senders
                        .get(endpoint_id as usize)
                        .and_then(|opt| opt.as_ref())
                    {
                        Some(ep_sender) => ep_sender,
                        None => {
                            sender.send_anyhow(CompletionCode::EndpointNotEnabledError)?;
                            continue;
                        }
                    };

                    ep_sender.reset(sender)?;
                }
                SlotMessage::SetTrDequeuePointer(trb_data, sender) => {
                    let slot = match self
                        .slots
                        .get(trb_data.slot_id as usize)
                        .and_then(|opt| opt.as_ref())
                    {
                        Some(slot) => slot,
                        None => {
                            sender.send_anyhow(CompletionCode::SlotNotEnabledError)?;
                            continue;
                        }
                    };

                    let ep_sender = match slot
                        .endpoint_senders
                        .get(trb_data.endpoint_id as usize)
                        .and_then(|opt| opt.as_ref())
                    {
                        Some(ep_sender) => ep_sender,
                        None => {
                            sender.send_anyhow(CompletionCode::EndpointNotEnabledError)?;
                            continue;
                        }
                    };

                    ep_sender.set_tr_dequeue_pointer(
                        trb_data.dequeue_pointer,
                        trb_data.dequeue_cycle_state,
                        sender,
                    )?;
                }
            }
        }
    }

    async fn run(mut self) {
        match self.run_loop().await {
            Ok(_) => unreachable!(),
            Err(err) => info!("SlotWorker stopped {err}"),
        }
    }

    pub fn allocate_slot(&mut self) -> Result<u8, CompletionCode> {
        let available_slot_id = (1..=self.config_reg.num_slots_enabled())
            .find(|&slot_id| matches!(self.slots[slot_id as usize], None));

        if let Some(slot_id) = available_slot_id {
            let dcbaae = self.dcbaap.read().wrapping_add(slot_id as u64 * 8);
            self.slots[slot_id as usize] = Some(Slot::new(
                slot_id,
                dcbaae,
                self.dma_bus.clone(),
                self.ep_launch_sender.clone(),
            ));
        }

        available_slot_id.ok_or(CompletionCode::NoSlotsAvailableError)
    }

    pub fn free_slot(&mut self, slot_id: u8) -> CompletionCode {
        assert!(slot_id >= 1 && slot_id <= self.config_reg.num_slots_enabled());

        let slot = &mut self.slots[slot_id as usize];
        if matches!(slot, None) {
            return CompletionCode::SlotNotEnabledError;
        }
        *slot = None;
        CompletionCode::Success
    }

    // pub fn slot_ref(&self, slot_id: u8) -> Option<impl Deref<Target = Slot> + '_> {
    //     assert!(matches!(self.slots[slot_id as usize], None));

    //     self.slots[slot_id as usize]
    //         .as_ref()
    //         .map(|rwlock| rwlock.read().unwrap())
    // }

    // pub fn slot_mut(&self, slot_id: u8) -> Option<impl DerefMut<Target = Slot> + '_> {
    //     assert!(matches!(self.slots[slot_id as usize], None));

    //     self.slots[slot_id as usize]
    //         .as_ref()
    //         .map(|rwlock| rwlock.write().unwrap())
    // }
}

#[derive(Debug)]
struct Slot {
    id: u8,
    state: SlotState,
    // guest address of the DCBAA entry of this slot
    dcbaae: u64,
    // read from dcbaae once valid (through AddressDevice)
    base_address: Option<u64>,
    dma_bus: BusDeviceRef,
    endpoint_senders: OneIndexed<Option<EndpointSender>, 31>,
    ep_launch_sender: mpsc::UnboundedSender<LaunchRequest>,
}

#[derive(Debug, PartialEq, Eq)]
enum SlotState {
    Enabled,
    Default,
    Addressed,
    Configured,
}

impl Slot {
    fn new(
        id: u8,
        dcbaae: u64,
        dma_bus: BusDeviceRef,
        ep_launch_sender: mpsc::UnboundedSender<LaunchRequest>,
    ) -> Self {
        Self {
            id,
            state: SlotState::Enabled,
            dcbaae,
            base_address: None,
            dma_bus,
            endpoint_senders: [const { None }; 31].into(),
            ep_launch_sender,
        }
    }

    // &mut self is not technically necessary, but we take it to ensure
    // DMA accesses to the slot context are exclusive
    fn write_slot_state(&mut self, state: u8) {
        if let Some(base_address) = self.base_address {
            let addr = base_address.wrapping_add(15);
            self.dma_bus
                .write(Request::new(addr, RequestSize::Size1), (state << 3) as u64);
        } else {
            panic!("Tried to access slot context before knowing base_address");
        }
    }

    // 4.6.5 for reference
    async fn handle_address_device(
        &mut self,
        input_context_pointer: u64,
        bsr: bool,
    ) -> anyhow::Result<CompletionCode> {
        // address device with BSR=1 transitions from Enabled to Default
        // address device with BSR=0 transitions from Enabled/Default to Addressed
        //
        // we currently do not do address assigning to the device.
        // Theoretically, our root-hub-port-based device identification could
        // target an incorrect device (after quick detach--reattach).

        // we should not touch anything if the input is bad
        if self.check_slot_and_ep0_input_context(input_context_pointer) == false {
            return Ok(CompletionCode::ParameterError);
        }

        match (&self.state, bsr) {
            (SlotState::Enabled, true) => {
                // We transition from Enabled only to Default, a second AddressDevice
                // with BSR=0 will then transition to Addressed

                // DCBAA entry of this slot is now valid. Load and cache.
                self.base_address = Some(
                    self.dma_bus
                        .read(Request::new(self.dcbaae, RequestSize::Size8)),
                );

                self.dma_copy_slot_and_ep0_context(input_context_pointer);

                // set slot state
                self.state = SlotState::Default;
                self.write_slot_state(slot_state::DEFAULT);
            }
            (SlotState::Enabled, false) => {
                // We transition from Enabled directly to Addressed (USB-3 case)

                // DCBAA entry of this slot is now valid. Load and cache.
                self.base_address = Some(
                    self.dma_bus
                        .read(Request::new(self.dcbaae, RequestSize::Size8)),
                );

                self.dma_copy_slot_and_ep0_context(input_context_pointer);

                self.state = SlotState::Addressed;
                self.write_slot_state(slot_state::ADDRESSED);
            }
            (SlotState::Default, false) => {
                // another AddressDevice (with BSR=1) transitioned us to Default.

                // no need to read dcbaae again

                // ep0 worker is already running. Terminate before copying ep0 context.
                self.deconfigure_endpoint(1).await?;

                self.dma_copy_slot_and_ep0_context(input_context_pointer);

                self.state = SlotState::Addressed;
                self.write_slot_state(slot_state::ADDRESSED);
            }
            _ => return Ok(CompletionCode::ContextStateError),
        }

        // Safety: either base_address was already initialized or we just initialized
        let context = EndpointContext::new(
            self.base_address.unwrap().wrapping_add(32),
            self.dma_bus.clone(),
        );
        // set endpoint state here and not in the worker, so we do not need to wait for worker startup
        context.set_state(endpoint_state::RUNNING);

        self.configure_endpoint(1).await?;

        Ok(CompletionCode::Success)
    }

    fn check_slot_and_ep0_input_context(&self, _input_context_pointer: u64) -> bool {
        // TODO look if the fields have proper values
        true
    }

    fn dma_copy_slot_and_ep0_context(&self, input_context_pointer: u64) {
        let base_addr = match self.base_address {
            Some(base_addr) => base_addr,
            None => panic!(
                "do not call dma_copy_slot_and_ep0_context when base_addr is not initialized"
            ),
        };
        let mut context_buffer = [0; 32];

        let input_slot_context_addr = input_context_pointer.wrapping_add(32);
        self.dma_bus
            .read_bulk(input_slot_context_addr, &mut context_buffer);
        self.dma_bus.write_bulk(base_addr, &context_buffer);

        let input_ep_context_addr = input_context_pointer.wrapping_add(64);
        let ep_context_addr = base_addr.wrapping_add(32);
        self.dma_bus
            .read_bulk(input_ep_context_addr, &mut context_buffer);
        self.dma_bus.write_bulk(ep_context_addr, &context_buffer);
    }

    fn dma_copy_ep_context(&self, endpoint_id: u8, input_context_pointer: u64) {
        let base_addr = match self.base_address {
            Some(base_addr) => base_addr,
            None => panic!("do not call dma_copy_ep_context when base_addr is not initialized"),
        };
        let mut context_buffer = [0; 32];

        let input_ep_context_addr =
            input_context_pointer.wrapping_add((endpoint_id as u64 + 1) * 32);
        let ep_context_addr = base_addr.wrapping_add(endpoint_id as u64 * 32);
        self.dma_bus
            .read_bulk(input_ep_context_addr, &mut context_buffer);
        self.dma_bus.write_bulk(ep_context_addr, &context_buffer);
    }

    fn root_hub_port(&self) -> u8 {
        let base_addr = match self.base_address {
            Some(base_addr) => base_addr,
            None => panic!("do not call root_hub_port when base_addr is not initialized"),
        };

        self.dma_bus
            .read(Request::new(base_addr.wrapping_add(6), RequestSize::Size1)) as u8
    }

    async fn handle_configure_endpoint(
        &mut self,
        input_context_pointer: u64,
        deconfigure: bool,
    ) -> anyhow::Result<CompletionCode> {
        // configure endpoint with DC=0 transitions from Addressed/Configured to Configured
        // configure endpoint with DC=1 transitions from Configured to Addressed
        if (!deconfigure
            && !(self.state == SlotState::Addressed || self.state == SlotState::Configured))
            || (deconfigure && self.state != SlotState::Configured)
        {
            return Ok(CompletionCode::ContextStateError);
        }

        // TODO input checks

        let drop_flags = self
            .dma_bus
            .read(Request::new(input_context_pointer, RequestSize::Size4));
        let add_flags = self.dma_bus.read(Request::new(
            input_context_pointer.wrapping_add(4),
            RequestSize::Size4,
        ));

        let mut to_deconfigure = Vec::new();
        let mut to_configure = Vec::new();

        for i in 2..=31 {
            if drop_flags & (1 << i) != 0 {
                debug!("D{i} set");
                to_deconfigure.push(i);
            }
            if add_flags & (1 << i) != 0 {
                debug!("A{i} set");
                to_configure.push(i);
            }
        }

        for ep in to_deconfigure {
            self.deconfigure_endpoint(ep).await?;
        }

        for ep in to_configure {
            self.dma_copy_ep_context(ep, input_context_pointer);
            self.configure_endpoint(ep).await?;
        }

        self.state = SlotState::Configured;
        self.write_slot_state(slot_state::CONFIGURED);

        Ok(CompletionCode::Success)
    }

    // helper method for address_advice and configure_endpoint.
    // do only call for already disabled endpoints.
    async fn deconfigure_endpoint(&mut self, endpoint_id: u8) -> anyhow::Result<()> {
        self.endpoint_senders[endpoint_id as usize]
            .as_ref()
            .expect("deconfigure_endpoint called on disabled endpoint")
            .terminate()
            .await?;
        self.endpoint_senders[endpoint_id as usize] = None;

        Ok(())
    }

    // helper method for address_advice and configure_endpoint.
    // do only call for already enabled endpoints.
    async fn configure_endpoint(&mut self, endpoint_id: u8) -> anyhow::Result<()> {
        // Safety: either base_address was already initialized or we just initialized
        let context = EndpointContext::new(
            self.base_address
                .unwrap()
                .wrapping_add(endpoint_id as u64 * 32),
            self.dma_bus.clone(),
        );
        // set endpoint state here and not in the worker, so we do not need to wait for worker startup
        context.set_state(endpoint_state::RUNNING);

        let (send, recv) = oneshot::channel();
        let launch_request = LaunchRequest {
            slot_id: self.id,
            endpoint_id,
            root_hub_port: self.root_hub_port(),
            endpoint_context: context,
            responder: send,
        };
        self.ep_launch_sender.send(launch_request)?;
        let ep_sender = recv.await?;
        self.endpoint_senders[endpoint_id as usize] = Some(ep_sender);

        Ok(())
    }

    fn handle_evaluate_context(&self, _input_context_pointer: u64) -> CompletionCode {
        todo!();
    }

    // fn send_to_endpoint(&self, endpoint_id: u8, msg: EndpointMessage) -> anyhow::Result<()> {
    //     self.endpoint_senders[endpoint_id as usize]
    //         .as_ref()
    //         .expect("send_to_endpoint called on disabled endpoint")
    //         .send(msg)?;

    //     Ok(())
    // }
}

/// A wrapper around DMA accesses to endpoint context structures.
///
/// The structure is explained in the XHCI spec 6.2.3.
/// An endpoint context has a size of 32 bytes, lies in guest memory, and
/// contains information about an endpoint, most importantly for us the dequeue
/// pointer and cycle state of the associated transfer ring.
#[derive(Debug)]
pub struct EndpointContext {
    /// The address of the endpoint context in guest memory.
    address: u64,
    /// Reference to the guest memory.
    dma_bus: BusDeviceRef,
}

impl EndpointContext {
    /// Create a new instance.
    ///
    /// # Parameters
    ///
    /// - address: the address of the endpoint context in guest memory.
    /// - dma_bus: reference to the guest memory.
    pub const fn new(address: u64, dma_bus: BusDeviceRef) -> Self {
        Self { address, dma_bus }
    }

    /// DMA read the dequeue pointer and consumer cycle state of the endpoint's
    /// transfer ring.
    pub fn get_dequeue_pointer_and_cycle_state(&self) -> (u64, bool) {
        let bytes = self.dma_bus.read(Request::new(
            self.address.wrapping_add(8),
            RequestSize::Size8,
        ));
        let dequeue_pointer = bytes & !0xf;
        let cycle_state = bytes & 0x1 != 0;
        (dequeue_pointer, cycle_state)
    }

    /// DMA write the dequeue pointer and consumer cycle state of the endpoint's
    /// transfer ring.
    ///
    /// Call this function after retrieving TRBs from the transfer ring.
    pub fn set_dequeue_pointer_and_cycle_state(&self, dequeue_pointer: u64, cycle_state: bool) {
        assert!(
            dequeue_pointer & 0xf == 0,
            "dequeue_pointer has to be aligned to 16 bytes"
        );
        self.dma_bus.write(
            Request::new(self.address.wrapping_add(8), RequestSize::Size8),
            dequeue_pointer | cycle_state as u64,
        );
    }

    pub fn set_state(&self, state: u8) {
        self.dma_bus
            .write(Request::new(self.address, RequestSize::Size1), state as u64);
    }

    pub fn get_endpoint_type(&self) -> EndpointType {
        let guest_mem_byte = self.dma_bus.read(Request::new(
            self.address.wrapping_add(4),
            RequestSize::Size1,
        ));
        match (guest_mem_byte >> 3) & 0x7 {
            2 => EndpointType::BulkOut,
            6 => EndpointType::BulkIn,
            4 => EndpointType::Control,
            7 => EndpointType::InterruptIn,
            3 => EndpointType::InterruptOut,
            val => {
                warn!("encountered unsupported endpoint type: {}", val);
                EndpointType::Unsupported
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointType {
    Control,
    BulkIn,
    BulkOut,
    InterruptIn,
    InterruptOut,
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct SlotWorkerHandle {
    msg_send: mpsc::UnboundedSender<SlotMessage>,
}

impl SlotWorkerHandle {
    pub async fn enable_slot(&self) -> anyhow::Result<Result<u8, CompletionCode>> {
        let (send, recv) = oneshot::channel();
        let msg = SlotMessage::EnableSlot(send);
        self.msg_send.send(msg)?;
        let response = recv.await?;
        Ok(response)
    }

    pub async fn disable_slot(&self, slot_id: u8) -> anyhow::Result<CompletionCode> {
        let (send, recv) = oneshot::channel();
        let msg = SlotMessage::DisableSlot(slot_id, send);
        self.msg_send.send(msg)?;
        let completion_code = recv.await?;
        Ok(completion_code)
    }

    pub async fn address_device(
        &self,
        trb_data: AddressDeviceCommandTrbData,
    ) -> anyhow::Result<CompletionCode> {
        let (send, recv) = oneshot::channel();
        let msg = SlotMessage::AddressDevice(trb_data, send);
        self.msg_send.send(msg)?;
        let completion_code = recv.await?;
        Ok(completion_code)
    }

    pub async fn configure_endpoint(
        &self,
        trb_data: ConfigureEndpointCommandTrbData,
    ) -> anyhow::Result<CompletionCode> {
        let (send, recv) = oneshot::channel();
        let msg = SlotMessage::ConfigureEndpoint(trb_data, send);
        self.msg_send.send(msg)?;
        let completion_code = recv.await?;
        Ok(completion_code)
    }

    pub async fn stop_endpoint(
        &self,
        slot_id: u8,
        endpoint_id: u8,
    ) -> anyhow::Result<CompletionCode> {
        let (send, recv) = oneshot::channel();
        let msg = SlotMessage::StopEndpoint(slot_id, endpoint_id, send);
        self.msg_send.send(msg)?;
        let completion_code = recv.await?;
        Ok(completion_code)
    }

    pub async fn reset_endpoint(
        &self,
        slot_id: u8,
        endpoint_id: u8,
    ) -> anyhow::Result<CompletionCode> {
        let (send, recv) = oneshot::channel();
        let msg = SlotMessage::ResetEndpoint(slot_id, endpoint_id, send);
        self.msg_send.send(msg)?;
        let completion_code = recv.await?;
        Ok(completion_code)
    }

    pub async fn set_tr_dequeue_pointer(
        &self,
        trb_data: SetTrDequeuePointerCommandTrbData,
    ) -> anyhow::Result<CompletionCode> {
        let (send, recv) = oneshot::channel();
        let msg = SlotMessage::SetTrDequeuePointer(trb_data, send);
        self.msg_send.send(msg)?;
        let completion_code = recv.await?;
        Ok(completion_code)
    }
}
