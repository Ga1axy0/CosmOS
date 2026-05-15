//! The heap allocator.

use super::frame_allocator::{frame_alloc, frame_alloc_contiguous, frame_dealloc};
use super::{VirtAddr, KERNEL_SPACE};
use crate::config::{KERNEL_HEAP_BASE, MAX_KERNEL_HEAP_SIZE, MEMORY_END, PAGE_SIZE};
use buddy_system_allocator::LockedHeap;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[global_allocator]
static HEAP_ALLOCATOR: KernelHeapAllocator = KernelHeapAllocator::new();

const KERNEL_HEAP_GROW_PAGES: usize = 64;
const KERNEL_HEAP_GROW_SIZE: usize = KERNEL_HEAP_GROW_PAGES * PAGE_SIZE;
const KERNEL_HEAP_BOOTSTRAP_PAGES: usize = 64;

pub static KERNEL_HEAP_BYTES: AtomicUsize = AtomicUsize::new(0);
static KERNEL_HEAP_VIRTUAL_BYTES: AtomicUsize = AtomicUsize::new(0);
static KERNEL_HEAP_VIRTUAL_READY: AtomicBool = AtomicBool::new(false);

struct KernelHeapAllocator {
    heap: LockedHeap,
}

impl KernelHeapAllocator {
    const fn new() -> Self {
        Self {
            heap: LockedHeap::empty(),
        }
    }

    fn grow(&self, required_bytes: usize) -> bool {
        let (start, bytes) = if KERNEL_HEAP_VIRTUAL_READY.load(Ordering::Acquire) {
            let Some((virtual_offset, bytes)) = reserve_virtual_heap_bytes(required_bytes) else {
                return false;
            };
            // debug!(
            //     "Growing virtual kernel heap: {} KiB -> {} KiB",
            //     virtual_offset / 1024,
            //     (virtual_offset + bytes) / 1024
            // );
            let start = KERNEL_HEAP_BASE + virtual_offset;
            if !map_heap_pages(start, bytes / PAGE_SIZE) {
                KERNEL_HEAP_VIRTUAL_BYTES.fetch_sub(bytes, Ordering::AcqRel);
                KERNEL_HEAP_BYTES.fetch_sub(bytes, Ordering::AcqRel);
                return false;
            }
            (start, bytes)
        } else {
            let Some(bytes) = reserve_bootstrap_heap_bytes(required_bytes) else {
                return false;
            };
            // debug!(
            //     "Growing bootstrap kernel heap: +{} KiB, total {} KiB",
            //     bytes / 1024,
            //     KERNEL_HEAP_BYTES.load(Ordering::Acquire) / 1024
            // );
            let Some(start) = alloc_bootstrap_heap_pages(bytes / PAGE_SIZE) else {
                KERNEL_HEAP_BYTES.fetch_sub(bytes, Ordering::AcqRel);
                return false;
            };
            (start, bytes)
        };
        unsafe {
            self.heap.lock().add_to_heap(start, start + bytes);
        }
        true
    }
}

unsafe impl GlobalAlloc for KernelHeapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if let Ok(allocation) = self.heap.lock().alloc(layout) {
            return allocation.as_ptr();
        }
        let Some(required_bytes) = layout_required_bytes(layout) else {
            return null_mut();
        };
        loop {
            // debug!("Heap allocation {layout:?} failed, trying to grow heap: required_bytes = {required_bytes}");
            if !self.grow(required_bytes) {
                return null_mut();
            }
            if let Ok(allocation) = self.heap.lock().alloc(layout) {
                return allocation.as_ptr();
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.heap
            .lock()
            .dealloc(core::ptr::NonNull::new_unchecked(ptr), layout)
    }
}

#[alloc_error_handler]
pub fn handle_alloc_error(layout: core::alloc::Layout) -> ! {
    panic!("Heap allocation error, layout = {:?}", layout);
}

pub fn init_heap() {
    assert!(
        HEAP_ALLOCATOR.grow(KERNEL_HEAP_BOOTSTRAP_PAGES * PAGE_SIZE),
        "failed to initialize kernel heap"
    );
}

pub fn init_heap_virtual_window() {
    KERNEL_HEAP_VIRTUAL_READY.store(true, Ordering::Release);
    assert!(
        HEAP_ALLOCATOR.grow(KERNEL_HEAP_GROW_SIZE),
        "failed to initialize virtual kernel heap"
    );
}

fn layout_required_bytes(layout: Layout) -> Option<usize> {
    let min_size = layout
        .size()
        .max(layout.align())
        .max(core::mem::size_of::<usize>());
    let class_size = min_size.checked_next_power_of_two()?;
    align_up_to_page(class_size)
}

