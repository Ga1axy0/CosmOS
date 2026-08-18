//! Minimal validated FDT blob handle.
//!
//! It has no device binding knowledge.  Binding-specific matching belongs in
//! drivers and platform enumeration code.

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_HEADER_SIZE: usize = 40;
/// Largest firmware FDT accepted during early boot.
pub const MAX_FDT_SIZE: usize = 16 * 1024 * 1024;

/// FDT header validation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FdtError {
    /// No FDT address was supplied.
    Null,
    /// The header magic is not the FDT magic.
    BadMagic,
    /// A header field or an internal block range is invalid.
    BadHeader,
    /// The structure block contains an invalid or truncated token stream.
    BadStructure,
}

/// Consumer for a depth-first Device Tree walk.
///
/// The walker owns wire-format decoding only.  Consumers interpret node names,
/// compatible strings, and properties according to their own binding.
pub trait FdtVisitor {
    /// A memory reservation-map entry.
    fn reserve_entry(&mut self, address: u64, size: u64);
    /// Start a node; the root node has an empty name.
    fn begin_node(&mut self, name: &[u8]);
    /// A property belonging to the current node.
    fn property(&mut self, name: &[u8], value: &[u8]);
    /// End the current node.
    fn end_node(&mut self);
}

/// A firmware FDT whose header and internal block ranges were validated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FdtBlob {
    virtual_address: usize,
    total_size: usize,
}

impl FdtBlob {
    /// Validate a directly mapped FDT virtual address.
    ///
    /// # Safety
    /// The caller must ensure the header is readable during early boot.
    pub unsafe fn from_virt(virtual_address: usize) -> Result<Self, FdtError> {
        if virtual_address == 0 {
            return Err(FdtError::Null);
        }
        let magic = read_be_u32(virtual_address).ok_or(FdtError::BadHeader)?;
        if magic != FDT_MAGIC {
            return Err(FdtError::BadMagic);
        }
        let total_size = read_be_u32(virtual_address + 4).ok_or(FdtError::BadHeader)? as usize;
        let off_dt_struct = read_be_u32(virtual_address + 8).ok_or(FdtError::BadHeader)? as usize;
        let off_dt_strings = read_be_u32(virtual_address + 12).ok_or(FdtError::BadHeader)? as usize;
        let off_mem_rsvmap = read_be_u32(virtual_address + 16).ok_or(FdtError::BadHeader)? as usize;
        let version = read_be_u32(virtual_address + 20).ok_or(FdtError::BadHeader)?;
        let last_compatible_version = read_be_u32(virtual_address + 24).ok_or(FdtError::BadHeader)?;
        let size_dt_strings = read_be_u32(virtual_address + 32).ok_or(FdtError::BadHeader)? as usize;
        let size_dt_struct = read_be_u32(virtual_address + 36).ok_or(FdtError::BadHeader)? as usize;
        if !(FDT_HEADER_SIZE..=MAX_FDT_SIZE).contains(&total_size)
            || version < 17
            || last_compatible_version > 17
            || off_mem_rsvmap < FDT_HEADER_SIZE
            || off_mem_rsvmap >= total_size
            || off_dt_struct >= total_size
            || off_dt_struct.checked_add(size_dt_struct).is_none_or(|end| end > total_size)
            || off_dt_strings >= total_size
            || off_dt_strings.checked_add(size_dt_strings).is_none_or(|end| end > total_size)
        {
            return Err(FdtError::BadHeader);
        }
        Ok(Self {
            virtual_address,
            total_size,
        })
    }

    /// Return the validated directly mapped FDT address.
    pub const fn virtual_address(self) -> usize {
        self.virtual_address
    }

    /// Return the complete validated blob size.
    pub const fn total_size(self) -> usize {
        self.total_size
    }

