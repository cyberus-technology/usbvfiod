//! Emulation of a USB3 Host (XHCI) controller.
//!
//! The specification is available
//! [here](https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf).

use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

use crate::device::{
    bus::{BusDeviceRef, Request, RequestSize, SingleThreadedBusDevice},
    interrupt_line::{DummyInterruptLine, InterruptLine},
    pci::{
        config_space::{ConfigSpace, ConfigSpaceBuilder},
        constants::xhci::{
            capability, offset,
            operational::{crcr, portsc},
            runtime, MAX_INTRS, MAX_SLOTS, OP_BASE, RUN_BASE,
        },
        rings::TransferRing,
        traits::PciDevice,
        trb::{CompletionCode, EventTrb, TransferTrb},
    },
};

use super::{
    config_space::BarInfo,
    rings::{CommandRing, EventRing},
    trb::CommandTrb,
};

/// The emulation of a XHCI controller.
#[derive(Debug, Clone)]
pub struct XhciController {
    /// hacky variable to answer to transfer ring control requests
    control_request_counter: usize,

    /// hacky variable to give out slot ids
    slot_id_counter: u8,

    /// A reference to the VM memory to perform DMA on.
    #[allow(unused)]
    dma_bus: BusDeviceRef,

    /// The PCI Configuration Space of the controller.
    config_space: ConfigSpace,

    /// The current Run/Stop status of the controller.
    running: bool,

    /// The current Run/Stop status of the command ring.
    command_ring_running: bool,

    /// Internal Command Ring position.
    command_ring_dequeue_pointer: u64,

    /// The Command Ring.
    command_ring: CommandRing,

    /// The Event Ring of the single Interrupt Register Set.
    event_ring: EventRing,

    /// Internal Consumer Cycle State for the next TRB fetch.
    consumer_cycle_state: bool,

    /// Configured device slots.
    slots: Vec<()>,

    /// Device Context Array
    /// TODO: currently just the raw pointer configured by the OS
    device_contexts: Vec<u64>,

    /// Interrupt management register
    interrupt_management: u64,

    /// The minimum interval in 250ns increments between interrupts.
    interrupt_moderation_interval: u64,

    /// The interrupt line triggered to signal device events.
    interrupt_line: Arc<dyn InterruptLine>,

    portsc: u64,
}

impl XhciController {
    /// Create a new XHCI controller with default settings.
    ///
    /// `dma_bus` is the device on which we will perform DMA
    /// operations. This is typically VM guest memory.
    #[must_use]
    pub fn new(dma_bus: BusDeviceRef) -> Self {
        use crate::device::pci::constants::config_space::*;

        Self {
            control_request_counter: 0,
            slot_id_counter: 1,
            dma_bus,
            config_space: ConfigSpaceBuilder::new(vendor::REDHAT, device::REDHAT_XHCI)
                .class(class::SERIAL, subclass::SERIAL_USB, progif::USB_XHCI)
                // TODO Should be a 64-bit BAR.
                .mem32_nonprefetchable_bar(0, 4 * 0x1000)
                .mem32_nonprefetchable_bar(3, 2 * 0x1000)
                .msix_capability(MAX_INTRS.try_into().unwrap(), 3, 0, 3, 0x1000)
                .config_space(),
            running: false,
            command_ring_running: false,
            command_ring_dequeue_pointer: 0,
            command_ring: CommandRing::default(),
            consumer_cycle_state: false,
            event_ring: EventRing::default(),
            slots: vec![],
            device_contexts: vec![],
            interrupt_management: 0,
            interrupt_moderation_interval: runtime::IMOD_DEFAULT,
            interrupt_line: Arc::new(DummyInterruptLine::default()),
            portsc: 0x00260203,
        }
    }

    /// Configure the interrupt line for the controller.
    ///
    /// The [`XhciController`] uses this to issue interrupts for events.
    pub fn connect_irq(&mut self, irq: Arc<dyn InterruptLine>) {
        self.interrupt_line = irq.clone();
    }

    /// Obtain the current host controller status as defined for the `USBSTS` register.
    #[must_use]
    pub fn status(&self) -> u64 {
        debug!("usbsts read");
        (!u64::from(self.running) & 0x1u64) | 0x8 | 0x10
    }

    /// Obtain the current command ring status as defined for reading the `CRCR` register.
    #[must_use]
    pub fn command_ring_status(&self) -> u64 {
        // All fields except CRR (command ring running) read as zero.
        (u64::from(self.command_ring_running) << 3) & 0b100
    }

    /// Obtain the current host controller configuration as defined for the `CONFIG` register.
    #[must_use]
    pub fn config(&self) -> u64 {
        u64::try_from(self.slots.len()).unwrap() & 0x8u64
    }

    /// Enable device slots.
    pub fn enable_slots(&mut self, count: u64) {
        assert!(count <= MAX_SLOTS);

        self.slots = (0..count).map(|_| ()).collect();

        debug!("enabled {} device slots", self.slots.len());
    }

