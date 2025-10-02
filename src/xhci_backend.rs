use std::{
    fs::File,
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use nusb::MaybeFuture;
use tracing::{debug, info, trace};

use vfio_bindings::bindings::vfio::{
    vfio_region_info, VFIO_PCI_BAR0_REGION_INDEX, VFIO_PCI_BAR1_REGION_INDEX,
    VFIO_PCI_BAR2_REGION_INDEX, VFIO_PCI_BAR3_REGION_INDEX, VFIO_PCI_BAR4_REGION_INDEX,
    VFIO_PCI_BAR5_REGION_INDEX, VFIO_PCI_CONFIG_REGION_INDEX, VFIO_PCI_MSIX_IRQ_INDEX,
    VFIO_PCI_NUM_IRQS, VFIO_PCI_NUM_REGIONS, VFIO_REGION_INFO_FLAG_READ,
    VFIO_REGION_INFO_FLAG_WRITE,
};
use vfio_user::{IrqInfo, ServerBackend};

use crate::device::{
    bus::{Request, RequestSize},
    interrupt_line::{DummyInterruptLine, InterruptLine},
    pci::{nusb::NusbDeviceWrapper, traits::PciDevice, xhci::XhciController},
};

use crate::{dynamic_bus::DynamicBus, memory_segment::MemorySegment};

#[derive(Debug)]
pub struct XhciBackend {
    dma_bus: Arc<DynamicBus>,
    controller: Mutex<XhciController>,
}

#[derive(Debug)]
struct InterruptEventFd {
    /// TODO: Get rid of the Mutex. Writes to the EventFd are safe.
    ///  This just satisfies the Send + Sync requirements and provides
    ///  interior mutability for the [`InterruptLine`] trait.
    fd: Mutex<File>,
}

impl InterruptLine for InterruptEventFd {
    fn interrupt(&self) {
        // Ensure all guest memory modifications can be observed by another process.
        // The sequentially consistent fence emits a hardware memory barrier operation.
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);

        // Write any 8 byte value to the EventFd.
        // TODO: we just expect this to always work currently.
        let _amount = self
            .fd
            .lock()
            .unwrap()
            .write(&1u64.to_le_bytes())
            .expect("should always be able to write event fd");
    }
}

impl XhciBackend {
    /// Create a new virtual XHCI controller with the given USB
    /// devices attached at creation time.
    pub fn new<I>(devices: I) -> Result<Self>
    where
        I: IntoIterator,
        I::Item: AsRef<Path>,
    {
        let dma_bus = Arc::new(DynamicBus::new());

        let backend = Self {
            controller: Mutex::new(XhciController::new(dma_bus.clone())),
            dma_bus,
        };

        for device in devices {
            backend.add_device_from_path(device)?;
        }

        Ok(backend)
    }

    /// Add a USB device to the virtual XHCI controller.
    fn add_device(&self, device: nusb::Device) -> Result<()> {
        // Add the device to the XHCI controller.
        let wrapped_device = Box::new(NusbDeviceWrapper::new(device));
        self.controller.lock().unwrap().set_device(wrapped_device);

        Ok(())
    }

    /// Add a USB device via its path in `/dev/bus/usb`.
    pub fn add_device_from_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let path: &Path = path.as_ref();
        let open_file = |err_msg| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .with_context(|| format!("{}: {}", err_msg, path.display()))
        };

        let file = open_file("Failed to open USB device file")?;
        let device = nusb::Device::from_fd(file.into()).wait()?;
        device.reset().wait()?;

        // After the reset, the device instance is no longer usable and we need
        // to reopen.
        let file = open_file("Failed to open USB device file after device reset")?;
        self.add_device(nusb::Device::from_fd(file.into()).wait()?)
    }
}

impl XhciBackend {
    /// Return a list of regions for [`vfio_user::Server::new`].
    pub fn regions(&self) -> Vec<vfio_region_info> {
        (0..VFIO_PCI_NUM_REGIONS)
            .map(|i| {
                let empty_region = vfio_region_info {
                    argsz: size_of::<vfio_region_info>() as u32,
                    index: i,
                    ..Default::default()
                };

                match i {
                    VFIO_PCI_CONFIG_REGION_INDEX => {
                        debug!("Client queried config space region");

                        vfio_region_info {
                            argsz: size_of::<vfio_region_info>() as u32,
                            index: i,
                            size: 256,
                            flags: VFIO_REGION_INFO_FLAG_READ | VFIO_REGION_INFO_FLAG_WRITE,
                            ..Default::default()
                        }
                    }

                    VFIO_PCI_BAR0_REGION_INDEX
                    | VFIO_PCI_BAR1_REGION_INDEX
                    | VFIO_PCI_BAR2_REGION_INDEX
                    | VFIO_PCI_BAR3_REGION_INDEX
                    | VFIO_PCI_BAR4_REGION_INDEX
                    | VFIO_PCI_BAR5_REGION_INDEX => {
                        let bar_no = i - VFIO_PCI_BAR0_REGION_INDEX;

                        u8::try_from(bar_no)
                            .ok()
                            .and_then(|bar_no| self.controller.bar(bar_no))
                            .map_or_else(
                                || {
                                    debug!("Client queried BAR{bar_no} region: (empty)");
                                    empty_region
                                },
                                |bar_info| {
                                    debug!("Client queried BAR{bar_no} region: {:?}", bar_info);
                                    vfio_region_info {
                                        argsz: size_of::<vfio_region_info>() as u32,
                                        index: i,
                                        size: bar_info.size.into(),
                                        flags: VFIO_REGION_INFO_FLAG_READ
                                            | VFIO_REGION_INFO_FLAG_WRITE,
                                        ..Default::default()
                                    }
                                },
                            )
                    }

                    unknown => {
                        debug!("Client queried unknown VFIO region: {unknown}");
                        empty_region
                    }
                }
            })
            .collect()
    }

