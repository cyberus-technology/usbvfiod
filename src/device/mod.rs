//! # Device Emulation Code
//!
//! This crate contains device emulation code. It
//! should never depend on hypervisor, x86 or Linux specific parts.

#![deny(missing_docs)]
#![deny(rustdoc::all)]
#![allow(rustdoc::private_doc_tests)]
#![deny(clippy::must_use_candidate)]
#![deny(missing_debug_implementations)]

pub mod bus;
pub mod interrupt_line;
pub mod interval;
pub mod msi_message;
pub mod pci;
pub mod register_set;
pub mod xhci;
