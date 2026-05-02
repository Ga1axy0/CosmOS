//! The heap allocator.

use super::frame_allocator::frame_alloc_contiguous_for_heap;
use crate::config::{KERNEL_HEAP_SIZE, MEMORY_END, PAGE_SIZE};
use buddy_system_allocator::LockedHeap;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use core::sync::atomic::{AtomicUsize, Ordering};

#[global_allocator]
static HEAP_ALLOCATOR: KernelHeapAllocator = KernelHeapAllocator::new();

const KERNEL_HEAP_INITIAL_PAGES: usize = 64;
const KERNEL_HEAP_GROW_SIZE: usize = KERNEL_HEAP_INITIAL_PAGES * PAGE_SIZE;

static KERNEL_HEAP_BYTES: AtomicUsize = AtomicUsize::new(0);

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
        let Some(bytes) = reserve_heap_bytes(required_bytes) else {
            return false;
        };
        let pages = bytes / PAGE_SIZE;
        let Some(start) = frame_alloc_contiguous_for_heap(pages) else {
            KERNEL_HEAP_BYTES.fetch_sub(bytes, Ordering::AcqRel);
            return false;
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
        HEAP_ALLOCATOR.grow(KERNEL_HEAP_GROW_SIZE),
        "failed to initialize kernel heap"
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

fn reserve_heap_bytes(required_bytes: usize) -> Option<usize> {
    let required_bytes = align_up_to_page(required_bytes)?;
    loop {
        let used = KERNEL_HEAP_BYTES.load(Ordering::Acquire);
        let remaining = KERNEL_HEAP_SIZE.checked_sub(used)?;
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
    let frame_backed_range = ekernel as usize..MEMORY_END;
    let a = Box::new(5);
    assert_eq!(*a, 5);
    let a_ptr = a.as_ref() as *const _ as usize;
    assert!(!bss_range.contains(&a_ptr));
    assert!(frame_backed_range.contains(&a_ptr));
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
    assert!(frame_backed_range.contains(&v_ptr));
    drop(v);
    println!("heap_test passed!");
}
