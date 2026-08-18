//! Firmware-described generic device resources.

/// One physical MMIO window and its optional interrupt specifier.
///
/// Binding-specific resource collections are built from this type rather than
/// teaching the early boot context about every driver.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeviceResource {
    /// Physical register base.
    pub start: usize,
    /// Register window size.
    pub size: usize,
    /// First decoded interrupt specifier cell, when present.
    pub irq: Option<u32>,
}

impl DeviceResource {
    /// Construct an absent resource sentinel for fixed early collections.
    pub const fn empty() -> Self {
        Self {
            start: 0,
            size: 0,
            irq: None,
        }
    }

    /// Return the exclusive end of this window when it does not overflow.
    pub fn end(self) -> Option<usize> {
        self.start.checked_add(self.size)
    }
}
