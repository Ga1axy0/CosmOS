//! Open Firmware PCI host-controller resources.

use crate::of::DeviceResource;

const PCI_INTX_ENTRIES: usize = 32 * 4;

/// One PCI ECAM host bridge and its firmware-assigned MMIO aperture.
#[derive(Clone, Copy, Debug)]
pub struct PciHostResource {
    /// ECAM register window.
    pub ecam: DeviceResource,
    /// Inclusive first PCI bus number.
    pub bus_start: u8,
    /// Inclusive last PCI bus number.
    pub bus_end: u8,
    /// CPU physical base of the non-prefetchable PCI memory aperture.
    pub memory_start: usize,
    /// Size of the PCI memory aperture.
    pub memory_size: usize,
    intx_irqs: [u32; PCI_INTX_ENTRIES],
}

impl PciHostResource {
    /// Construct an initially empty parsed PCI host resource.
    pub(crate) const fn empty() -> Self {
        Self {
            ecam: DeviceResource::empty(), bus_start: 0, bus_end: 0,
            memory_start: 0, memory_size: 0, intx_irqs: [0; PCI_INTX_ENTRIES],
        }
    }

    /// Record an INTx mapping parsed from `interrupt-map`.
    pub(crate) fn set_intx_irq(&mut self, slot: usize, pin: usize, irq: u32) {
        if slot < 32 && (1..=4).contains(&pin) {
            self.intx_irqs[slot * 4 + pin - 1] = irq;
        }
    }

    /// Resolve a PCI slot and one-based INTx pin through `interrupt-map`.
    pub fn intx_irq(&self, slot: u8, pin: u8) -> Option<u32> {
        if pin == 0 || pin > 4 || slot >= 32 { return None; }
        let irq = self.intx_irqs[slot as usize * 4 + pin as usize - 1];
        (irq != 0).then_some(irq)
    }
}
