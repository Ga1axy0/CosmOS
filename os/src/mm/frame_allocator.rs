//! Physical page frame allocator

use super::{virt_to_phys, PhysPageNum};
use crate::bootinfo::{self, PhysMemoryRegion};
use crate::config::PAGE_SIZE;
use crate::fs::PAGE_CACHE_MANAGER;
use crate::sync::SpinNoIrqLock;
use core::cmp::{max, min};
use core::fmt::{self, Debug, Formatter};
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::*;

const MAX_ORDER: usize = 32;
const INVALID_PPN: usize = usize::MAX;
const MAX_MANAGED_REGIONS: usize = 16;
static FRAME_ALLOC_OOM_COUNT: AtomicUsize = AtomicUsize::new(0);

/// tracker for physical page frame allocation and deallocation
pub struct FrameTracker {
    /// physical page number
    pub ppn: PhysPageNum,
}

impl FrameTracker {
    /// Create a new FrameTracker
    pub fn new(ppn: PhysPageNum) -> Self {
        // page cleaning
        let bytes_array = ppn.get_bytes_array();
        for i in bytes_array {
            *i = 0;
        }
        Self { ppn }
    }
}

impl Debug for FrameTracker {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("FrameTracker:PPN={:#x}", self.ppn.0))
    }
}

impl Drop for FrameTracker {
    fn drop(&mut self) {
        frame_dealloc(self.ppn);
    }
}

/// RAII handle for a physically contiguous frame range.
pub struct ContiguousFrames {
    start: PhysPageNum,
    pages: usize,
}

impl ContiguousFrames {
    fn new(start: PhysPageNum, pages: usize) -> Self {
        for ppn in start.0..start.0 + pages {
            clear_frame(PhysPageNum(ppn));
        }
        Self { start, pages }
    }

    /// Return the first physical page number in this contiguous range.
    pub fn start_ppn(&self) -> PhysPageNum {
        self.start
    }

    /// Return the number of pages owned by this range.
    pub fn pages(&self) -> usize {
        self.pages
    }
}

impl Drop for ContiguousFrames {
    fn drop(&mut self) {
        frame_dealloc_range(self.start, self.pages);
    }
}

trait FrameAllocator {
    fn new() -> Self;
    fn alloc(&mut self) -> Option<PhysPageNum>;
    fn dealloc(&mut self, ppn: PhysPageNum);
}

pub struct BuddyFrameAllocator {
    start: usize,
    end: usize,
    regions: [PpnRegion; MAX_MANAGED_REGIONS],
    region_count: usize,
    free_list: [Option<usize>; MAX_ORDER],
    free_pages: usize,
    allocated_pages: usize,
}

#[derive(Clone, Copy)]
struct PpnRegion {
    start: usize,
    end: usize,
}

impl PpnRegion {
    const fn empty() -> Self {
        Self { start: 0, end: 0 }
    }
}

impl BuddyFrameAllocator {
    pub fn init_from_bootinfo(&mut self, kernel_start: PhysPageNum, kernel_end: PhysPageNum) {
        self.reset();
        bootinfo::for_each_usable_memory_region(|region| {
            self.add_usable_region(region, kernel_start.0, kernel_end.0);
        });
    }

    fn reset(&mut self) {
        self.start = usize::MAX;
        self.end = 0;
        self.regions = [PpnRegion::empty(); MAX_MANAGED_REGIONS];
        self.region_count = 0;
        self.free_list = [None; MAX_ORDER];
        self.free_pages = 0;
        self.allocated_pages = 0;
    }

    fn add_usable_region(
        &mut self,
        region: PhysMemoryRegion,
        kernel_start: usize,
        kernel_end: usize,
    ) {
        let start = phys_addr_ceil_ppn(region.start);
        let end = phys_addr_floor_ppn(region.end);
        if start >= end {
            return;
        }

        if kernel_start < kernel_end {
            self.add_managed_range(start, min(end, kernel_start));
            self.add_managed_range(max(start, kernel_end), end);
        } else {
            self.add_managed_range(start, end);
        }
    }

    fn add_managed_range(&mut self, start: usize, end: usize) {
        if start >= end || self.region_count >= MAX_MANAGED_REGIONS {
            return;
        }
        self.regions[self.region_count] = PpnRegion { start, end };
        self.region_count += 1;
        self.start = self.start.min(start);
        self.end = self.end.max(end);
        self.add_range(start, end);
    }

    fn is_managed_range(&self, ppn: usize, pages: usize) -> bool {
        self.regions[..self.region_count]
            .iter()
            .any(|region| ppn >= region.start && ppn.saturating_add(pages) <= region.end)
    }

    fn set_next(ppn: usize, next: Option<usize>) {
        let next = next.unwrap_or(INVALID_PPN);
        *PhysPageNum(ppn).get_mut::<usize>() = next;
    }

    fn next(ppn: usize) -> Option<usize> {
        let next = *PhysPageNum(ppn).get_mut::<usize>();
        if next == INVALID_PPN {
            None
        } else {
            Some(next)
        }
    }