    /// Configure the device context array from the array base pointer.
    pub fn configure_device_contexts(&mut self, device_context_base_array_ptr: u64) {
        debug!(
            "configuring device contexts from pointer {:#x}",
            device_context_base_array_ptr
        );
        self.device_contexts.clear();
        self.device_contexts.push(device_context_base_array_ptr);
    }

    /// Start/Stop controller operation
    ///
    /// This is called for writes of the `USBCMD` register.
    pub fn run(&mut self, usbcmd: u64) {
        self.running = usbcmd & 0x1 == 0x1;
        if self.running {
            debug!("controller started with cmd {usbcmd:#x}");

            // Send a port status change event, which signals the driver to
            // inspect the PORTSC status register.
            //let trb = EventTrb::new_port_status_change_event_trb(0);
            //self.event_ring.enqueue(&trb, self.dma_bus.clone());

            // XXX: This is just a test to see if we can generate interrupts.
            // This will be removed once we generate interrupts in the right
            // place, (e.g. generate a Port Connect Status Event) and test it.
            self.interrupt_line.interrupt();
            debug!("signalled a bogus interrupt");
            // check command ring---it should be empty at this point
            //let cmd_trb = self.command_ring.next_command_trb(self.dma_bus.clone());
            //debug!("checking command ring: {:?}", cmd_trb);
        } else {
            debug!("controller stopped with cmd {usbcmd:#x}");
        }
    }

    /// Handle Command Ring Control Register (CRCR) updates.
    pub fn update_command_ring(&mut self, value: u64) {
        if self.command_ring_running {
            match value {
                abort if abort & crcr::CA != 0 => todo!(),
                stop if stop & crcr::CS != 0 => todo!(),
                ignored => {
                    warn!(
                        "received useless write to CRCR while running {:#x}",
                        ignored
                    )
                }
            }
        } else {
            let dequeue_ptr = value & crcr::DEQUEUE_POINTER_MASK;
            if self.command_ring_dequeue_pointer != dequeue_ptr {
                debug!(
                    "updating command ring dequeue ptr from {:#x} to {:#x}",
                    self.command_ring_dequeue_pointer, dequeue_ptr
                );
                self.command_ring_dequeue_pointer = dequeue_ptr;
            }
            // Update internal consumer cycle state for next TRB fetch.
            self.consumer_cycle_state = value & crcr::RCS != 0;
            debug!(
                "configuring command ring with dp={:#x} and cs={}",
                dequeue_ptr, self.consumer_cycle_state as u8
            );
            self.command_ring
                .configure(dequeue_ptr, self.consumer_cycle_state);
        }
    }

    /// enqueue port status change event TRB on event ring and signal an interrupt
    pub fn send_port_status_change(&mut self) {
        let port_status_change_event = Self::create_port_status_change_event_trb(1);
        // TODO enqueue event
        self.interrupt_line.interrupt();
        debug!("signalled an interrupt");
    }

    fn create_port_status_change_event_trb(port_id: u8) -> [u8; 16] {
        let port_status_change_event_id = 34;
        let completion_code_success = 1;
        let mut trb = [0; 16];

        trb[3] = port_id;
        trb[11] = completion_code_success;
        trb[13] = port_status_change_event_id << 2;

        trb
    }

    fn create_command_completion_event_trb(command_trb_pointer: u64) -> [u8; 16] {
        let completion_code_success = 1;
        let _command_completion_parameter = 0;
        let _slot_id = 0;
        let _vf_id = 0;
        let port_command_completion_event_id = 33;
        let mut trb = [0; 16];
        let slot_id = 1;

        trb[0..8].copy_from_slice(&command_trb_pointer.to_le_bytes());
        trb[11] = completion_code_success;
        trb[13] = port_command_completion_event_id << 2;
        trb[15] = slot_id;

        trb
    }

    fn doorbell(&mut self) {
        debug!("Ding Dong!");
        // check command available
        let next = self.command_ring.next_command_trb(self.dma_bus.clone());
        if let Some((address, Ok(cmd_trb))) = next {
            self.handle_command(address, cmd_trb);
        } else {
            debug!(
                "Doorbell was rang, but no (valid) command found on the command ring ({:?})",
                next
            );
        }
    }

