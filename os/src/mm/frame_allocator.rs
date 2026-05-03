//! Physical page frame allocator

use super::{PhysAddr, PhysPageNum};
use crate::fs::PAGE_CACHE_MANAGER;
use crate::mm::heap_allocator::KERNEL_HEAP_BYTES;
use crate::{config::MEMORY_END, sync::SpinNoIrqLock};
use core::fmt::{self, Debug, Formatter};
use lazy_static::*;
use virtio_drivers::PAGE_SIZE;
use core::sync::atomic::Ordering;

const MAX_ORDER: usize = 32;
const INVALID_PPN: usize = usize::MAX;

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
    free_list: [Option<usize>; MAX_ORDER],
    free_pages: usize,
    allocated_pages: usize,
}

impl BuddyFrameAllocator {
    pub fn init(&mut self, l: PhysPageNum, r: PhysPageNum) {
        self.start = l.0;
        self.end = r.0;
        self.free_list = [None; MAX_ORDER];
        self.free_pages = 0;
        self.allocated_pages = 0;
        self.add_range(l.0, r.0);
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
        let mut ppn = ppn.0;
        let pages = 1usize << order;
        if order >= MAX_ORDER
            || ppn < self.start
            || ppn + pages > self.end
            || ppn & (pages - 1) != 0
            || self.contains_free_block(ppn)
        {
            panic!(
                "Frame ppn={:#x}, pages={} has not been allocated!",
                ppn, pages
            );
        }

        let mut current_order = order;
        while current_order + 1 < MAX_ORDER {
            let buddy = ppn ^ (1usize << current_order);
            if buddy < self.start || buddy + (1usize << current_order) > self.end {
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
            free_list: [None; MAX_ORDER],
            free_pages: 0,
            allocated_pages: 0,
        }
    }
    fn alloc(&mut self) -> Option<PhysPageNum> {
        trace!(
            "FrameAllocator: Used {} | PageCache {} | Free {} | Kernel heap {}",
            self.allocated_pages,
            PAGE_CACHE_MANAGER.lock().cached_pages,
            self.free_pages,
            KERNEL_HEAP_BYTES.load(Ordering::Acquire) / PAGE_SIZE
        );
        self.alloc_order(0)
    }
    fn dealloc(&mut self, ppn: PhysPageNum) {
        self.dealloc_order(ppn, 0);
    }
}

type FrameAllocatorImpl = BuddyFrameAllocator;

lazy_static! {
    pub static ref FRAME_ALLOCATOR: SpinNoIrqLock<FrameAllocatorImpl> =
        SpinNoIrqLock::new(FrameAllocatorImpl::new());
}

pub fn init_frame_allocator() {
    extern "C" {
        fn ekernel();
    }
    FRAME_ALLOCATOR.lock().init(
        PhysAddr::from(ekernel as usize).ceil(),
        PhysAddr::from(MEMORY_END).floor(),
    );
}

/// Allocate a physical page frame in FrameTracker style
pub fn frame_alloc() -> Option<FrameTracker> {
    FRAME_ALLOCATOR.lock().alloc().map(FrameTracker::new)
}

/// Deallocate a physical page frame with a given ppn
pub fn frame_dealloc(ppn: PhysPageNum) {
    FRAME_ALLOCATOR.lock().dealloc(ppn);
}

/// Allocate a physically contiguous frame range.
/// Simplified implmentation: maybe fail when align_pages > pages (require over-alignment)
pub fn frame_alloc_contiguous(pages: usize, align_pages: usize) -> Option<ContiguousFrames> {
    info!("Allocating contiguous frames: pages={}, align_pages={}", pages, align_pages);
    if pages == 0 || align_pages == 0 || !pages.is_power_of_two() || !align_pages.is_power_of_two()
    {
        return None;
    }
    let order = pages.trailing_zeros() as usize;
    let start = FRAME_ALLOCATOR.lock().alloc_order(order)?;
    if start.0 & (align_pages - 1) != 0 {
        FRAME_ALLOCATOR.lock().dealloc_order(start, order);
        return None;
    }
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
