//! # MSI Messages

use std::fmt::Debug;

/// The address/data pair for an MSI.
///
/// The PCI specification makes no limitations here. The interpretation of address and data is
/// entirely platform specific. For x86, refer to the Intel SDM Vol 3. Chapter 10.11 "Message
/// Signalled Interrupts".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiMessage {
    /// The guest physical address where the MSI message is sent to.
    ///
    /// As a rule of thumb, this value determines which CPU the interrupt is routed to.
    pub address: u64,

    /// The payload of the MSI.
    ///
    /// As a rule of thumb, this value determines what interrupt vector is triggered on the selected
    /// CPU.
    pub data: u16,
}

impl MsiMessage {
    /// Create a new [`MsiMessage`] struct.
    #[must_use]
    pub const fn new(address: u64, data: u16) -> Self {
        Self { address, data }
    }
}
