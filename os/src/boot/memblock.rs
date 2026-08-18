//! Early physical-memory bookkeeping, modelled after Linux `memblock`.
//!
//! Firmware supplies available RAM and reservations independently.  Keeping
//! the two lists intact until the page allocator is initialized avoids making
//! every early consumer reimplement range subtraction.

use core::cmp::{max, min};

/// Maximum separately described RAM regions retained before allocator setup.
pub const MAX_MEMORY_REGIONS: usize = 32;
/// Maximum reserved physical ranges retained before allocator setup.
pub const MAX_RESERVED_REGIONS: usize = 64;

/// A half-open physical address range `[start, end)`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PhysMemoryRegion {
    /// Inclusive physical start address.
    pub start: usize,
    /// Exclusive physical end address.
    pub end: usize,
}

impl PhysMemoryRegion {
    /// Empty sentinel range.
    pub const EMPTY: Self = Self { start: 0, end: 0 };

    /// Create a half-open physical range.
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// Return whether the range contains no byte.
    pub const fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

/// Why a physical range cannot be handed to the page allocator.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReserveKind {
    /// Reserved by firmware itself.
    Firmware,
    /// The live firmware device tree blob.
    Fdt,
    /// The loaded kernel image.
    Kernel,
    /// A firmware-provided initial RAM disk.
    Initrd,
    /// A platform-specific reservation.
    Platform,
}

/// Failure to retain an early-boot range.  This is fatal: silently dropping a
/// reservation can corrupt the FDT, firmware, or the kernel image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemBlockError {
    /// Too many disjoint RAM ranges were supplied.
    MemoryCapacity,
    /// Too many disjoint reservations were supplied.
    ReservedCapacity,
}

/// Fixed-capacity early physical-memory database.
#[derive(Clone, Copy, Debug)]
pub struct MemBlock {
    memory: [PhysMemoryRegion; MAX_MEMORY_REGIONS],
    memory_count: usize,
    reserved: [PhysMemoryRegion; MAX_RESERVED_REGIONS],
    reserved_count: usize,
}

impl MemBlock {
    /// Create an empty physical-memory database.
    pub const fn empty() -> Self {
        Self {
            memory: [PhysMemoryRegion::EMPTY; MAX_MEMORY_REGIONS],
            memory_count: 0,
            reserved: [PhysMemoryRegion::EMPTY; MAX_RESERVED_REGIONS],
            reserved_count: 0,
        }
    }

    /// Register an available RAM range, merging overlapping neighbours.
    pub fn add_memory(&mut self, start: usize, end: usize) -> Result<(), MemBlockError> {
        insert_range(
            &mut self.memory,
            &mut self.memory_count,
            PhysMemoryRegion::new(start, end),
            MemBlockError::MemoryCapacity,
        )
    }

    /// Mark a physical range unavailable to the page allocator.
    pub fn reserve(
        &mut self,
        start: usize,
        end: usize,
        _kind: ReserveKind,
    ) -> Result<(), MemBlockError> {
        insert_range(
            &mut self.reserved,
            &mut self.reserved_count,
            PhysMemoryRegion::new(start, end),
            MemBlockError::ReservedCapacity,
        )
    }

    /// Return normalized firmware RAM ranges.
    pub fn memory(&self) -> &[PhysMemoryRegion] {
        &self.memory[..self.memory_count]
    }

    /// Return normalized reserved ranges.
    pub fn reserved(&self) -> &[PhysMemoryRegion] {
        &self.reserved[..self.reserved_count]
    }

    /// Invoke `f` for every available RAM fragment after all reservations.
    pub fn for_each_free_range(&self, mut f: impl FnMut(PhysMemoryRegion)) {
        for memory in self.memory() {
            let mut cursor = memory.start;
            for reserved in self.reserved() {
                if reserved.end <= cursor {
                    continue;
                }
                if reserved.start >= memory.end {
                    break;
                }
                let end = min(reserved.start, memory.end);
                if cursor < end {
                    f(PhysMemoryRegion::new(cursor, end));
                }
                cursor = max(cursor, reserved.end);
                if cursor >= memory.end {
                    break;
                }
            }
            if cursor < memory.end {
                f(PhysMemoryRegion::new(cursor, memory.end));
            }
        }
    }
}

impl Default for MemBlock {
    fn default() -> Self {
        Self::empty()
    }
}

fn insert_range(
    ranges: &mut [PhysMemoryRegion],
    count: &mut usize,
    mut range: PhysMemoryRegion,
    capacity_error: MemBlockError,
) -> Result<(), MemBlockError> {
    if range.is_empty() {
        return Ok(());
    }

    let mut index = 0;
    while index < *count && ranges[index].end < range.start {
        index += 1;
    }
    while index < *count && ranges[index].start <= range.end {
        range.start = min(range.start, ranges[index].start);
        range.end = max(range.end, ranges[index].end);
        for move_index in index + 1..*count {
            ranges[move_index - 1] = ranges[move_index];
        }
        *count -= 1;
    }
    if *count == ranges.len() {
        return Err(capacity_error);
    }
    for move_index in (index..*count).rev() {
        ranges[move_index + 1] = ranges[move_index];
    }
    ranges[index] = range;
    *count += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_and_subtracts_ranges() {
        let mut mem = MemBlock::empty();
        mem.add_memory(0x1000, 0x5000).unwrap();
        mem.add_memory(0x4000, 0x9000).unwrap();
        mem.reserve(0x2000, 0x3000, ReserveKind::Firmware).unwrap();
        mem.reserve(0x6000, 0x7000, ReserveKind::Kernel).unwrap();
        let mut free = [(0, 0); 3];
        let mut count = 0;
        mem.for_each_free_range(|range| {
            free[count] = (range.start, range.end);
            count += 1;
        });
        assert_eq!(&free[..count], &[(0x1000, 0x2000), (0x3000, 0x6000), (0x7000, 0x9000)]);
    }
}
