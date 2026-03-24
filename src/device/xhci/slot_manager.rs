use tokio::{
    runtime,
    sync::{mpsc, oneshot},
};
use tracing::{trace, warn};

use crate::{
    device::{
        bus::{BusDeviceRef, Request, RequestSize},
        pci::{
            constants::xhci::{
                device_slots::{endpoint_state, slot_state},
                MAX_SLOTS,
            },
            registers::{ConfigureRegister, DcbaapRegister},
            trb::{AddressDeviceCommandTrbData, CompletionCode},
        },
        xhci::{endpoint::EndpointMessage, endpoint_launcher::LaunchRequest},
    },
    one_indexed_array::OneIndexed,
};

#[derive(Debug)]
pub struct SlotManager {
    pub config_reg: ConfigureRegister,
    pub dcbaap: DcbaapRegister,
    pub msg_send: mpsc::UnboundedSender<SlotMessage>,
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

    pub fn doorbell(&self, slot_id: u8, endpoint_id: u8) {
        trace!("Doorbell for slot {slot_id} endpoint {endpoint_id}");
        self.msg_send
            .send(SlotMessage::Doorbell(slot_id, endpoint_id));
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

    async fn run(mut self) -> ! {
        loop {
            let msg = self
                .msg_recv
                .recv()
                .await
                .expect("channel should never close");
            match msg {
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
                    ep_sender.send(EndpointMessage::Doorbell);
                }
                SlotMessage::EnableSlot(sender) => {
                    let result = self.allocate_slot();
                    sender.send(result);
                }
                SlotMessage::DisableSlot(slot_id, sender) => {
                    let result = self.free_slot(slot_id);
                    sender.send(result);
                }
                SlotMessage::AddressDevice(trb_data, sender) => {
                    let slot = match self
                        .slots
                        .get_mut(trb_data.slot_id as usize)
                        .and_then(|opt| opt.as_mut())
                    {
                        Some(slot) => slot,
                        None => {
                            sender.send(CompletionCode::SlotNotEnabledError);
                            continue;
                        }
                    };

                    let result = slot
                        .handle_address_device(
                            trb_data.input_context_pointer,
                            trb_data.block_set_address_request,
                        )
                        .await;
                    sender.send(result);
                }
            }
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
    endpoint_senders: OneIndexed<Option<mpsc::UnboundedSender<EndpointMessage>>, 31>,
    ep_launch_sender: mpsc::UnboundedSender<LaunchRequest>,
}

#[derive(Debug, PartialEq, Eq)]
enum SlotState {
    Enabled,
    Default,
    Addressed,
    Configured,
}

type EndpointNotConfiguredError = ();

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
    pub async fn handle_address_device(
        &mut self,
        input_context_pointer: u64,
        bsr: bool,
    ) -> CompletionCode {
        // address device with BSR=1 transitions from Enabled to Default
        // address device with BSR=0 transitions from Enabled/Default to Addressed
        //
        // we currently do not do address assigning to the device.
        // Theoretically, our root-hub-port-based device identification could
        // target an incorrect device (after quick detach--reattach).

        // we should not touch anything if the input is bad
        if self.check_slot_and_ep0_input_context(input_context_pointer) == false {
            return CompletionCode::ParameterError;
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

                self.write_slot_state(slot_state::ADDRESSED);
            }
            (SlotState::Default, false) => {
                // another AddressDevice (with BSR=1) transitioned us to Default.

                // no need to read dcbaae again

                // ep0 worker is already running. Terminate before copying ep0 context.
                let (send, recv) = oneshot::channel();
                self.send_to_endpoint(1, EndpointMessage::Terminate(send));
                recv.await;

                self.dma_copy_slot_and_ep0_context(input_context_pointer);

                self.write_slot_state(slot_state::ADDRESSED);
            }
            _ => return CompletionCode::ContextStateError,
        }

        // Safety: either base_address was already initialized or we just initialized
        let context = EndpointContext::new(
            self.base_address.unwrap().wrapping_add(32),
            self.dma_bus.clone(),
        );
        // set endpoint state here and not in the worker, so we do not need to wait for worker startup
        context.set_state(endpoint_state::RUNNING);

        let (send, recv) = oneshot::channel();
        let launch_request = LaunchRequest {
            slot_id: self.id,
            endpoint_id: 1,
            endpoint_context: context,
            responder: send,
        };
        self.ep_launch_sender.send(launch_request);
        let ep_sender = recv.await.expect("endpoint launcher should always answer");
        self.endpoint_senders[1] = Some(ep_sender);

        CompletionCode::Success
    }

    fn check_slot_and_ep0_input_context(&self, _input_context_pointer: u64) -> bool {
        // TODO look if the fields have proper values
        true
    }

    fn dma_copy_slot_and_ep0_context(&self, input_context_pointer: u64) {
        let mut context_buffer = [0; 32];

        let slot_context_addr = input_context_pointer;
        self.dma_bus
            .read_bulk(slot_context_addr, &mut context_buffer);
        self.dma_bus.write_bulk(slot_context_addr, &context_buffer);

        let ep_context_addr = input_context_pointer.wrapping_add(32);
        self.dma_bus.read_bulk(ep_context_addr, &mut context_buffer);
        self.dma_bus.write_bulk(ep_context_addr, &context_buffer);
    }

    pub fn handle_configure_endpoint(
        &mut self,
        _input_context_pointer: u64,
        deconfigure: bool,
    ) -> CompletionCode {
        // configure endpoint with DC=0 transitions from Addressed/Configured to Configured
        // configure endpoint with DC=1 transitions from Configured to Addressed
        if (!deconfigure
            && !(self.state == SlotState::Addressed || self.state == SlotState::Configured))
            || (deconfigure && self.state != SlotState::Configured)
        {
            return CompletionCode::ContextStateError;
        }

        todo!();
        // CompletionCode::Success
    }

    pub fn handle_evaluate_context(&self, _input_context_pointer: u64) -> CompletionCode {
        todo!();
    }

    pub fn send_to_endpoint(
        &self,
        endpoint_id: u8,
        msg: EndpointMessage,
    ) -> Result<(), EndpointNotConfiguredError> {
        assert!(endpoint_id >= 1 && endpoint_id <= 31);

        match &self.endpoint_senders[endpoint_id as usize] {
            Some(sender) => {
                sender.send(msg);
                Ok(())
            }
            None => Err(()),
        }
    }
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

    fn get_state(&self) -> u8 {
        self.dma_bus
            .read(Request::new(self.address, RequestSize::Size1)) as u8
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

    pub fn get_root_hub_port(&self) -> u8 {
        self.dma_bus.read(Request::new(
            self.address.wrapping_add(6),
            RequestSize::Size1,
        )) as u8
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