    /// Decode reservations and the structure block, invoking `visitor` in DT
    /// order.  No allocation and no binding-specific policy is involved.
    pub fn walk(self, visitor: &mut impl FdtVisitor) -> Result<(), FdtError> {
        let off_mem_rsvmap = self.header_u32(16)? as usize;
        let off_dt_struct = self.header_u32(8)? as usize;
        let off_dt_strings = self.header_u32(12)? as usize;
        let size_dt_strings = self.header_u32(32)? as usize;
        let size_dt_struct = self.header_u32(36)? as usize;
        let structure_end = off_dt_struct.checked_add(size_dt_struct).ok_or(FdtError::BadStructure)?;
        let strings_end = off_dt_strings.checked_add(size_dt_strings).ok_or(FdtError::BadStructure)?;
        if structure_end > self.total_size || strings_end > self.total_size {
            return Err(FdtError::BadStructure);
        }

        let mut reserve = off_mem_rsvmap;
        let mut reserve_terminated = false;
        while reserve.checked_add(16).is_some_and(|end| end <= self.total_size) {
            let address = self.be_u64(reserve)?;
            let size = self.be_u64(reserve + 8)?;
            reserve += 16;
            if address == 0 && size == 0 {
                reserve_terminated = true;
                break;
            }
            visitor.reserve_entry(address, size);
        }
        if !reserve_terminated {
            return Err(FdtError::BadStructure);
        }

        let mut cursor = off_dt_struct;
        while cursor.checked_add(4).is_some_and(|end| end <= structure_end) {
            let token = self.be_u32(cursor)?;
            cursor += 4;
            match token {
                1 => {
                    let start = cursor;
                    while cursor < structure_end && self.byte(cursor)? != 0 {
                        cursor += 1;
                    }
                    if cursor == structure_end {
                        return Err(FdtError::BadStructure);
                    }
                    visitor.begin_node(self.slice(start, cursor - start)?);
                    cursor = align4(cursor + 1).ok_or(FdtError::BadStructure)?;
                    if cursor > structure_end {
                        return Err(FdtError::BadStructure);
                    }
                }
                2 => visitor.end_node(),
                3 => {
                    if cursor.checked_add(8).is_none_or(|end| end > structure_end) {
                        return Err(FdtError::BadStructure);
                    }
                    let len = self.be_u32(cursor)? as usize;
                    let nameoff = self.be_u32(cursor + 4)? as usize;
                    cursor += 8;
                    let value_end = cursor.checked_add(len).ok_or(FdtError::BadStructure)?;
                    if value_end > structure_end || nameoff >= size_dt_strings {
                        return Err(FdtError::BadStructure);
                    }
                    let name_start = off_dt_strings + nameoff;
                    let mut name_end = name_start;
                    while name_end < strings_end && self.byte(name_end)? != 0 {
                        name_end += 1;
                    }
                    if name_end == strings_end {
                        return Err(FdtError::BadStructure);
                    }
                    visitor.property(self.slice(name_start, name_end - name_start)?, self.slice(cursor, len)?);
                    cursor = align4(value_end).ok_or(FdtError::BadStructure)?;
                    if cursor > structure_end {
                        return Err(FdtError::BadStructure);
                    }
                }
                4 => {}
                9 => return Ok(()),
                _ => return Err(FdtError::BadStructure),
            }
        }
        Err(FdtError::BadStructure)
    }

    fn header_u32(self, offset: usize) -> Result<u32, FdtError> {
        self.be_u32(offset)
    }

    fn be_u32(self, offset: usize) -> Result<u32, FdtError> {
        let address = self.virtual_address.checked_add(offset).ok_or(FdtError::BadStructure)?;
        read_be_u32(address).ok_or(FdtError::BadStructure)
    }

    fn be_u64(self, offset: usize) -> Result<u64, FdtError> {
        let high = self.be_u32(offset)? as u64;
        let low = self.be_u32(offset.checked_add(4).ok_or(FdtError::BadStructure)?)? as u64;
        Ok((high << 32) | low)
    }

    fn byte(self, offset: usize) -> Result<u8, FdtError> {
        if offset >= self.total_size {
            return Err(FdtError::BadStructure);
        }
        let address = self.virtual_address.checked_add(offset).ok_or(FdtError::BadStructure)?;
        Ok(unsafe { core::ptr::read_volatile(address as *const u8) })
    }

    fn slice(self, offset: usize, len: usize) -> Result<&'static [u8], FdtError> {
        let end = offset.checked_add(len).ok_or(FdtError::BadStructure)?;
        if end > self.total_size {
            return Err(FdtError::BadStructure);
        }
        let address = self.virtual_address.checked_add(offset).ok_or(FdtError::BadStructure)?;
        Ok(unsafe { core::slice::from_raw_parts(address as *const u8, len) })
    }
}

fn align4(value: usize) -> Option<usize> {
    value.checked_add(3).map(|value| value & !3)
}

fn read_be_u32(address: usize) -> Option<u32> {
    if address == 0 {
        return None;
    }
    Some(u32::from_be(unsafe {
        core::ptr::read_volatile(address as *const u32)
    }))
}
