//! Device Tree interrupt-property decoding.

use crate::of::pci::PciHostResource;

/// Decode the first interrupt specifier carried by an `interrupts` property.
pub fn parse_interrupts(value: &[u8]) -> Option<u32> {
    read_be_u32(value.get(..4)?)
}

/// Decode the supported Loongson PCI `interrupt-map` rows into `host`.
pub fn parse_pci_interrupt_map(mut value: &[u8], host: &mut PciHostResource) {
    const ROW_BYTES: usize = 7 * 4;
    while value.len() >= ROW_BYTES {
        let address_hi = read_be_u32(&value[..4]).unwrap_or(0);
        let pin = read_be_u32(&value[12..16]).unwrap_or(0) as usize;
        let irq = read_be_u32(&value[20..24]).unwrap_or(0);
        let slot = ((address_hi >> 11) & 0x1f) as usize;
        host.set_intx_irq(slot, pin, irq);
        value = &value[ROW_BYTES..];
    }
}

fn read_be_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}
