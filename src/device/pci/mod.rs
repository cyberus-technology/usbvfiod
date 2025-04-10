//! # PCI Local Bus Emulation
//!
//! The PCI Local Bus is the central component for attaching devices
//! to a virtual machine. This module contains the generic PCI
//! emulation logic for the configuration space.
pub mod config_space;
pub mod constants;
pub mod msix_table;
pub mod traits;
