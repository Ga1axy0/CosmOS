//! Physical page frame allocator

use super::{virt_to_phys, PhysPageNum};
use crate::boot::context;
use crate::boot::memblock::PhysMemoryRegion;
use crate::config::{MAX_HARTS, PAGE_SIZE};
use crate::fs::PAGE_CACHE_MANAGER;
use crate::hal::hartid;
#[cfg(feature = "cosmos-meminfo")]
use crate::hal::traits::Timer as _;
#[cfg(feature = "cosmos-meminfo")]
use crate::hal::Plat;
use crate::sync::SpinNoIrqLock;
use core::cmp::{max, min};
use core::fmt::{self, Debug, Formatter};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use lazy_static::*;

const MAX_ORDER: usize = 32;
const INVALID_PPN: usize = usize::MAX;
const MAX_MANAGED_REGIONS: usize = 16;
const PER_CPU_FRAME_CACHE_CAPACITY: usize = 32;
const PER_CPU_FRAME_CACHE_FLUSH: usize = PER_CPU_FRAME_CACHE_CAPACITY / 2;
const PER_CPU_FRAME_CACHE_REFILL_ORDER: usize = 3;
const PER_CPU_FRAME_CACHE_REFILL_PAGES: usize = 1 << PER_CPU_FRAME_CACHE_REFILL_ORDER;
const PER_CPU_FRAME_CACHE_DEFAULT_ENABLED: bool = true;
// Optional O(1) buddy-membership acceleration for RAM spans up to one million
// pages. Larger firmware-described spans remain correct via free-list scans.
const MAX_BITMAP_PAGES: usize = 1024 * 1024;
// Each buddy order is stored in a separate word-aligned slice. Besides the
// geometric sum of at most two bits per page, reserve one rounding word per
// order; rounding only once for the whole bitmap is too small at the 4 GiB
// boundary.
const MAX_BITMAP_WORDS: usize = (2 * MAX_BITMAP_PAGES + 63) / 64 + MAX_ORDER;
static FRAME_ALLOC_OOM_COUNT: AtomicUsize = AtomicUsize::new(0);
static FRAME_PER_CPU_CACHE_ENABLED: AtomicBool = AtomicBool::new(false);
static FRAME_PER_CPU_CACHED_PAGES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static FRAME_ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static FRAME_DEALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static FRAME_CONTIGUOUS_ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static FRAME_RANGE_DEALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static FRAME_ALLOCATOR_LOCK_WAIT_TICKS: AtomicUsize = AtomicUsize::new(0);
/// Number of physical pages explicitly cleared before being handed to a caller.
#[cfg(feature = "cosmos-meminfo")]
static FRAME_ZEROED_PAGES: AtomicUsize = AtomicUsize::new(0);
/// Number of bytes explicitly cleared before being handed to a caller.
#[cfg(feature = "cosmos-meminfo")]
static FRAME_ZEROED_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Cumulative platform timer ticks spent clearing physical pages.
#[cfg(feature = "cosmos-meminfo")]
static FRAME_ZERO_TIME_TICKS: AtomicUsize = AtomicUsize::new(0);
/// Per-CPU frame-cache counters.
#[cfg(feature = "cosmos-meminfo")]
static FRAME_PER_CPU_CACHE_HITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static FRAME_PER_CPU_CACHE_MISSES: AtomicUsize = AtomicUsize::new(0);

struct PerCpuFrameCache {
    pages: [usize; PER_CPU_FRAME_CACHE_CAPACITY],
    len: usize,
}

impl PerCpuFrameCache {
    const fn new() -> Self {
        Self {
            pages: [INVALID_PPN; PER_CPU_FRAME_CACHE_CAPACITY],
            len: 0,
        }
    }

    fn push(&mut self, ppn: PhysPageNum) -> bool {
        if self.len == self.pages.len() {
            return false;
        }
        self.pages[self.len] = ppn.0;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<PhysPageNum> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        let ppn = self.pages[self.len];
        self.pages[self.len] = INVALID_PPN;
        Some(PhysPageNum(ppn))
    }

    fn clear(&mut self) {
        self.pages.fill(INVALID_PPN);
        self.len = 0;
    }
}

/// tracker for physical page frame allocation and deallocation
pub struct FrameTracker {
    /// physical page number
    pub ppn: PhysPageNum,
}

