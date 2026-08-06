use tokio::{runtime, sync::oneshot};
use tracing::error;

use crate::device::{pci::constants::xhci::operational::usbcmd, xhci::registers::UsbcmdRegister};

/// Sends reset requests to a component and reports when the reset finished.
///
/// The completion notifier must be triggered by the worker after it has applied
/// its reset side effects.
pub trait ResetSender: Send + Sync {
    fn send_reset(&self, completion_notifier: oneshot::Sender<()>) -> anyhow::Result<()>;

    fn reset(&self) -> anyhow::Result<oneshot::Receiver<()>> {
        let (send, recv) = oneshot::channel();
        self.send_reset(send)?;

        Ok(recv)
    }
}

/// Coordinates host controller reset across all resettable xHCI components.
///
/// The coordinator waits for `USBCMD.HCRST`, resets all registered components,
/// and clears `HCRST` after all reset completion signals were received.
pub struct ResetCoordinator {
    usbcmd: UsbcmdRegister,
    reset_senders: [Box<dyn ResetSender>; 3],
}

impl ResetCoordinator {
    pub fn start(
        usbcmd: UsbcmdRegister,
        reset_senders: [Box<dyn ResetSender>; 3],
        async_runtime: &runtime::Handle,
    ) {
        let coordinator = Self {
            usbcmd,
            reset_senders,
        };

        async_runtime.spawn(coordinator.run_loop());
    }

    async fn run_loop(self) {
        loop {
            self.usbcmd.hcrst_notification().await;

            if self.usbcmd.read() & usbcmd::HCRST == 0 {
                continue;
            }

            if let Err(err) = self.reset().await {
                error!("failed to reset host controller: {err}");
            }
        }
    }

    async fn reset(&self) -> anyhow::Result<()> {
        for reset_sender in &self.reset_senders {
            reset_sender.reset()?.await?;
        }
        self.usbcmd.clear_hcrst();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;

    #[derive(Debug, Clone)]
    struct TestResetSender {
        completion_sender: mpsc::UnboundedSender<oneshot::Sender<()>>,
    }

    impl ResetSender for TestResetSender {
        fn send_reset(&self, completion_notifier: oneshot::Sender<()>) -> anyhow::Result<()> {
            self.completion_sender.send(completion_notifier)?;

            Ok(())
        }
    }

    #[tokio::test]
    async fn reset_clears_hcrst_after_all_components_complete() {
        let usbcmd = UsbcmdRegister::new();
        usbcmd.write(usbcmd::HCRST);

        let (completion_sender, mut completion_receiver) = mpsc::unbounded_channel();
        let coordinator = ResetCoordinator {
            usbcmd: usbcmd.clone(),
            reset_senders: [
                Box::new(TestResetSender {
                    completion_sender: completion_sender.clone(),
                }),
                Box::new(TestResetSender {
                    completion_sender: completion_sender.clone(),
                }),
                Box::new(TestResetSender { completion_sender }),
            ],
        };

        let reset_task = tokio::spawn(async move { coordinator.reset().await });

        for _ in 0..3 {
            let completion = completion_receiver.recv().await.unwrap();
            assert_eq!(usbcmd.read() & usbcmd::HCRST, usbcmd::HCRST);
            completion.send(()).unwrap();
        }

        reset_task.await.unwrap().unwrap();
        assert_eq!(usbcmd.read() & usbcmd::HCRST, 0);
    }
}
