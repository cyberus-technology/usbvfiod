/// A simple PORTSC register implementation supporting RW1C bits.
///
/// The PORTSC register requires us to initially set some bits and
/// later react to 1-to-clear writes (RW1C) to get a device to show up.
/// Perhaps later we need more fine-grained access to the bits or state
/// handling, but we can use the simplistic implementation for now.
#[derive(Debug, Clone, Copy)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portsc_read_write() {
        let mut reg = PortscRegister::new(0x00260203);
        assert_eq!(reg.read(), 0x00260203);

        reg.write(0x0);
        assert_eq!(
            reg.read(),
            0x00260203,
            "writing 0 should affect neither the read-only nor the RW1C bits."
        );

        reg.write(0x00200000);
        assert_eq!(
            reg.read(),
            0x00060203,
            "writing 1 to bit 21 should clear the bit."
        );

        reg.write(0x00040000);
        assert_eq!(
            reg.read(),
            0x00020203,
            "writing 1 to bit 18 should clear the bit."
        );

        reg.write(0x00020000);
        assert_eq!(
            reg.read(),
            0x00000203,
            "writing 1 to bit 17 should clear the bit."
        );
    }
}
