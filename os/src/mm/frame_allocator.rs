//! Physical page frame allocator

use super::{PhysAddr, PhysPageNum};
use crate::fs::PAGE_CACHE_MANAGER;
use crate::{config::MEMORY_END, sync::SpinNoIrqLock};
use core::fmt::{self, Debug, Formatter};
use lazy_static::*;

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

trait FrameAllocator {
    fn new() -> Self;
    fn alloc(&mut self) -> Option<PhysPageNum>;
    fn dealloc(&mut self, ppn: PhysPageNum);
}

pub struct StackFrameAllocator {
    start: usize,
    current: usize,
    end: usize,
    recycled_head: Option<usize>,
    recycled_count: usize,
}

impl StackFrameAllocator {
    pub fn init(&mut self, l: PhysPageNum, r: PhysPageNum) {
        self.start = l.0;
        self.current = l.0;
        self.end = r.0;
        self.recycled_head = None;
        self.recycled_count = 0;
        // trace!("last {} Physical Frames.", self.end - self.current);
    }

    fn set_recycled_next(ppn: usize, next: Option<usize>) {
        let next = next.unwrap_or(usize::MAX);
        *PhysPageNum(ppn).get_mut::<usize>() = next;
    }

    fn recycled_next(ppn: usize) -> Option<usize> {
        let next = *PhysPageNum(ppn).get_mut::<usize>();
        if next == usize::MAX {
            None
        } else {
            Some(next)
        }
    }

    fn recycled_contains(&self, ppn: usize) -> bool {
        let mut current = self.recycled_head;
        while let Some(recycled_ppn) = current {
            if recycled_ppn == ppn {
                return true;
            }
            current = Self::recycled_next(recycled_ppn);
        }
        false
    }

    fn recycle_raw(&mut self, ppn: usize) {
        Self::set_recycled_next(ppn, self.recycled_head);
        self.recycled_head = Some(ppn);
        self.recycled_count += 1;
    }

    fn alloc_contiguous_aligned(
        &mut self,
        pages: usize,
        align_pages: usize,
    ) -> Option<PhysPageNum> {
        if pages == 0 || align_pages == 0 {
            return None;
        }
        let aligned_current = align_up(self.current, align_pages)?;
        if self.end.saturating_sub(aligned_current) < pages {
            return None;
        }
        for ppn in self.current..aligned_current {
            self.recycle_raw(ppn);
        }
        let start = aligned_current;
        self.current = aligned_current + pages;
        Some(start.into())
    }
}
impl FrameAllocator for StackFrameAllocator {
    fn new() -> Self {
        Self {
            start: 0,
            current: 0,
            end: 0,
            recycled_head: None,
            recycled_count: 0,
        }
    }
    fn alloc(&mut self) -> Option<PhysPageNum> {
        trace!(
            "FrameAllocator: Used {} | PageCache {} | Free {} | Recycled {}",
            self.current - self.start - self.recycled_count,
            PAGE_CACHE_MANAGER.lock().cached_pages,
            self.end - self.current,
            self.recycled_count
        );
        if let Some(ppn) = self.recycled_head {
            self.recycled_head = Self::recycled_next(ppn);
            self.recycled_count -= 1;
            Some(ppn.into())
        } else if self.current == self.end {
            None
        } else {
            self.current += 1;
            Some((self.current - 1).into())
        }
    }
    fn dealloc(&mut self, ppn: PhysPageNum) {
        let ppn = ppn.0;
        // validity check
        if ppn < self.start || ppn >= self.current || self.recycled_contains(ppn) {
            panic!("Frame ppn={:#x} has not been allocated!", ppn);
        }
        // recycle
        Self::set_recycled_next(ppn, self.recycled_head);
        self.recycled_head = Some(ppn);
        self.recycled_count += 1;
    }
}

type FrameAllocatorImpl = StackFrameAllocator;

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

/// Allocate contiguous physical pages for the kernel heap.
///
/// These pages become permanently owned by the heap allocator, so they are not
/// wrapped in `FrameTracker` and will not be returned by dropping a tracker.
pub(super) fn frame_alloc_contiguous_for_heap(pages: usize) -> Option<usize> {
    let start_ppn = FRAME_ALLOCATOR
        .lock()
        .alloc_contiguous_aligned(pages, pages)?;
    for ppn in start_ppn.0..start_ppn.0 + pages {
        for byte in PhysPageNum(ppn).get_bytes_array() {
            *byte = 0;
        }
    }
    let start_pa: PhysAddr = start_ppn.into();
    Some(start_pa.into())
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    let mask = align.checked_sub(1)?;
    Some(value.checked_add(mask)? & !mask)
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
