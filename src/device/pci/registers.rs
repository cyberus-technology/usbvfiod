/// A simple PORTSC register implementation supporting RW1C bits.
///
/// The PORTSC register requires us to initially set some bits and
/// later react to 1-to-clear writes (RW1C) to get a device to show up.
/// Perhaps later we need more fine-grained access to the bits or state
/// handling, but we can use the simplistic implementation for now.
#[derive(Debug, Clone)]
pub struct PortscRegister {
    value: u64,
    bitmask_rw1c: u64,
}

impl PortscRegister {
    /// Create a new instance of the PORTSC register.
    ///
    /// # Parameters
    ///
    /// - initial_value: the initial value of the register.
    pub const fn new(initial_value: u64) -> Self {
        Self {
            value: initial_value,
            bitmask_rw1c: 0x00260000,
        }
    }

    /// Read the current register value.
    ///
    /// This function should be called when an MMIO read happens.
    pub const fn read(&self) -> u64 {
        self.value
    }

    /// Update the current register value.
    ///
    /// This function should be called when an MMIO write happens.
    /// RW1C bits are updates according to RW1C semantics, all
    /// other bits are treated as read-only.
    pub const fn write(&mut self, new_value: u64) {
        let bits_to_clear = new_value & self.bitmask_rw1c;
        self.value &= !bits_to_clear;
    }
}