fn align_up_to_page(value: usize) -> Option<usize> {
    value
        .checked_add(PAGE_SIZE - 1)
        .map(|value| value & !(PAGE_SIZE - 1))
}

fn reserve_bootstrap_heap_bytes(required_bytes: usize) -> Option<usize> {
    let required_bytes = align_up_to_page(required_bytes)?;
    loop {
        let used = KERNEL_HEAP_BYTES.load(Ordering::Acquire);
        let remaining = MAX_KERNEL_HEAP_SIZE.checked_sub(used)?;
        if remaining < required_bytes {
            return None;
        }
        let grow_bytes = KERNEL_HEAP_GROW_SIZE.max(required_bytes).min(remaining);
        if KERNEL_HEAP_BYTES
            .compare_exchange(used, used + grow_bytes, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(grow_bytes);
        }
    }
}

fn reserve_virtual_heap_bytes(required_bytes: usize) -> Option<(usize, usize)> {
    let required_bytes = align_up_to_page(required_bytes)?;
    loop {
        let used = KERNEL_HEAP_VIRTUAL_BYTES.load(Ordering::Acquire);
        let aligned_block_end = align_up(used, required_bytes)?.checked_add(required_bytes)?;
        let normal_grow_end = used.checked_add(KERNEL_HEAP_GROW_SIZE.max(required_bytes))?;
        let new_used = aligned_block_end.max(normal_grow_end);
        if new_used > MAX_KERNEL_HEAP_SIZE {
            return None;
        }
        let bytes = new_used - used;
        if KERNEL_HEAP_VIRTUAL_BYTES
            .compare_exchange(used, new_used, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            KERNEL_HEAP_BYTES.fetch_add(bytes, Ordering::AcqRel);
            return Some((used, bytes));
        }
    }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    let mask = align.checked_sub(1)?;
    Some(value.checked_add(mask)? & !mask)
}

fn alloc_bootstrap_heap_pages(pages: usize) -> Option<usize> {
    let frames = frame_alloc_contiguous(pages, pages)?;
    let first = frames.start_ppn();
    core::mem::forget(frames);
    let start_pa: super::PhysAddr = first.into();
    Some(start_pa.into())
}

fn map_heap_pages(start_va: usize, pages: usize) -> bool {
    let mut kernel_space = KERNEL_SPACE.lock();
    for page in 0..pages {
        let va = start_va + page * PAGE_SIZE;
        let vpn = VirtAddr::from(va).floor();
        if kernel_space.page_table.translate(vpn).is_some() {
            rollback_heap_pages(&mut kernel_space.page_table, start_va, page);
            return false;
        }
        let Some(frame) = frame_alloc() else {
            rollback_heap_pages(&mut kernel_space.page_table, start_va, page);
            return false;
        };
        let ppn = frame.ppn;
        kernel_space.page_table.map_kernel_untracked(
            vpn,
            ppn,
            super::page_table::PTEFlags::R | super::page_table::PTEFlags::W,
        );
        core::mem::forget(frame);
    }
    unsafe {
        core::arch::asm!("sfence.vma");
    }
    true
}

fn rollback_heap_pages(page_table: &mut super::PageTable, start_va: usize, pages: usize) {
    for page in 0..pages {
        let va = start_va + page * PAGE_SIZE;
        let vpn = VirtAddr::from(va).floor();
        if let Some(pte) = page_table.clear(vpn) {
            frame_dealloc(pte.ppn());
        }
    }
    unsafe {
        core::arch::asm!("sfence.vma");
    }
}

#[allow(unused)]
pub fn heap_test() {
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    extern "C" {
        fn sbss();
        fn ebss();
        fn ekernel();
    }
    let bss_range = sbss as usize..ebss as usize;
    let bootstrap_frame_backed_range = ekernel as usize..MEMORY_END;
    let virtual_heap_range = KERNEL_HEAP_BASE..KERNEL_HEAP_BASE + MAX_KERNEL_HEAP_SIZE;
    let a = Box::new(5);
    assert_eq!(*a, 5);
    let a_ptr = a.as_ref() as *const _ as usize;
    assert!(!bss_range.contains(&a_ptr));
    assert!(bootstrap_frame_backed_range.contains(&a_ptr) || virtual_heap_range.contains(&a_ptr));
    drop(a);
    let mut v: Vec<usize> = Vec::new();
    for i in 0..500 {
        v.push(i);
    }
    for (i, val) in v.iter().take(500).enumerate() {
        assert_eq!(*val, i);
    }
    let v_ptr = v.as_ptr() as usize;
    assert!(!bss_range.contains(&v_ptr));
    assert!(bootstrap_frame_backed_range.contains(&v_ptr) || virtual_heap_range.contains(&v_ptr));
    drop(v);
    println!("heap_test passed!");
}
