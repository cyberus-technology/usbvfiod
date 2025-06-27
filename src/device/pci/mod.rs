//! # PCI Local Bus Emulation
//!
//! The PCI Local Bus is the central component for attaching devices
//! to a virtual machine. This module contains the generic PCI
//! emulation logic for the configuration space.
pub mod config_space;
pub mod constants;
pub mod device_slots;
pub mod msix_table;
pub mod registers;
pub mod rings;
pub mod traits;
pub mod trb;
pub mod xhci;