    fn doorbell_slot1(&mut self, value: u64) {
        debug!("Ding Dong from Slot 1 with value {}", value);

        // look up dequeue_ptr of transfer_ring
        let dcbaap = *self
            .device_contexts
            .get(0)
            .expect("the dcbaap should be in the device_contexts vec");

        let slot_id = 1;
        let device_context_pointer = self
            .dma_bus
            .read(Request::new(dcbaap + slot_id * 8, RequestSize::Size8));

        let bytes = self.dma_bus.read(Request::new(
            device_context_pointer + 32 + 8,
            RequestSize::Size8,
        ));
        let cycle_bit = bytes & 0x1 != 0;
        let dequeue_pointer = bytes & !0xf;

        // instantiate transfer ring
        // we shouldn't do this every time, not sure how to organize best
        let mut transfer_ring = TransferRing::default();
        transfer_ring.configure(dequeue_pointer, cycle_bit);

        if self.control_request_counter == 0 {
            debug!("first doorbell[1] ring -- device descriptor for packet size");
            // read three TRBs from ring
            let setup = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed setup stage is available");
            debug!("from transfer ring: {:?}", setup);
            let data = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed data stage is available");
            debug!("from transfer ring: {:?}", data);
            let status = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed status stage is available");
            debug!("from transfer ring: {:?}", status);
            let further = transfer_ring.next_transfer_trb(self.dma_bus.clone());
            debug!("from transfer ring: {:?}", further);

            // update dequeue_pointer
            self.dma_bus.write(
                Request::new(device_context_pointer + 32 + 8, RequestSize::Size8),
                bytes + 3 * 16,
            );

            // answer device descriptor request
            let data_pointer = match data.1.unwrap() {
                TransferTrb::DataStage(data) => data.data_pointer,
                _ => panic!("expected data stage"),
            };
            debug!("writing descriptor to data pointer {:#x}", data_pointer);
            let device_descriptor_first_8_bytes = [
                0x12, // bLength
                0x01, // bDescriptorType (Device)
                0x00, 0x03, // bcdUSB (USB 3.0)
                0x00, // bDeviceClass (defined at interface level)
                0x00, // bDeviceSubClass
                0x00, // bDeviceProtocol
                0x09, // bMaxPacketSize0 (2^9 = 512 bytes)
            ];
            self.dma_bus
                .write_bulk(data_pointer, &device_descriptor_first_8_bytes);

            // send transfer event
            let trb =
                EventTrb::new_transfer_event_trb(status.0, 0, CompletionCode::Success, false, 1, 1);
            debug!("transfer event: {:?}", trb.to_bytes(false));
            self.event_ring.enqueue(&trb, self.dma_bus.clone());
            self.interrupt_line.interrupt();
            debug!("enqueued transfer event trb and triggered interrupt");
        } else if self.control_request_counter == 1 {
            debug!("second doorbell[1] ring -- ??");

            // read two TRBs from ring
            let setup = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed setup stage is available");
            debug!("from transfer ring: {:?}", setup);
            let status = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed status stage is available");
            debug!("from transfer ring: {:?}", status);
            let further = transfer_ring.next_transfer_trb(self.dma_bus.clone());
            debug!("from transfer ring: {:?}", further);

            // update dequeue_pointer
            self.dma_bus.write(
                Request::new(device_context_pointer + 32 + 8, RequestSize::Size8),
                bytes + 2 * 16,
            );

            // send transfer event
            let trb =
                EventTrb::new_transfer_event_trb(status.0, 0, CompletionCode::Success, false, 1, 1);
            debug!("transfer event: {:?}", trb.to_bytes(false));
            self.event_ring.enqueue(&trb, self.dma_bus.clone());
            self.interrupt_line.interrupt();
            debug!("enqueued transfer event trb and triggered interrupt");
        } else if self.control_request_counter == 2 {
            debug!("third doorbell[1] ring -- full device descriptor");
            // read three TRBs from ring
            let setup = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed setup stage is available");
            debug!("from transfer ring: {:?}", setup);
            let data = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed data stage is available");
            debug!("from transfer ring: {:?}", data);
            let status = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed status stage is available");
            debug!("from transfer ring: {:?}", status);
            let further = transfer_ring.next_transfer_trb(self.dma_bus.clone());
            debug!("from transfer ring: {:?}", further);

            // update dequeue_pointer
            self.dma_bus.write(
                Request::new(device_context_pointer + 32 + 8, RequestSize::Size8),
                bytes + 3 * 16,
            );

            // answer device descriptor request
            let data_pointer = match data.1.unwrap() {
                TransferTrb::DataStage(data) => data.data_pointer,
                _ => panic!("expected data stage"),
            };
            debug!("writing descriptor to data pointer {:#x}", data_pointer);
            let device_descriptor = [
                0x12, // bLength
                0x01, // bDescriptorType (Device)
                0x00, 0x03, // bcdUSB (USB 3.0)
                0x00, // bDeviceClass (defined at interface level)
                0x00, // bDeviceSubClass
                0x00, // bDeviceProtocol
                0x09, // bMaxPacketSize0 (2^9 = 512 bytes)
                0x34, 0x12, // idVendor (0x1234)
                0x78, 0x56, // idProduct (0x5678)
                0x00, 0x01, // bcdDevice (1.00)
                0x01, // iManufacturer
                0x02, // iProduct
                0x03, // iSerialNumber
                0x01, // bNumConfigurations
            ];
            self.dma_bus.write_bulk(data_pointer, &device_descriptor);

            // send transfer event
            let trb =
                EventTrb::new_transfer_event_trb(status.0, 0, CompletionCode::Success, false, 1, 1);
            debug!("transfer event: {:?}", trb.to_bytes(false));
            self.event_ring.enqueue(&trb, self.dma_bus.clone());
            self.interrupt_line.interrupt();
            debug!("enqueued transfer event trb and triggered interrupt");
        } else if self.control_request_counter == 3 || self.control_request_counter == 4 {
            debug!("fourth/fifth doorbell[1] ring -- BOS descriptor");
            // read three TRBs from ring
            let setup = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed setup stage is available");
            debug!("from transfer ring: {:?}", setup);
            let data = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed data stage is available");
            debug!("from transfer ring: {:?}", data);
            let status = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed status stage is available");
            debug!("from transfer ring: {:?}", status);
            let further = transfer_ring.next_transfer_trb(self.dma_bus.clone());
            debug!("from transfer ring: {:?}", further);

            // update dequeue_pointer
            self.dma_bus.write(
                Request::new(device_context_pointer + 32 + 8, RequestSize::Size8),
                bytes + 3 * 16,
            );

            // answer BOS descriptor request
            let data_pointer = match data.1.unwrap() {
                TransferTrb::DataStage(data) => data.data_pointer,
                _ => panic!("expected data stage"),
            };
            let bos_descriptor: [u8; 5] = [
                5,    // bLength
                0x0F, // bDescriptorType (BOS)
                5, 0, // wTotalLength (5 bytes, little endian)
                0, // bNumDeviceCaps (no device capabilities)
            ];
            debug!("writing descriptor to data pointer {:#x}", data_pointer);
            self.dma_bus.write_bulk(data_pointer, &bos_descriptor);

            // send transfer event
            let trb =
                EventTrb::new_transfer_event_trb(status.0, 0, CompletionCode::Success, false, 1, 1);
            debug!("transfer event: {:?}", trb.to_bytes(false));
            self.event_ring.enqueue(&trb, self.dma_bus.clone());
            self.interrupt_line.interrupt();
            debug!("enqueued transfer event trb and triggered interrupt");
        } else if self.control_request_counter == 5 {
            debug!("sixth doorbell[1] ring -- header of configuration descriptor");
            // read three TRBs from ring
            let setup = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed setup stage is available");
            debug!("from transfer ring: {:?}", setup);
            let data = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed data stage is available");
            debug!("from transfer ring: {:?}", data);
            let status = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed status stage is available");
            debug!("from transfer ring: {:?}", status);
            let further = transfer_ring.next_transfer_trb(self.dma_bus.clone());
            debug!("from transfer ring: {:?}", further);

            // update dequeue_pointer
            self.dma_bus.write(
                Request::new(device_context_pointer + 32 + 8, RequestSize::Size8),
                bytes + 3 * 16,
            );

            // answer configuration descriptor request
            let data_pointer = match data.1.unwrap() {
                TransferTrb::DataStage(data) => data.data_pointer,
                _ => panic!("expected data stage"),
            };
            let configuration_descriptor = [
                // Configuration Descriptor (9 bytes)
                0x09, // bLength
                0x02, // bDescriptorType (Configuration)
                0x20, 0x00, // wTotalLength = 32 bytes (config + interface)
                0x01, // bNumInterfaces
                0x01, // bConfigurationValue
                0x00, // iConfiguration (string index)
                0x80, // bmAttributes (bus powered, no remote wakeup)
                0x32, // bMaxPower (100 mA)
            ];
            debug!("writing descriptor to data pointer {:#x}", data_pointer);
            self.dma_bus
                .write_bulk(data_pointer, &configuration_descriptor);

            // send transfer event
            let trb =
                EventTrb::new_transfer_event_trb(status.0, 0, CompletionCode::Success, false, 1, 1);
            debug!("transfer event: {:?}", trb.to_bytes(false));
            self.event_ring.enqueue(&trb, self.dma_bus.clone());
            self.interrupt_line.interrupt();
            debug!("enqueued transfer event trb and triggered interrupt");
        } else if self.control_request_counter == 6 {
            debug!("seventh doorbell[1] ring -- full configuration descriptor");
            // read three TRBs from ring
            let setup = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed setup stage is available");
            debug!("from transfer ring: {:?}", setup);
            let data = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed data stage is available");
            debug!("from transfer ring: {:?}", data);
            let status = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed status stage is available");
            debug!("from transfer ring: {:?}", status);
            let further = transfer_ring.next_transfer_trb(self.dma_bus.clone());
            debug!("from transfer ring: {:?}", further);

            // update dequeue_pointer
            self.dma_bus.write(
                Request::new(device_context_pointer + 32 + 8, RequestSize::Size8),
                bytes + 3 * 16,
            );

            // answer configuration descriptor request
            let data_pointer = match data.1.unwrap() {
                TransferTrb::DataStage(data) => data.data_pointer,
                _ => panic!("expected data stage"),
            };
            let configuration_descriptor: [u8; 32] = [
                // Configuration Descriptor (9 bytes)
                0x09, // bLength
                0x02, // bDescriptorType (Configuration)
                0x20, 0x00, // wTotalLength = 32 bytes (config + interface)
                0x01, // bNumInterfaces
                0x01, // bConfigurationValue
                0x00, // iConfiguration (string index)
                0x80, // bmAttributes (bus powered, no remote wakeup)
                0x32, // bMaxPower (100 mA)
                // Interface Descriptor (9 bytes)
                0x09, // bLength
                0x04, // bDescriptorType (Interface)
                0x00, // bInterfaceNumber
                0x00, // bAlternateSetting
                0x00, // bNumEndpoints (0 additional endpoints; only EP0)
                0x00, // bInterfaceClass (defined at device level or vendor-specific)
                0x00, // bInterfaceSubClass
                0x00, // bInterfaceProtocol
                0x00, // iInterface (string index)
                // Padding (14 bytes) to fill wTotalLength (optional, or you can adjust length)
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ];
            debug!("writing descriptor to data pointer {:#x}", data_pointer);
            self.dma_bus
                .write_bulk(data_pointer, &configuration_descriptor);

            // send transfer event
            let trb =
                EventTrb::new_transfer_event_trb(status.0, 0, CompletionCode::Success, false, 1, 1);
            debug!("transfer event: {:?}", trb.to_bytes(false));
            self.event_ring.enqueue(&trb, self.dma_bus.clone());
            self.interrupt_line.interrupt();
            debug!("enqueued transfer event trb and triggered interrupt");
        } else if self.control_request_counter == 7 {
            debug!("eight doorbell[1] ring -- language descriptor");
            // read three TRBs from ring
            let setup = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed setup stage is available");
            debug!("from transfer ring: {:?}", setup);
            let data = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed data stage is available");
            debug!("from transfer ring: {:?}", data);
            let status = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed status stage is available");
            debug!("from transfer ring: {:?}", status);
            let further = transfer_ring.next_transfer_trb(self.dma_bus.clone());
            debug!("from transfer ring: {:?}", further);

            // update dequeue_pointer
            self.dma_bus.write(
                Request::new(device_context_pointer + 32 + 8, RequestSize::Size8),
                bytes + 3 * 16,
            );

            // answer language descriptor request
            let data_pointer = match data.1.unwrap() {
                TransferTrb::DataStage(data) => data.data_pointer,
                _ => panic!("expected data stage"),
            };
            let lang_id_descriptor = [
                0x04, // bLength (4 bytes)
                0x03, // bDescriptorType (STRING)
                0x09, 0x04, // LANGID (0x0409 = English - United States)
            ];
            debug!("writing descriptor to data pointer {:#x}", data_pointer);
            self.dma_bus.write_bulk(data_pointer, &lang_id_descriptor);

            // send transfer event
            let trb =
                EventTrb::new_transfer_event_trb(status.0, 0, CompletionCode::Success, false, 1, 1);
            debug!("transfer event: {:?}", trb.to_bytes(false));
            self.event_ring.enqueue(&trb, self.dma_bus.clone());
            self.interrupt_line.interrupt();
            debug!("enqueued transfer event trb and triggered interrupt");
        } else if self.control_request_counter == 8 {
            debug!("ninth doorbell[1] ring -- string descriptor");
            // read three TRBs from ring
            let setup = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed setup stage is available");
            debug!("from transfer ring: {:?}", setup);
            let data = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed data stage is available");
            debug!("from transfer ring: {:?}", data);
            let status = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed status stage is available");
            debug!("from transfer ring: {:?}", status);
            let further = transfer_ring.next_transfer_trb(self.dma_bus.clone());
            debug!("from transfer ring: {:?}", further);

            // update dequeue_pointer
            self.dma_bus.write(
                Request::new(device_context_pointer + 32 + 8, RequestSize::Size8),
                bytes + 3 * 16,
            );

            // answer string descriptor request
            let data_pointer = match data.1.unwrap() {
                TransferTrb::DataStage(data) => data.data_pointer,
                _ => panic!("expected data stage"),
            };
            let string_descriptor_2: [u8; 22] = [
                22,   // bLength
                0x03, // bDescriptorType (STRING)
                b'U', 0x00, b'S', 0x00, b'B', 0x00, b' ', 0x00, b'D', 0x00, b'e', 0x00, b'v', 0x00,
                b'i', 0x00, b'c', 0x00, b'e', 0x00,
            ];
            debug!("writing descriptor to data pointer {:#x}", data_pointer);
            self.dma_bus.write_bulk(data_pointer, &string_descriptor_2);

            // send transfer event
            let trb =
                EventTrb::new_transfer_event_trb(status.0, 0, CompletionCode::Success, false, 1, 1);
            debug!("transfer event: {:?}", trb.to_bytes(false));
            self.event_ring.enqueue(&trb, self.dma_bus.clone());
            self.interrupt_line.interrupt();
            debug!("enqueued transfer event trb and triggered interrupt");
        } else if self.control_request_counter == 9 {
            debug!("10th doorbell[1] ring -- string descriptor manufacturer");
            // read three TRBs from ring
            let setup = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed setup stage is available");
            debug!("from transfer ring: {:?}", setup);
            let data = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed data stage is available");
            debug!("from transfer ring: {:?}", data);
            let status = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed status stage is available");
            debug!("from transfer ring: {:?}", status);
            let further = transfer_ring.next_transfer_trb(self.dma_bus.clone());
            debug!("from transfer ring: {:?}", further);

            // update dequeue_pointer
            self.dma_bus.write(
                Request::new(device_context_pointer + 32 + 8, RequestSize::Size8),
                bytes + 3 * 16,
            );

            // answer manufacturer string descriptor request
            let data_pointer = match data.1.unwrap() {
                TransferTrb::DataStage(data) => data.data_pointer,
                _ => panic!("expected data stage"),
            };
            let manufacturer_string = [
                16,   // bLength (2 + 7 chars * 2)
                0x03, // bDescriptorType (STRING)
                b'C', 0x00, b'Y', 0x00, b'B', 0x00, b'E', 0x00, b'R', 0x00, b'U', 0x00, b'S', 0x00,
            ];
            debug!("writing descriptor to data pointer {:#x}", data_pointer);
            self.dma_bus.write_bulk(data_pointer, &manufacturer_string);

            // send transfer event
            let trb =
                EventTrb::new_transfer_event_trb(status.0, 0, CompletionCode::Success, false, 1, 1);
            debug!("transfer event: {:?}", trb.to_bytes(false));
            self.event_ring.enqueue(&trb, self.dma_bus.clone());
            self.interrupt_line.interrupt();
            debug!("enqueued transfer event trb and triggered interrupt");
        } else if self.control_request_counter == 10 {
            debug!("11th doorbell[1] ring -- string descriptor serialnumber");
            // read three TRBs from ring
            let setup = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed setup stage is available");
            debug!("from transfer ring: {:?}", setup);
            let data = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed data stage is available");
            debug!("from transfer ring: {:?}", data);
            let status = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed status stage is available");
            debug!("from transfer ring: {:?}", status);
            let further = transfer_ring.next_transfer_trb(self.dma_bus.clone());
            debug!("from transfer ring: {:?}", further);

            // update dequeue_pointer
            self.dma_bus.write(
                Request::new(device_context_pointer + 32 + 8, RequestSize::Size8),
                bytes + 3 * 16,
            );

            // answer serial number descriptor request
            let data_pointer = match data.1.unwrap() {
                TransferTrb::DataStage(data) => data.data_pointer,
                _ => panic!("expected data stage"),
            };
            debug!("writing descriptor to data pointer {:#x}", data_pointer);
            let serial_number_string: [u8; 14] = [
                14,   // bLength (2 + 6 chars * 2 bytes)
                0x03, // bDescriptorType (STRING)
                b'S', 0x00, b'N', 0x00, b'1', 0x00, b'2', 0x00, b'3', 0x00, b'4', 0x00,
            ];
            self.dma_bus.write_bulk(data_pointer, &serial_number_string);

            // send transfer event
            let trb =
                EventTrb::new_transfer_event_trb(status.0, 0, CompletionCode::Success, false, 1, 1);
            debug!("transfer event: {:?}", trb.to_bytes(false));
            self.event_ring.enqueue(&trb, self.dma_bus.clone());
            self.interrupt_line.interrupt();
            debug!("enqueued transfer event trb and triggered interrupt");
        } else if self.control_request_counter == 11 {
            debug!("12th doorbell[1] ring -- set configuration");
            // read three TRBs from ring
            let setup = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed setup stage is available");
            debug!("from transfer ring: {:?}", setup);
            let status = transfer_ring
                .next_transfer_trb(self.dma_bus.clone())
                .expect("assumed status stage is available");
            debug!("from transfer ring: {:?}", status);
            let further = transfer_ring.next_transfer_trb(self.dma_bus.clone());
            debug!("from transfer ring: {:?}", further);

            // update dequeue_pointer
            self.dma_bus.write(
                Request::new(device_context_pointer + 32 + 8, RequestSize::Size8),
                bytes + 3 * 16,
            );

            // send transfer event
            let trb =
                EventTrb::new_transfer_event_trb(status.0, 0, CompletionCode::Success, false, 1, 1);
            debug!("transfer event: {:?}", trb.to_bytes(false));
            self.event_ring.enqueue(&trb, self.dma_bus.clone());
            self.interrupt_line.interrupt();
            debug!("enqueued transfer event trb and triggered interrupt");
        } else {
            // read all available TRBs
            debug!("too far! don't know what this transfer request is");
            loop {
                if let Some(trb) = transfer_ring.next_transfer_trb(self.dma_bus.clone()) {
                    debug!("found trb on transfer_ring: {:?}", trb);
                } else {
                    debug!("no more TRBs on transfer ring found");
                    panic!();
                }
            }
        }
        self.control_request_counter += 1;
    }

