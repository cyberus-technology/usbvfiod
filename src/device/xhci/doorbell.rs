use std::sync::{Arc, RwLock};

use tokio::{runtime, sync::mpsc};
use tracing::{debug, warn};

use crate::device::xhci::{
    endpoint::EndpointMessage,
    real_device::{Identifier, RealDevice},
    slot_manager::SlotManager,
};

#[derive(Debug)]
pub struct DoorbellArray {
    sender: mpsc::Sender<(u8, u8)>,
}

impl DoorbellArray {
    pub fn new<RD: RealDevice, ID: Identifier>(
        async_runtime: runtime::Handle,
        slot_manager: Arc<RwLock<SlotManager>>,
    ) -> Self {
        let (sender, recv) = mpsc::channel(10);

        let worker = DoorbellWorker { recv, slot_manager };

        async_runtime.spawn(worker.run());

        DoorbellArray { sender }
    }

    pub fn ring(&self, doorbell_index: u8, value: u8) {
        self.sender.send((doorbell_index, value));
    }
}

struct DoorbellWorker {
    recv: mpsc::Receiver<(u8, u8)>,
    slot_manager: Arc<RwLock<SlotManager>>,
}

impl DoorbellWorker {
    async fn run(mut self) {
        while let Some((slot_id, endpoint_id)) = self.recv.recv().await {
            if let Some(slot) = self.slot_manager.read().unwrap().slot_ref(slot_id) {
                debug!("Doorbell (Slot {slot_id}, Endpoint {endpoint_id})");

                let result = slot.send_to_endpoint(endpoint_id, EndpointMessage::Doorbell);

                if let Err(_) = result {
                    warn!(
                        "Doorbell for disabled endpoint (Slot {slot_id}, Endpoint {endpoint_id})"
                    );
                }
            } else {
                warn!("Doorbell for disabled slot (Slot {slot_id}, Endpoint {endpoint_id})");
            }
        }
        debug!("Doorbell worker stopped (channel has closed)");
    }
}