    /// Return a list of IRQs for [`vfio_user::Server::new`].
    pub fn irqs(&self) -> Vec<IrqInfo> {
        (0..VFIO_PCI_NUM_IRQS)
            .map(|index| IrqInfo {
                index,
                count: match index {
                    VFIO_PCI_MSIX_IRQ_INDEX => 1,
                    _ => 0,
                },
                flags: 0,
            })
            .collect()
    }
}

impl ServerBackend for XhciBackend {
    fn region_read(
        &mut self,
        region: u32,
        offset: u64,
        data: &mut [u8],
    ) -> Result<(), std::io::Error> {
        trace!("read  region {region} offset {offset:#x}+{}", data.len());

        let value: u64 = match region {
            VFIO_PCI_CONFIG_REGION_INDEX => self.controller.read_cfg(Request::new(
                offset,
                RequestSize::try_from(data.len() as u64).expect("should use valid request size"),
            )),

            0 => self.controller.read_io(
                0,
                Request::new(
                    offset,
                    RequestSize::try_from(data.len() as u64)
                        .expect("should use valid request size"),
                ),
            ),

            _ => !0u64,
        };

        data.copy_from_slice(&value.to_le_bytes()[0..data.len()]);

        Ok(())
    }

    fn region_write(
        &mut self,
        region: u32,
        offset: u64,
        data: &[u8],
    ) -> Result<(), std::io::Error> {
        trace!(
            "write region {region} offset {offset:#x}+{} val {:?}",
            data.len(),
            data
        );

        match region {
            VFIO_PCI_CONFIG_REGION_INDEX => self.controller.write_cfg(
                Request::new(
                    offset,
                    RequestSize::try_from(data.len() as u64)
                        .expect("should use valid request size"),
                ),
                match data.len() {
                    1 => data[0].into(),
                    2 => {
                        // SAFETY:: this branch is only taken if data.len() == 2
                        let val: [u8; 2] = data.try_into().unwrap();
                        u16::from_le_bytes(val).into()
                    }

                    4 => {
                        // SAFETY:: this branch is only taken if data.len() == 4
                        let val: [u8; 4] = data.try_into().unwrap();
                        u32::from_le_bytes(val).into()
                    }
                    _ => todo!(),
                },
            ),

            0 => self.controller.write_io(
                0,
                Request::new(
                    offset,
                    RequestSize::try_from(data.len() as u64)
                        .expect("should use valid request size"),
                ),
                match data.len() {
                    1 => data[0].into(),
                    2 => {
                        // SAFETY:: this branch is only taken if data.len() == 2
                        let val: [u8; 2] = data.try_into().unwrap();
                        u16::from_le_bytes(val).into()
                    }

                    4 => {
                        // SAFETY:: this branch is only taken if data.len() == 4
                        let val: [u8; 4] = data.try_into().unwrap();
                        u32::from_le_bytes(val).into()
                    }
                    _ => todo!(),
                },
            ),
            _ => todo!(),
        }

        Ok(())
    }

    fn dma_map(
        &mut self,
        flags: vfio_user::DmaMapFlags,
        offset: u64,
        address: u64,
        size: u64,
        fd: Option<File>,
    ) -> Result<(), std::io::Error> {
        info!("dma_map flags = {flags:?} offset = {offset} address = {address} size = {size} fd = {fd:?}");

        if let Some(fd) = fd {
            let mseg = MemorySegment::new_from_fd(
                &fd,
                offset,
                size,
                // We want to know when this happens, so bail out eagerly here.
                flags.try_into().expect("Failed to convert flags"),
            )?;

            // Guest provided invalid memory region setup - no reasonable recovery possible
            self.dma_bus.add(address, Arc::new(mseg)).unwrap();
        } else {
            todo!("Memory region without file descriptor");
        }

        Ok(())
    }

    fn dma_unmap(
        &mut self,
        _flags: vfio_user::DmaUnmapFlags,
        _address: u64,
        _size: u64,
    ) -> Result<(), std::io::Error> {
        todo!()
    }

    fn reset(&mut self) -> Result<(), std::io::Error> {
        todo!()
    }

    fn set_irqs(
        &mut self,
        index: u32,
        flags: u32,
        start: u32,
        count: u32,
        fds: Vec<File>,
    ) -> Result<(), std::io::Error> {
        debug!(
            "set IRQs: {index} flags: {flags:#x} start: {start:#x} count: {count:#x} #fds: {}",
            fds.len()
        );
        assert_eq!(
            index, VFIO_PCI_MSIX_IRQ_INDEX,
            "Only MSI-X interrupts are supported"
        );
        assert!(count <= 1, "Only a single interrupt is supported");

        let irqs: Vec<Arc<InterruptEventFd>> = fds
            .into_iter()
            .map(|file| {
                Arc::new(InterruptEventFd {
                    fd: Mutex::new(file),
                })
            })
            .collect();

        // The solution offered by clippy produces a type error.
        #[allow(clippy::option_if_let_else)]
        let irq: Arc<dyn InterruptLine> = match irqs.first() {
            Some(eventfd) => eventfd.clone(),
            _ => Arc::new(DummyInterruptLine::default()),
        };

        self.controller.lock().unwrap().connect_irq(irq);

        Ok(())
    }
}