    fn handle_command(&mut self, address: u64, cmd: CommandTrb) {
        debug!("handling command {:?} at {:#x}", cmd, address);
        match cmd {
            CommandTrb::EnableSlotCommand => {
                let slot_id = self.slot_id_counter;
                self.slot_id_counter += 1;
                let completion_event = EventTrb::new_command_completion_event_trb(
                    address,
                    0,
                    CompletionCode::Success,
                    slot_id,
                );
                self.event_ring
                    .enqueue(&completion_event, self.dma_bus.clone());
                self.interrupt_line.interrupt();
                debug!("send command completion event for EnableSlotCommand");
            }
            CommandTrb::DisableSlotCommand => todo!(),
            CommandTrb::AddressDeviceCommand(data) => {
                // look up pointer to device context. The driver wrote it to
                // the DCBAA beforehand (after the Enable Slot Command).
                // We need to copy data from the input context there.
                let dcbaap = *self
                    .device_contexts
                    .get(0)
                    .expect("the dcbaap should be in the device_contexts vec");

                debug!("DCBAAP is at {:#x}", dcbaap);

                let device_context_pointer = self.dma_bus.read(Request::new(
                    dcbaap + data.slot_id as u64 * 8,
                    RequestSize::Size8,
                ));

                debug!(
                    "looked up pointer to device context for slot id {}: {:#x}",
                    data.slot_id, device_context_pointer
                );

                // retrieve and inspect input context
                // index 0: input control context
                // index 1: slot context
                // index 2: endpoint context for ep0
                // index 3: endpoint context for ep1 (unused, zeroed)
                let mut input_context = [0; 1056];
                self.dma_bus
                    .read_bulk(data.input_context_pointer, &mut input_context);
                for i in 0..4 {
                    let context_offset = i * 32;
                    debug!("now printing context info at index {}", i);
                    for j in 0..4 {
                        debug!(
                            "{:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x} {:#x}",
                            input_context[context_offset + j * 8 + 0],
                            input_context[context_offset + j * 8 + 1],
                            input_context[context_offset + j * 8 + 2],
                            input_context[context_offset + j * 8 + 3],
                            input_context[context_offset + j * 8 + 4],
                            input_context[context_offset + j * 8 + 5],
                            input_context[context_offset + j * 8 + 6],
                            input_context[context_offset + j * 8 + 7]
                        );
                    }
                }

                // copy slot context and ep0 context over to output device context
                self.dma_bus
                    .write_bulk(device_context_pointer + 0, &input_context[32..64]);
                self.dma_bus
                    .write_bulk(device_context_pointer + 32, &input_context[64..96]);

                // set slot state to addressed
                let slot_state_addressed = 2;
                self.dma_bus.write_bulk(
                    device_context_pointer + 0 + 15,
                    &[slot_state_addressed << 3; 1],
                );

                // set endpoint state to enabled
                let ep_state_running = 1;
                self.dma_bus
                    .write_bulk(device_context_pointer + 32 + 0, &[ep_state_running]);

                // send completion event to driver
                let completion_event = EventTrb::new_command_completion_event_trb(
                    address,
                    0,
                    CompletionCode::Success,
                    data.slot_id,
                );
                self.event_ring
                    .enqueue(&completion_event, self.dma_bus.clone());
                self.interrupt_line.interrupt();
                debug!("send command completion event for AddressDeviceCommand");
            }
            CommandTrb::ConfigureEndpointCommand => todo!(),
            CommandTrb::EvaluateContextCommand => todo!(),
            CommandTrb::ResetEndpointCommand => todo!(),
            CommandTrb::StopEndpointCommand => todo!(),
            CommandTrb::SetTrDequeuePointerCommand => todo!(),
            CommandTrb::ResetDeviceCommand => todo!(),
            CommandTrb::ForceHeaderCommand => todo!(),
            CommandTrb::NoOpCommand => todo!(),
            CommandTrb::Link(_) => unreachable!(),
        }
    }
}

