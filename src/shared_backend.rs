//! Share a single [`XhciBackend`] between several vfio-user client connections.
//!
//! usbvfiod historically served exactly one vfio-user client and exited when it
//! disconnected. During a same-host live migration the destination VMM connects
//! while the source is still connected, which a single-`accept()` server cannot
//! serve: the destination blocks forever in the vfio-user version handshake and
//! the migration never completes.
//!
//! The handshake commands (`Version`, `DeviceGetInfo`, `DeviceGetRegionInfo`,
//! `GetIrqInfo`) never touch the backend, whereas the data path commands
//! (`RegionRead`/`RegionWrite`/`DmaMap`/`DmaUnmap`/`SetIrqs`/`DeviceReset`) do.
//! This wrapper therefore takes the backend lock **per command** instead of per
//! connection, so a second client can complete its handshake while the first
//! one is still connected.
//!
//! # Stale clients must not tear the device down
//!
//! After a successful migration the *source* VMM shuts its device down and
//! sends `SetIrqs` with no file descriptors (disable) and `DmaUnmap` for the
//! mappings it owns. Both would land on the shared backend *after* the
//! destination has already registered its own interrupts and DMA mappings,
//! leaving the surviving VMM with a dummy interrupt line - the guest then sees
//! the xHCI controller stop responding and declares it dead.
//!
//! The wrapper therefore tracks which connection most recently (re)registered
//! interrupts and only lets that connection issue the destructive counterparts.

use std::{
    fs::File,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use tracing::warn;
use vfio_user::{DmaMapFlags, DmaUnmapFlags, ServerBackend};

use crate::{device::xhci::real_device::CompleteRealDevice, xhci_backend::XhciBackend};

/// Sentinel for "no connection owns the interrupt line yet".
const NO_OWNER: u64 = u64::MAX;

/// State shared by every connection serving the same controller.
#[derive(Debug)]
pub struct SharedBackendState<CRD: CompleteRealDevice> {
    backend: Mutex<XhciBackend<CRD>>,
    /// Id of the connection that most recently registered interrupts.
    irq_owner: AtomicU64,
    /// Hands out a unique id per connection.
    next_id: AtomicU64,
}

impl<CRD: CompleteRealDevice> SharedBackendState<CRD> {
    pub const fn new(backend: XhciBackend<CRD>) -> Self {
        Self {
            backend: Mutex::new(backend),
            irq_owner: AtomicU64::new(NO_OWNER),
            next_id: AtomicU64::new(0),
        }
    }

    /// Create a handle for one client connection.
    pub fn connect(self: &Arc<Self>) -> SharedBackend<CRD> {
        SharedBackend {
            id: self.next_id.fetch_add(1, Ordering::SeqCst),
            state: Arc::clone(self),
        }
    }
}

/// A single vfio-user client connection onto a shared [`XhciBackend`].
#[derive(Debug)]
pub struct SharedBackend<CRD: CompleteRealDevice> {
    id: u64,
    state: Arc<SharedBackendState<CRD>>,
}

impl<CRD: CompleteRealDevice> SharedBackend<CRD> {
    fn backend(&self) -> MutexGuard<'_, XhciBackend<CRD>> {
        // A panicking client must not take the whole controller down, so
        // recover from a poisoned mutex rather than propagating the panic.
        self.state
            .backend
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// True if this connection is allowed to issue destructive teardown.
    fn owns_device(&self) -> bool {
        self.state.irq_owner.load(Ordering::SeqCst) == self.id
    }
}

impl<CRD: CompleteRealDevice> ServerBackend for SharedBackend<CRD> {
    fn region_read(
        &mut self,
        region: u32,
        offset: u64,
        data: &mut [u8],
    ) -> Result<(), std::io::Error> {
        self.backend().region_read(region, offset, data)
    }

    fn region_write(
        &mut self,
        region: u32,
        offset: u64,
        data: &[u8],
    ) -> Result<(), std::io::Error> {
        self.backend().region_write(region, offset, data)
    }

    fn dma_map(
        &mut self,
        flags: DmaMapFlags,
        offset: u64,
        address: u64,
        size: u64,
        fd: Option<File>,
    ) -> Result<(), std::io::Error> {
        self.backend().dma_map(flags, offset, address, size, fd)
    }

    fn dma_unmap(
        &mut self,
        flags: DmaUnmapFlags,
        address: u64,
        size: u64,
    ) -> Result<(), std::io::Error> {
        // A departing VMM unmaps the regions it created. If another connection
        // has taken the device over in the meantime, that unmap must not tear
        // down the new owner's mappings.
        if !self.owns_device() {
            warn!(
                "ignoring DMA unmap at {address:#x} from stale vfio-user client {}",
                self.id
            );
            return Ok(());
        }
        self.backend().dma_unmap(flags, address, size)
    }

    fn reset(&mut self) -> Result<(), std::io::Error> {
        self.backend().reset()
    }

    fn set_irqs(
        &mut self,
        index: u32,
        flags: u32,
        start: u32,
        count: u32,
        fds: Vec<File>,
    ) -> Result<(), std::io::Error> {
        // An empty fd list means "disable interrupts". Only the connection that
        // currently owns the interrupt line may do that; otherwise the source
        // VMM of a finished migration would disable the destination's line.
        if fds.is_empty() && !self.owns_device() {
            warn!(
                "ignoring IRQ disable from stale vfio-user client {}",
                self.id
            );
            return Ok(());
        }

        let registering = !fds.is_empty();
        let result = self.backend().set_irqs(index, flags, start, count, fds);

        if result.is_ok() && registering {
            self.state.irq_owner.store(self.id, Ordering::SeqCst);
        }

        result
    }
}