    fn push_block(&mut self, order: usize, ppn: usize) {
        debug_assert!(order < MAX_ORDER);
        debug_assert_eq!(ppn & ((1usize << order) - 1), 0);
        Self::set_next(ppn, self.free_list[order]);
        self.free_list[order] = Some(ppn);
    }

    fn pop_block(&mut self, order: usize) -> Option<usize> {
        let ppn = self.free_list[order]?;
        self.free_list[order] = Self::next(ppn);
        Self::set_next(ppn, None);
        Some(ppn)
    }

    fn remove_block(&mut self, order: usize, target: usize) -> bool {
        let mut current = self.free_list[order];
        let mut previous = None;
        while let Some(ppn) = current {
            let next = Self::next(ppn);
            if ppn == target {
                if let Some(previous) = previous {
                    Self::set_next(previous, next);
                } else {
                    self.free_list[order] = next;
                }
                Self::set_next(ppn, None);
                return true;
            }
            previous = current;
            current = next;
        }
        false
    }

    fn contains_free_block(&self, ppn: usize) -> bool {
        for order in 0..MAX_ORDER {
            let mut current = self.free_list[order];
            while let Some(block) = current {
                if block <= ppn && ppn < block + (1usize << order) {
                    return true;
                }
                current = Self::next(block);
            }
        }
        false
    }

    fn add_range(&mut self, mut start: usize, end: usize) {
        while start < end {
            let remaining = end - start;
            let lowbit_order = if start == 0 {
                MAX_ORDER - 1
            } else {
                (start.trailing_zeros() as usize).min(MAX_ORDER - 1)
            };
            let mut order = floor_log2(remaining).min(lowbit_order).min(MAX_ORDER - 1);
            while start + (1usize << order) > end {
                order -= 1;
            }
            self.push_block(order, start);
            self.free_pages += 1usize << order;
            start += 1usize << order;
        }
    }

    fn alloc_order(&mut self, order: usize) -> Option<PhysPageNum> {
        if order >= MAX_ORDER {
            return None;
        }
        let mut source_order = order;
        while source_order < MAX_ORDER && self.free_list[source_order].is_none() {
            source_order += 1;
        }
        if source_order == MAX_ORDER {
            return None;
        }
        let ppn = self.pop_block(source_order)?;
        while source_order > order {
            source_order -= 1;
            self.push_block(source_order, ppn + (1usize << source_order));
        }
        self.free_pages -= 1usize << order;
        self.allocated_pages += 1usize << order;
        Some(ppn.into())
    }

    fn dealloc_order(&mut self, ppn: PhysPageNum, order: usize) {
        if order >= MAX_ORDER {
            panic!(
                "Frame ppn={:#x}, order={} has not been allocated!",
                ppn.0, order
            );
        }
        let mut ppn = ppn.0;
        let pages = 1usize << order;
        if !self.is_managed_range(ppn, pages) || ppn & (pages - 1) != 0 {
            panic!(
                "Frame ppn={:#x}, pages={} has not been allocated!",
                ppn, pages
            );
        }
        debug_assert!(
            !self.contains_free_block(ppn),
            "frame ppn={:#x}, pages={} was already freed",
            ppn,
            pages
        );

        let mut current_order = order;
        while current_order + 1 < MAX_ORDER {
            let buddy = ppn ^ (1usize << current_order);
            if !self.is_managed_range(buddy, 1usize << current_order) {
                break;
            }
            if !self.remove_block(current_order, buddy) {
                break;
            }
            ppn = ppn.min(buddy);
            current_order += 1;
        }

        self.push_block(current_order, ppn);
        self.free_pages += pages;
        self.allocated_pages -= pages;
    }
}

impl FrameAllocator for BuddyFrameAllocator {
    fn new() -> Self {
        Self {
            start: 0,
            end: 0,
            regions: [PpnRegion::empty(); MAX_MANAGED_REGIONS],
            region_count: 0,
            free_list: [None; MAX_ORDER],
            free_pages: 0,
            allocated_pages: 0,
        }
    }
    fn alloc(&mut self) -> Option<PhysPageNum> {
        // trace!(
        //     "FrameAllocator: Used {} | PageCache {} | Free {} | Kernel heap {}",
        //     self.allocated_pages,
        //     PAGE_CACHE_MANAGER.lock().cached_pages,
        //     self.free_pages,
        //     KERNEL_HEAP_BYTES.load(Ordering::Acquire) / PAGE_SIZE
        // );
        self.alloc_order(0)
    }
    fn dealloc(&mut self, ppn: PhysPageNum) {
        self.dealloc_order(ppn, 0);
    }
}

type FrameAllocatorImpl = BuddyFrameAllocator;

#[derive(Clone, Copy, Debug)]
/// Runtime statistics of the frame allocator.
pub struct FrameAllocatorStats {
    /// Number of free physical pages.
    pub free_pages: usize,
    /// Number of allocated physical pages.
    pub allocated_pages: usize,
    /// Total number of managed physical pages.
    pub total_pages: usize,
    /// Number of failed single-frame allocation attempts.
    pub oom_count: usize,
}