impl PciDevice for Mutex<XhciController> {
    fn write_cfg(&self, req: Request, value: u64) {
        self.lock().unwrap().config_space.write(req, value);
    }

    fn read_cfg(&self, req: Request) -> u64 {
        self.lock().unwrap().config_space.read(req)
    }

    fn write_io(&self, region: u32, req: Request, value: u64) {
        // The XHCI Controller has a single MMIO BAR.
        assert_eq!(region, 0);

        match req.addr {
            // xHC Operational Registers
            offset::USBCMD => self.lock().unwrap().run(value),
            offset::DNCTL => assert_eq!(value, 2, "debug notifications not supported"),
            offset::CRCR => self.lock().unwrap().update_command_ring(value),
            offset::CRCR_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::DCBAAP => self.lock().unwrap().configure_device_contexts(value),
            offset::DCBAAP_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::CONFIG => self.lock().unwrap().enable_slots(value),

            offset::PORTSC => {
                debug!("portsc write: {:#x}", value);
                if (value & 0x00020000 != 0) {
                    debug!("found 1-to-clear on bit 17, unsetting bit");
                    self.lock().unwrap().portsc &= !0x00020000;
                }
                if (value & 0x00040000 != 0) {
                    debug!("found 1-to-clear on bit 18, unsetting bit");
                    self.lock().unwrap().portsc &= !0x00040000;
                }
                if (value & 0x00200000 != 0) {
                    debug!("found 1-to-clear on bit 21, unsetting bit");
                    self.lock().unwrap().portsc &= !0x00200000;
                }
            }
            // xHC Runtime Registers
            offset::IMAN => self.lock().unwrap().interrupt_management = value,
            offset::IMOD => self.lock().unwrap().interrupt_moderation_interval = value,
            offset::ERSTSZ => assert!(value <= 1, "only a single segment supported"),
            offset::ERSTBA => {
                let mut xhci = self.lock().unwrap();
                let dma_bus = xhci.dma_bus.clone();
                xhci.event_ring.configure(value, dma_bus)
            }
            offset::ERSTBA_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            offset::ERDP => self
                .lock()
                .unwrap()
                .event_ring
                .update_dequeue_pointer(value),
            offset::ERDP_HI => assert_eq!(value, 0, "no support for configuration above 4G"),
            0x2000 => self.lock().unwrap().doorbell(),
            0x2004 => self.lock().unwrap().doorbell_slot1(value),
            _ => {
                debug! {"unknown write to {:#x} with value {:#x}", req.addr, value};
                //todo!()
            }
        }
    }