impl FrameTracker {
    /// Create a new FrameTracker
    pub fn new(ppn: PhysPageNum) -> Self {
        clear_frame(ppn);
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

struct FreeBlockBitmap {
    bits: [u64; MAX_BITMAP_WORDS],
    offsets: [usize; MAX_ORDER],
    words: [usize; MAX_ORDER],
    base: usize,
    span_pages: usize,
    enabled: bool,
}

impl FreeBlockBitmap {
    const fn empty() -> Self {
        Self {
            bits: [0; MAX_BITMAP_WORDS],
            offsets: [0; MAX_ORDER],
            words: [0; MAX_ORDER],
            base: 0,
            span_pages: 0,
            enabled: false,
        }
    }

    fn reset(&mut self, base: usize, end: usize) {
        self.bits.fill(0);
        self.offsets.fill(0);
        self.words.fill(0);
        self.base = base;
        self.span_pages = end.saturating_sub(base);
        self.enabled = base < end && self.span_pages <= MAX_BITMAP_PAGES;
        if !self.enabled {
            return;
        }

        let mut offset = 0;
        for order in 0..MAX_ORDER {
            let block_size = 1usize << order;
            let block_count = (self.span_pages + block_size - 1) / block_size;
            let words = (block_count + 63) / 64;
            self.offsets[order] = offset;
            self.words[order] = words;
            offset += words;
        }
        debug_assert!(offset <= MAX_BITMAP_WORDS);
    }

    fn position(&self, order: usize, ppn: usize) -> Option<(usize, u64)> {
        if !self.enabled || order >= MAX_ORDER || ppn < self.base {
            return None;
        }
        let relative = ppn - self.base;
        if relative >= self.span_pages {
            return None;
        }
        let block = relative >> order;
        let word = block >> 6;
        if word >= self.words[order] {
            return None;
        }
        Some((self.offsets[order] + word, 1u64 << (block & 63)))
    }

    fn is_set(&self, order: usize, ppn: usize) -> bool {
        self.position(order, ppn)
            .map(|(word, mask)| self.bits[word] & mask != 0)
            .unwrap_or(false)
    }

    fn set(&mut self, order: usize, ppn: usize) {
        if let Some((word, mask)) = self.position(order, ppn) {
            debug_assert_eq!(self.bits[word] & mask, 0);
            self.bits[word] |= mask;
        }
    }

    fn clear(&mut self, order: usize, ppn: usize) {
        if let Some((word, mask)) = self.position(order, ppn) {
            debug_assert_ne!(self.bits[word] & mask, 0);
            self.bits[word] &= !mask;
        }
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
    free_bitmap: FreeBlockBitmap,
    free_pages: usize,
    allocated_pages: usize,
    /// Cumulative free-list nodes inspected while finding a buddy to merge.
    #[cfg(feature = "cosmos-meminfo")]
    free_scan_steps: usize,
    /// Cumulative number of buddy blocks split to satisfy an allocation.
    #[cfg(feature = "cosmos-meminfo")]
    split_ops: usize,
    /// Cumulative number of buddy blocks merged during deallocation.
    #[cfg(feature = "cosmos-meminfo")]
    merge_ops: usize,
    /// Number of buddy membership checks performed while deallocating.
    #[cfg(feature = "cosmos-meminfo")]
    buddy_search_calls: usize,
    /// Number of buddy membership checks that found a free buddy.
    #[cfg(feature = "cosmos-meminfo")]
    buddy_search_hits: usize,
    /// Number of buddy membership checks that found no free buddy.
    #[cfg(feature = "cosmos-meminfo")]
    buddy_search_misses: usize,
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
    const fn empty() -> Self {
        Self {
            start: 0,
            end: 0,
            regions: [PpnRegion::empty(); MAX_MANAGED_REGIONS],
            region_count: 0,
            free_list: [None; MAX_ORDER],
            free_bitmap: FreeBlockBitmap::empty(),
            free_pages: 0,
            allocated_pages: 0,
            #[cfg(feature = "cosmos-meminfo")]
            free_scan_steps: 0,
            #[cfg(feature = "cosmos-meminfo")]
            split_ops: 0,
            #[cfg(feature = "cosmos-meminfo")]
            merge_ops: 0,
            #[cfg(feature = "cosmos-meminfo")]
            buddy_search_calls: 0,
            #[cfg(feature = "cosmos-meminfo")]
            buddy_search_hits: 0,
            #[cfg(feature = "cosmos-meminfo")]
            buddy_search_misses: 0,
        }
    }

    pub fn init_from_bootinfo(&mut self, kernel_start: PhysPageNum, kernel_end: PhysPageNum) {
        self.reset();
        context::get().memblock().for_each_free_range(|region| {
            self.add_usable_region(region, kernel_start.0, kernel_end.0);
        });
        self.free_bitmap.reset(self.start, self.end);
        for index in 0..self.region_count {
            let region = self.regions[index];
            self.add_range(region.start, region.end);
        }
    }

    fn reset(&mut self) {
        self.start = usize::MAX;
        self.end = 0;
        self.regions = [PpnRegion::empty(); MAX_MANAGED_REGIONS];
        self.region_count = 0;
        self.free_list = [None; MAX_ORDER];
        self.free_pages = 0;
        self.allocated_pages = 0;
        #[cfg(feature = "cosmos-meminfo")]
        {
            self.free_scan_steps = 0;
            self.split_ops = 0;
            self.merge_ops = 0;
            self.buddy_search_calls = 0;
            self.buddy_search_hits = 0;
            self.buddy_search_misses = 0;
        }
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

    fn set_previous(ppn: usize, previous: Option<usize>) {
        let previous = previous.unwrap_or(INVALID_PPN);
        let link = PhysPageNum(ppn).get_mut::<usize>() as *mut usize;
        unsafe { *link.add(1) = previous };
    }

    fn next(ppn: usize) -> Option<usize> {
        let next = *PhysPageNum(ppn).get_mut::<usize>();
        if next == INVALID_PPN {
            None
        } else {
            Some(next)
        }
    }

    fn previous(ppn: usize) -> Option<usize> {
        let link = PhysPageNum(ppn).get_mut::<usize>() as *mut usize;
        let previous = unsafe { *link.add(1) };
        if previous == INVALID_PPN {
            None
        } else {
            Some(previous)
        }
    }

    fn push_block(&mut self, order: usize, ppn: usize) {
        debug_assert!(order < MAX_ORDER);
        debug_assert_eq!(ppn & ((1usize << order) - 1), 0);
        debug_assert!(!self.free_bitmap.is_set(order, ppn));
        Self::set_next(ppn, self.free_list[order]);
        Self::set_previous(ppn, None);
        if let Some(head) = self.free_list[order] {
            Self::set_previous(head, Some(ppn));
        }
        self.free_list[order] = Some(ppn);
        self.free_bitmap.set(order, ppn);
    }

    fn pop_block(&mut self, order: usize) -> Option<usize> {
        let ppn = self.free_list[order]?;
        let next = Self::next(ppn);
        self.free_list[order] = next;
        if let Some(next) = next {
            Self::set_previous(next, None);
        }
        Self::set_next(ppn, None);
        Self::set_previous(ppn, None);
        self.free_bitmap.clear(order, ppn);
        Some(ppn)
    }

    fn remove_block(&mut self, order: usize, target: usize) -> bool {
        #[cfg(feature = "cosmos-meminfo")]
        {
            self.buddy_search_calls += 1;
        }
        if self.free_bitmap.enabled {
            if !self.free_bitmap.is_set(order, target) {
                #[cfg(feature = "cosmos-meminfo")]
                {
                    self.buddy_search_misses += 1;
                }
                return false;
            }
            #[cfg(feature = "cosmos-meminfo")]
            {
                self.buddy_search_hits += 1;
            }
            let previous = Self::previous(target);
            let next = Self::next(target);
            if let Some(previous) = previous {
                Self::set_next(previous, next);
            } else {
                debug_assert_eq!(self.free_list[order], Some(target));
                self.free_list[order] = next;
            }
            if let Some(next) = next {
                Self::set_previous(next, previous);
            }
            Self::set_next(target, None);
            Self::set_previous(target, None);
            self.free_bitmap.clear(order, target);
            return true;
        }

        let mut current = self.free_list[order];
        let mut previous = None;
        while let Some(ppn) = current {
            #[cfg(feature = "cosmos-meminfo")]
            {
                self.free_scan_steps += 1;
            }
            let next = Self::next(ppn);
            if ppn == target {
                if let Some(previous) = previous {
                    Self::set_next(previous, next);
                } else {
                    self.free_list[order] = next;
                }
                if let Some(next) = next {
                    Self::set_previous(next, previous);
                }
                Self::set_next(ppn, None);
                Self::set_previous(ppn, None);
                #[cfg(feature = "cosmos-meminfo")]
                {
                    self.buddy_search_hits += 1;
                }
                return true;
            }
            previous = current;
            current = next;
        }
        #[cfg(feature = "cosmos-meminfo")]
        {
            self.buddy_search_misses += 1;
        }
        false
    }

    fn contains_free_block(&self, ppn: usize) -> bool {
        if self.free_bitmap.enabled {
            for order in 0..MAX_ORDER {
                let block_size = 1usize << order;
                let block = ppn & !(block_size - 1);
                if self.free_bitmap.is_set(order, block) {
                    return true;
                }
            }
            return false;
        }
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
            #[cfg(feature = "cosmos-meminfo")]
            {
                self.split_ops += 1;
            }
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
            #[cfg(feature = "cosmos-meminfo")]
            {
                self.merge_ops += 1;
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
        Self::empty()
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
    /// Number of single-frame allocation attempts.
    #[cfg(feature = "cosmos-meminfo")]
    pub alloc_calls: usize,
    /// Number of single-frame deallocations.
    #[cfg(feature = "cosmos-meminfo")]
    pub dealloc_calls: usize,
    /// Number of contiguous allocation attempts.
    #[cfg(feature = "cosmos-meminfo")]
    pub contiguous_alloc_calls: usize,
    /// Number of contiguous range deallocations.
    #[cfg(feature = "cosmos-meminfo")]
    pub range_dealloc_calls: usize,
    /// Cumulative free-list nodes inspected while finding a buddy to merge.
    #[cfg(feature = "cosmos-meminfo")]
    pub free_scan_steps: usize,
    /// Cumulative number of buddy blocks split to satisfy an allocation.
    #[cfg(feature = "cosmos-meminfo")]
    pub split_ops: usize,
    /// Cumulative number of buddy blocks merged during deallocation.
    #[cfg(feature = "cosmos-meminfo")]
    pub merge_ops: usize,
    /// Number of buddy membership checks performed while deallocating.
    #[cfg(feature = "cosmos-meminfo")]
    pub buddy_search_calls: usize,
    /// Number of buddy membership checks that found a free buddy.
    #[cfg(feature = "cosmos-meminfo")]
    pub buddy_search_hits: usize,
    /// Number of buddy membership checks that found no free buddy.
    #[cfg(feature = "cosmos-meminfo")]
    pub buddy_search_misses: usize,
    /// Whether the O(1) per-order free-block bitmap is active.
    #[cfg(feature = "cosmos-meminfo")]
    pub bitmap_enabled: bool,
    /// Cumulative timer ticks spent acquiring the frame allocator lock.
    /// This includes the local interrupt save/restore overhead around lock
    /// acquisition, but excludes the allocator operation after acquisition.
    #[cfg(feature = "cosmos-meminfo")]
    pub lock_wait_ticks: usize,
    /// Number of pages cleared before allocation.
    #[cfg(feature = "cosmos-meminfo")]
    pub zeroed_pages: usize,
    /// Number of bytes cleared before allocation.
    #[cfg(feature = "cosmos-meminfo")]
    pub zeroed_bytes: usize,
    /// Cumulative timer ticks spent clearing pages.
    #[cfg(feature = "cosmos-meminfo")]
    pub zero_time_ticks: usize,
    /// Number of allocations served from a per-CPU frame cache.
    #[cfg(feature = "cosmos-meminfo")]
    pub per_cpu_cache_hits: usize,
    /// Number of allocations that fell through a per-CPU frame cache.
    #[cfg(feature = "cosmos-meminfo")]
    pub per_cpu_cache_misses: usize,
    /// Whether a per-CPU frame cache is currently enabled.
    #[cfg(feature = "cosmos-meminfo")]
    pub per_cpu_cache_enabled: bool,
    /// Number of free pages currently held outside the global buddy lists.
    #[cfg(feature = "cosmos-meminfo")]
    pub per_cpu_cached_pages: usize,
}

pub static FRAME_ALLOCATOR: SpinNoIrqLock<FrameAllocatorImpl> =
    SpinNoIrqLock::new(FrameAllocatorImpl::empty());

lazy_static! {
    static ref PER_CPU_FRAME_CACHES: [SpinNoIrqLock<PerCpuFrameCache>; MAX_HARTS] =
        core::array::from_fn(|_| SpinNoIrqLock::new(PerCpuFrameCache::new()));
}

pub fn init_frame_allocator() {
    extern "C" {
        fn skernel();
        fn ekernel();
    }
    let kernel_start = PhysPageNum(phys_addr_floor_ppn(virt_to_phys(skernel as usize)));
    let kernel_end = PhysPageNum(phys_addr_ceil_ppn(virt_to_phys(ekernel as usize)));
    FRAME_PER_CPU_CACHE_ENABLED.store(false, Ordering::Release);
    let mut allocator = FRAME_ALLOCATOR.lock();
    allocator.init_from_bootinfo(kernel_start, kernel_end);
    drop(allocator);
    for cache in PER_CPU_FRAME_CACHES.iter() {
        cache.lock().clear();
    }
    FRAME_PER_CPU_CACHED_PAGES.store(0, Ordering::Release);
    FRAME_PER_CPU_CACHE_ENABLED.store(PER_CPU_FRAME_CACHE_DEFAULT_ENABLED, Ordering::Release);
    FRAME_ALLOC_OOM_COUNT.store(0, Ordering::Release);
    #[cfg(feature = "cosmos-meminfo")]
    {
        FRAME_ALLOC_CALLS.store(0, Ordering::Release);
        FRAME_DEALLOC_CALLS.store(0, Ordering::Release);
        FRAME_CONTIGUOUS_ALLOC_CALLS.store(0, Ordering::Release);
        FRAME_RANGE_DEALLOC_CALLS.store(0, Ordering::Release);
        FRAME_ALLOCATOR_LOCK_WAIT_TICKS.store(0, Ordering::Release);
        FRAME_ZEROED_PAGES.store(0, Ordering::Release);
        FRAME_ZEROED_BYTES.store(0, Ordering::Release);
        FRAME_ZERO_TIME_TICKS.store(0, Ordering::Release);
        FRAME_PER_CPU_CACHE_HITS.store(0, Ordering::Release);
        FRAME_PER_CPU_CACHE_MISSES.store(0, Ordering::Release);
    }
}

/// Return runtime statistics of the frame allocator.
pub fn frame_allocator_stats() -> FrameAllocatorStats {
    let allocator = FRAME_ALLOCATOR.lock();
    let per_cpu_cached_pages = FRAME_PER_CPU_CACHED_PAGES.load(Ordering::Acquire);
    let free_pages = allocator.free_pages.saturating_add(per_cpu_cached_pages);
    let allocated_pages = allocator
        .allocated_pages
        .saturating_sub(per_cpu_cached_pages);
    FrameAllocatorStats {
        free_pages,
        allocated_pages,
        total_pages: free_pages + allocated_pages,
        oom_count: FRAME_ALLOC_OOM_COUNT.load(Ordering::Acquire),
        #[cfg(feature = "cosmos-meminfo")]
        alloc_calls: FRAME_ALLOC_CALLS.load(Ordering::Acquire),
        #[cfg(feature = "cosmos-meminfo")]
        dealloc_calls: FRAME_DEALLOC_CALLS.load(Ordering::Acquire),
        #[cfg(feature = "cosmos-meminfo")]
        contiguous_alloc_calls: FRAME_CONTIGUOUS_ALLOC_CALLS.load(Ordering::Acquire),
        #[cfg(feature = "cosmos-meminfo")]
        range_dealloc_calls: FRAME_RANGE_DEALLOC_CALLS.load(Ordering::Acquire),
        #[cfg(feature = "cosmos-meminfo")]
        free_scan_steps: allocator.free_scan_steps,
        #[cfg(feature = "cosmos-meminfo")]
        split_ops: allocator.split_ops,
        #[cfg(feature = "cosmos-meminfo")]
        merge_ops: allocator.merge_ops,
        #[cfg(feature = "cosmos-meminfo")]
        buddy_search_calls: allocator.buddy_search_calls,
        #[cfg(feature = "cosmos-meminfo")]
        buddy_search_hits: allocator.buddy_search_hits,
        #[cfg(feature = "cosmos-meminfo")]
        buddy_search_misses: allocator.buddy_search_misses,
        #[cfg(feature = "cosmos-meminfo")]
        bitmap_enabled: allocator.free_bitmap.enabled,
        #[cfg(feature = "cosmos-meminfo")]
        lock_wait_ticks: FRAME_ALLOCATOR_LOCK_WAIT_TICKS.load(Ordering::Acquire),
        #[cfg(feature = "cosmos-meminfo")]
        zeroed_pages: FRAME_ZEROED_PAGES.load(Ordering::Acquire),
        #[cfg(feature = "cosmos-meminfo")]
        zeroed_bytes: FRAME_ZEROED_BYTES.load(Ordering::Acquire),
        #[cfg(feature = "cosmos-meminfo")]
        zero_time_ticks: FRAME_ZERO_TIME_TICKS.load(Ordering::Acquire),
        #[cfg(feature = "cosmos-meminfo")]
        per_cpu_cache_hits: FRAME_PER_CPU_CACHE_HITS.load(Ordering::Acquire),
        #[cfg(feature = "cosmos-meminfo")]
        per_cpu_cache_misses: FRAME_PER_CPU_CACHE_MISSES.load(Ordering::Acquire),
        #[cfg(feature = "cosmos-meminfo")]
        per_cpu_cache_enabled: FRAME_PER_CPU_CACHE_ENABLED.load(Ordering::Acquire),
        #[cfg(feature = "cosmos-meminfo")]
        per_cpu_cached_pages,
    }
}

#[inline]
#[cfg(feature = "cosmos-meminfo")]
fn frame_allocator_lock_start() -> usize {
    Plat::read_time()
}

#[inline]
#[cfg(feature = "cosmos-meminfo")]
fn record_frame_allocator_lock_wait(start: usize) {
    FRAME_ALLOCATOR_LOCK_WAIT_TICKS
        .fetch_add(Plat::read_time().wrapping_sub(start), Ordering::Relaxed);
}

#[inline]
fn local_frame_cache_index() -> usize {
    hartid() % MAX_HARTS
}

fn try_pop_local_frame() -> Option<PhysPageNum> {
    if !FRAME_PER_CPU_CACHE_ENABLED.load(Ordering::Acquire) {
        return None;
    }
    let ppn = PER_CPU_FRAME_CACHES[local_frame_cache_index()]
        .lock()
        .pop()?;
    FRAME_PER_CPU_CACHED_PAGES.fetch_sub(1, Ordering::AcqRel);
    #[cfg(feature = "cosmos-meminfo")]
    FRAME_PER_CPU_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
    Some(ppn)
}

fn try_steal_cached_frame() -> Option<PhysPageNum> {
    if !FRAME_PER_CPU_CACHE_ENABLED.load(Ordering::Acquire) {
        return None;
    }
    let local = local_frame_cache_index();
    for offset in 1..=MAX_HARTS {
        let index = (local + offset) % MAX_HARTS;
        if let Some(ppn) = PER_CPU_FRAME_CACHES[index].lock().pop() {
            FRAME_PER_CPU_CACHED_PAGES.fetch_sub(1, Ordering::AcqRel);
            #[cfg(feature = "cosmos-meminfo")]
            FRAME_PER_CPU_CACHE_HITS.fetch_add(1, Ordering::Relaxed);
            return Some(ppn);
        }
    }
    None
}

/// Keep recently released order-0 frames local.  When one cache fills, return
/// half of it to the buddy allocator in a single global-lock acquisition so
/// contiguous allocations and remote harts still make forward progress.
fn try_push_local_frame(ppn: PhysPageNum) -> bool {
    if !FRAME_PER_CPU_CACHE_ENABLED.load(Ordering::Acquire) {
        return false;
    }

    let mut flushed = [INVALID_PPN; PER_CPU_FRAME_CACHE_FLUSH];
    let flushed_count = {
        let mut cache = PER_CPU_FRAME_CACHES[local_frame_cache_index()].lock();
        let mut count = 0;
        if cache.len == PER_CPU_FRAME_CACHE_CAPACITY {
            while count < flushed.len() {
                flushed[count] = cache
                    .pop()
                    .expect("full per-CPU frame cache became empty")
                    .0;
                count += 1;
            }
        }
        assert!(cache.push(ppn), "per-CPU frame cache push must fit");
        count
    };

    FRAME_PER_CPU_CACHED_PAGES.fetch_add(1, Ordering::AcqRel);
    if flushed_count != 0 {
        FRAME_PER_CPU_CACHED_PAGES.fetch_sub(flushed_count, Ordering::AcqRel);
        #[cfg(feature = "cosmos-meminfo")]
        let lock_start = frame_allocator_lock_start();
        let mut allocator = FRAME_ALLOCATOR.lock();
        #[cfg(feature = "cosmos-meminfo")]
        record_frame_allocator_lock_wait(lock_start);
        for raw_ppn in &flushed[..flushed_count] {
            allocator.dealloc(PhysPageNum(*raw_ppn));
        }
    }
    true
}

/// Return cached order-0 pages to the buddy lists before reporting that a
/// larger contiguous allocation cannot be satisfied.
fn drain_per_cpu_frame_caches() -> usize {
    if !FRAME_PER_CPU_CACHE_ENABLED.load(Ordering::Acquire) {
        return 0;
    }
    let mut total = 0usize;
    for cache in PER_CPU_FRAME_CACHES.iter() {
        let mut drained = [INVALID_PPN; PER_CPU_FRAME_CACHE_CAPACITY];
        let count = {
            let mut cache = cache.lock();
            let mut count = 0;
            while let Some(ppn) = cache.pop() {
                drained[count] = ppn.0;
                count += 1;
            }
            count
        };
        if count == 0 {
            continue;
        }
        FRAME_PER_CPU_CACHED_PAGES.fetch_sub(count, Ordering::AcqRel);
        #[cfg(feature = "cosmos-meminfo")]
        let lock_start = frame_allocator_lock_start();
        let mut allocator = FRAME_ALLOCATOR.lock();
        #[cfg(feature = "cosmos-meminfo")]
        record_frame_allocator_lock_wait(lock_start);
        for raw_ppn in &drained[..count] {
            allocator.dealloc(PhysPageNum(*raw_ppn));
        }
        total += count;
    }
    total
}

/// Allocate a physical page frame in FrameTracker style
pub fn frame_alloc() -> Option<FrameTracker> {
    #[cfg(feature = "cosmos-meminfo")]
    {
        FRAME_ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(ppn) = try_pop_local_frame() {
        return Some(FrameTracker::new(ppn));
    }
    #[cfg(feature = "cosmos-meminfo")]
    {
        FRAME_PER_CPU_CACHE_MISSES.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(feature = "cosmos-meminfo")]
    let lock_start = frame_allocator_lock_start();
    let mut allocator = FRAME_ALLOCATOR.lock();
    #[cfg(feature = "cosmos-meminfo")]
    {
        record_frame_allocator_lock_wait(lock_start);
    }
    let mut refill_start = None;
    let ppn = if FRAME_PER_CPU_CACHE_ENABLED.load(Ordering::Acquire) {
        match allocator.alloc_order(PER_CPU_FRAME_CACHE_REFILL_ORDER) {
            Some(start) => {
                refill_start = Some(start);
                Some(start)
            }
            None => allocator.alloc(),
        }
    } else {
        allocator.alloc()
    };
    drop(allocator);
    if let Some(start) = refill_start {
        let mut cached = 0usize;
        let mut overflow = [INVALID_PPN; PER_CPU_FRAME_CACHE_REFILL_PAGES - 1];
        let mut overflow_count = 0usize;
        {
            let mut cache = PER_CPU_FRAME_CACHES[local_frame_cache_index()].lock();
            for raw_ppn in start.0 + 1..start.0 + PER_CPU_FRAME_CACHE_REFILL_PAGES {
                if cache.push(PhysPageNum(raw_ppn)) {
                    cached += 1;
                } else {
                    overflow[overflow_count] = raw_ppn;
                    overflow_count += 1;
                }
            }
        }
        FRAME_PER_CPU_CACHED_PAGES.fetch_add(cached, Ordering::AcqRel);
        if overflow_count != 0 {
            #[cfg(feature = "cosmos-meminfo")]
            let lock_start = frame_allocator_lock_start();
            let mut allocator = FRAME_ALLOCATOR.lock();
            #[cfg(feature = "cosmos-meminfo")]
            record_frame_allocator_lock_wait(lock_start);
            for raw_ppn in &overflow[..overflow_count] {
                allocator.dealloc(PhysPageNum(*raw_ppn));
            }
        }
    }
    ppn.or_else(try_steal_cached_frame)
        .map(FrameTracker::new)
        .or_else(|| {
            FRAME_ALLOC_OOM_COUNT.fetch_add(1, Ordering::AcqRel);
            let frame_allocator_stats = frame_allocator_stats();
            // Keep the page-cache guard's lifetime bounded to this block.  Using
            // PAGE_CACHE_MANAGER.lock() once per format argument keeps the first
            // SpinNoIrqLockGuard alive until the end of the error! statement;
            // the second call then spins forever on the same lock (especially
            // because interrupts are disabled while acquiring it).
            let (cached_pages, low_watermark, high_watermark) = {
                let manager = PAGE_CACHE_MANAGER.lock();
                (
                    manager.cached_pages,
                    manager.low_watermark,
                    manager.high_watermark,
                )
            };
            error!(
                "frame_alloc: out of memory (free={} cached={} low={} high={} total={})",
                frame_allocator_stats.free_pages,
                cached_pages,
                low_watermark,
                high_watermark,
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
    crate::fs::reclaim_for_frame_allocation()
}

/// Deallocate a physical page frame with a given ppn
pub fn frame_dealloc(ppn: PhysPageNum) {
    #[cfg(feature = "cosmos-meminfo")]
    {
        FRAME_DEALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    if try_push_local_frame(ppn) {
        return;
    }
    #[cfg(feature = "cosmos-meminfo")]
    let lock_start = frame_allocator_lock_start();
    let mut allocator = FRAME_ALLOCATOR.lock();
    #[cfg(feature = "cosmos-meminfo")]
    {
        record_frame_allocator_lock_wait(lock_start);
    }
    allocator.dealloc(ppn);
}

/// Allocate a physically contiguous frame range.
/// Simplified implmentation: maybe fail when align_pages > pages (require over-alignment)
pub fn frame_alloc_contiguous(pages: usize, align_pages: usize) -> Option<ContiguousFrames> {
    #[cfg(feature = "cosmos-meminfo")]
    {
        FRAME_CONTIGUOUS_ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    if pages == 0 || align_pages == 0 || !pages.is_power_of_two() || !align_pages.is_power_of_two()
    {
        return None;
    }
    let order = pages.trailing_zeros() as usize;
    let allocate_order = || {
        #[cfg(feature = "cosmos-meminfo")]
        let lock_start = frame_allocator_lock_start();
        let mut allocator = FRAME_ALLOCATOR.lock();
        #[cfg(feature = "cosmos-meminfo")]
        {
            record_frame_allocator_lock_wait(lock_start);
        }
        allocator.alloc_order(order)
    };
    let start = allocate_order().or_else(|| {
        (drain_per_cpu_frame_caches() != 0)
            .then(allocate_order)
            .flatten()
    })?;
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
    #[cfg(feature = "cosmos-meminfo")]
    {
        FRAME_RANGE_DEALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(feature = "cosmos-meminfo")]
    let lock_start = frame_allocator_lock_start();
    let mut allocator = FRAME_ALLOCATOR.lock();
    #[cfg(feature = "cosmos-meminfo")]
    {
        record_frame_allocator_lock_wait(lock_start);
    }
    allocator.dealloc_order(start, pages.trailing_zeros() as usize);
}

fn clear_frame(ppn: PhysPageNum) {
    #[cfg(feature = "cosmos-meminfo")]
    let start = Plat::read_time();
    for byte in ppn.get_bytes_array() {
        *byte = 0;
    }
    #[cfg(feature = "cosmos-meminfo")]
    {
        FRAME_ZEROED_PAGES.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(feature = "cosmos-meminfo")]
    {
        FRAME_ZEROED_BYTES.fetch_add(PAGE_SIZE, Ordering::Relaxed);
    }
    #[cfg(feature = "cosmos-meminfo")]
    {
        FRAME_ZERO_TIME_TICKS.fetch_add(Plat::read_time().wrapping_sub(start), Ordering::Relaxed);
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
