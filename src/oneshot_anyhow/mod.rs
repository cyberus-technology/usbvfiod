use anyhow::anyhow;
use tokio::sync::oneshot;

pub trait SendWithAnyhowError<T> {
    fn send_anyhow(self, msg: T) -> anyhow::Result<()>;
}

impl<T> SendWithAnyhowError<T> for oneshot::Sender<T> {
    fn send_anyhow(self, msg: T) -> anyhow::Result<()> {
        self.send(msg)
            .map_err(|_| anyhow!("oneshot channel closed"))
    }
}