    fn read_io(&self, region: u32, req: Request) -> u64 {
        // The XHCI Controller has a single MMIO BAR.
        assert_eq!(region, 0);

        match req.addr {
            // xHC Capability Registers
            offset::CAPLENGTH => OP_BASE,
            offset::HCIVERSION => capability::HCIVERSION,
            offset::HCSPARAMS1 => capability::HCSPARAMS1,
            offset::HCSPARAMS2 => 0, /* ERST Max size is a single segment */
            offset::HCSPARAMS3 => 0,
            offset::HCCPARAMS1 => capability::HCCPARAMS1,
            offset::DBOFF => 0x2000,
            offset::RTSOFF => RUN_BASE,
            offset::HCCPARAMS2 => 0,

            // xHC Extended Capability ("Supported Protocols Capability")
            offset::SUPPORTED_PROTOCOLS => capability::supported_protocols::CAP_INFO,
            offset::SUPPORTED_PROTOCOLS_CONFIG => capability::supported_protocols::CONFIG,

            // xHC Operational Registers
            offset::USBCMD => 0,
            offset::USBSTS => self.lock().unwrap().status(),
            offset::DNCTL => 2,
            offset::CRCR => self.lock().unwrap().command_ring_status(),
            offset::CRCR_HI => 0,
            offset::PAGESIZE => 0x1, /* 4k Pages */
            offset::CONFIG => self.lock().unwrap().config(),

            offset::PORTSC => {
                let val = self.lock().unwrap().portsc;
                debug!("read PORTSC detected, supplying {:#x}", val);
                val
                //portsc::DEFAULT
            }
            offset::PORTLI => 0,

            // xHC Runtime Registers
            offset::IMAN => self.lock().unwrap().interrupt_management,
            offset::IMOD => self.lock().unwrap().interrupt_moderation_interval,
            offset::ERSTSZ => 1,
            offset::ERSTBA => self.lock().unwrap().event_ring.read_base_address(),
            offset::ERSTBA_HI => 0,
            offset::ERDP => self.lock().unwrap().event_ring.read_dequeue_pointer(),
            offset::ERDP_HI => 0,
            0x2000 => 0, // kernel reads the doorbell after write
            0x2004 => 0, // kernel reads the doorbell after write
            offset::DCBAAP => {
                let dc = self.lock().unwrap().device_contexts.get(0).map(|x| *x);
                debug!("read from DCBAAP {:?}", dc);
                dc.unwrap_or(0)
            }
            offset::DCBAAP_HI => {
                debug!("read from DCBAAP_HI");
                0
            }

            // Everything else is Reserved Zero
            _ => {
                debug! {"unknown read from {:#x}", req.addr};
                todo!()
            }
        }
    }

    fn bar(&self, bar_no: u8) -> Option<BarInfo> {
        self.lock().unwrap().config_space.bar(bar_no)
    }
}
