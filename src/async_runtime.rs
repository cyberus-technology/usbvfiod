use anyhow::{anyhow, Context, Result};
use std::sync::OnceLock;
use tokio::runtime::Handle;

static RUNTIME_HANDLE: OnceLock<Handle> = OnceLock::new();

pub fn runtime() -> &'static Handle {
    RUNTIME_HANDLE.get().expect("runtime not initialized")
}

pub fn init_runtime() -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to build tokio runtime")?;

    RUNTIME_HANDLE
        .set(rt.handle().clone())
        .map_err(|_| anyhow!("runtime already initialized"))?;

    // Keep runtime alive forever
    std::mem::forget(rt);

    Ok(())
}
