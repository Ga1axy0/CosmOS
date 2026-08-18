//! Device Tree cell, `reg`, and firmware-address decoding.

/// Decode one or two big-endian Device Tree cells as a machine address.
pub fn read_cells(value: &[u8], cells: usize) -> Option<usize> {
    if cells == 0 || cells > 2 || value.len() < cells * 4 {
        return None;
    }
    let mut out = 0usize;
    for cell in 0..cells {
        out = (out << 32) | read_be_u32(value.get(cell * 4..cell * 4 + 4)?)? as usize;
    }
    Some(out)
}

/// Iterate all address/size tuples in a `reg` property.
pub fn parse_reg(
    mut value: &[u8],
    address_cells: usize,
    size_cells: usize,
    mut f: impl FnMut(usize, usize),
) {
    let stride = (address_cells + size_cells) * 4;
    if stride == 0 {
        return;
    }
    while value.len() >= stride {
        let Some(start) = read_cells(&value[..address_cells * 4], address_cells) else { break; };
        let size_offset = address_cells * 4;
        let Some(size) = read_cells(&value[size_offset..size_offset + size_cells * 4], size_cells) else { break; };
        if size != 0 { f(start, size); }
        value = &value[stride..];
    }
}

/// Iterate address translations in a `ranges` property.
pub fn parse_ranges(
    mut value: &[u8],
    child_address_cells: usize,
    parent_address_cells: usize,
    size_cells: usize,
    mut f: impl FnMut(usize, usize, usize),
) {
    let stride = (child_address_cells + parent_address_cells + size_cells) * 4;
    if stride == 0 { return; }
    while value.len() >= stride {
        let child_end = child_address_cells * 4;
        let parent_end = child_end + parent_address_cells * 4;
        let Some(child) = read_cells(&value[..child_end], child_address_cells) else { break; };
        let Some(parent) = read_cells(&value[child_end..parent_end], parent_address_cells) else { break; };
        let Some(size) = read_cells(&value[parent_end..stride], size_cells) else { break; };
        f(child, parent, size);
        value = &value[stride..];
    }
}

/// Return the non-prefetchable memory aperture in a PCI `ranges` property.
pub fn parse_pci_memory_range(
    mut value: &[u8],
    child_address_cells: usize,
    parent_address_cells: usize,
    size_cells: usize,
) -> Option<(usize, usize)> {
    let stride = (child_address_cells + parent_address_cells + size_cells) * 4;
    while child_address_cells >= 3 && size_cells != 0 && value.len() >= stride {
        let flags = read_be_u32(&value[..4])?;
        let parent_offset = child_address_cells * 4;
        let size_offset = parent_offset + parent_address_cells * 4;
        if (flags >> 24) & 0x03 == 0x02 {
            return Some((read_cells(&value[parent_offset..size_offset], parent_address_cells)?, read_cells(&value[size_offset..stride], size_cells)?));
        }
        value = &value[stride..];
    }
    None
}

/// Normalize a firmware CPU address into CosmOS physical-address form.
pub fn firmware_address_to_phys(address: usize) -> usize {
    #[cfg(target_arch = "loongarch64")]
    { crate::platform::translate_direct_mapped_kernel_va(address).unwrap_or(address) }
    #[cfg(not(target_arch = "loongarch64"))]
    { address }
}

/// Decode a big-endian 32-bit cell.
pub fn read_be_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}
