use std::sync::{Arc, RwLock};

use tokio::{runtime, sync::mpsc};
use tracing::{debug, warn};

use crate::device::xhci::{
    endpoint::EndpointMessage,
    real_device::{Identifier, RealDevice},
    slot_manager::SlotWorker,
};

#[derive(Debug)]
pub struct DoorbellArray {
    sender: mpsc::Sender<(u8, u8)>,
}

impl DoorbellArray {
    pub fn new<RD: RealDevice, ID: Identifier>(
        async_runtime: runtime::Handle,
        slot_manager: Arc<RwLock<SlotWorker>>,
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
    slot_manager: Arc<RwLock<SlotWorker>>,
}

impl DoorbellWorker {
    async fn run(mut self) {
        todo!();
    }
}