lazy_static! {
    pub static ref FRAME_ALLOCATOR: SpinNoIrqLock<FrameAllocatorImpl> =
        SpinNoIrqLock::new(FrameAllocatorImpl::new());
}

pub fn init_frame_allocator() {
    extern "C" {
        fn skernel();
        fn ekernel();
    }
    let kernel_start = PhysPageNum(phys_addr_floor_ppn(virt_to_phys(skernel as usize)));
    let kernel_end = PhysPageNum(phys_addr_ceil_ppn(virt_to_phys(ekernel as usize)));
    FRAME_ALLOCATOR
        .lock()
        .init_from_bootinfo(kernel_start, kernel_end);
}

/// Return runtime statistics of the frame allocator.
pub fn frame_allocator_stats() -> FrameAllocatorStats {
    let allocator = FRAME_ALLOCATOR.lock();
    let free_pages = allocator.free_pages;
    let allocated_pages = allocator.allocated_pages;
    FrameAllocatorStats {
        free_pages,
        allocated_pages,
        total_pages: free_pages + allocated_pages,
        oom_count: FRAME_ALLOC_OOM_COUNT.load(Ordering::Acquire),
    }
}

/// Allocate a physical page frame in FrameTracker style
pub fn frame_alloc() -> Option<FrameTracker> {
    let ppn = {
        let mut allocator = FRAME_ALLOCATOR.lock();
        allocator.alloc()
    };
    ppn.map(FrameTracker::new).or_else(|| {
        FRAME_ALLOC_OOM_COUNT.fetch_add(1, Ordering::AcqRel);
        let frame_allocator_stats = frame_allocator_stats();
        error!(
            "frame_alloc: out of memory (free={} cached={} low={} high={} total={})",
            frame_allocator_stats.free_pages,
            PAGE_CACHE_MANAGER.lock().cached_pages,
            PAGE_CACHE_MANAGER.lock().low_watermark,
            PAGE_CACHE_MANAGER.lock().high_watermark,
            frame_allocator_stats.total_pages,
        );
        None
    })
}

/// Allocate a physical page frame, triggering page-cache reclamation on first
/// failure. Prefer this over [`frame_alloc`] in process-context paths
/// (syscall handling, page faults) where blocking on I/O is safe. Do not use
/// from interrupt or trap-from-kernel context.
pub fn frame_alloc_with_reclaim() -> Option<FrameTracker> {
    if let Some(frame) = frame_alloc() {
        return Some(frame);
    }
    crate::fs::reclaim_if_needed();
    frame_alloc()
}

/// Deallocate a physical page frame with a given ppn
pub fn frame_dealloc(ppn: PhysPageNum) {
    FRAME_ALLOCATOR.lock().dealloc(ppn);
}

/// Allocate a physically contiguous frame range.
/// Simplified implmentation: maybe fail when align_pages > pages (require over-alignment)
pub fn frame_alloc_contiguous(pages: usize, align_pages: usize) -> Option<ContiguousFrames> {
    if pages == 0 || align_pages == 0 || !pages.is_power_of_two() || !align_pages.is_power_of_two()
    {
        return None;
    }
    let order = pages.trailing_zeros() as usize;
    let start = {
        let mut allocator = FRAME_ALLOCATOR.lock();
        let start = allocator.alloc_order(order)?;
        if start.0 & (align_pages - 1) != 0 {
            allocator.dealloc_order(start, order);
            return None;
        }
        start
    };
    Some(ContiguousFrames::new(start, pages))
}

/// Deallocate a physically contiguous frame range.
pub fn frame_dealloc_range(start: PhysPageNum, pages: usize) {
    if pages == 0 || !pages.is_power_of_two() {
        panic!("invalid frame range: start={:#x}, pages={}", start.0, pages);
    }
    FRAME_ALLOCATOR
        .lock()
        .dealloc_order(start, pages.trailing_zeros() as usize);
}

fn clear_frame(ppn: PhysPageNum) {
    for byte in ppn.get_bytes_array() {
        *byte = 0;
    }
}

fn floor_log2(value: usize) -> usize {
    usize::BITS as usize - 1 - value.leading_zeros() as usize
}

fn phys_addr_floor_ppn(pa: usize) -> usize {
    pa / PAGE_SIZE
}

fn phys_addr_ceil_ppn(pa: usize) -> usize {
    pa.saturating_add(PAGE_SIZE - 1) / PAGE_SIZE
}

#[allow(unused)]
pub fn frame_allocator_test() {
    use alloc::vec::Vec;

    let mut v: Vec<FrameTracker> = Vec::new();
    for i in 0..5 {
        let frame = frame_alloc().unwrap();
        println!("{:?}", frame);
        v.push(frame);
    }
    v.clear();
    for i in 0..5 {
        let frame = frame_alloc().unwrap();
        println!("{:?}", frame);
        v.push(frame);
    }
    drop(v);
    println!("frame_allocator_test passed!");
}
