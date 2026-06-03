//! The heap allocator.

use super::frame_allocator::{frame_alloc, frame_alloc_contiguous, frame_dealloc};
use super::page_table::PTEFlags;
use super::{PageTableEntry, PhysPageNum, VirtAddr, KERNEL_SPACE};
use crate::config::{KERNEL_HEAP_BASE, MAX_KERNEL_HEAP_SIZE, MEMORY_END, PAGE_SIZE};
use crate::sync::SpinNoIrqLock;
use buddy_system_allocator::LockedHeap;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use riscv::register::sstatus;

#[global_allocator]
static HEAP_ALLOCATOR: KernelHeapAllocator = KernelHeapAllocator::new();

/// RAII guard that disables supervisor interrupts (`sstatus.SIE`) for the
/// duration of a kernel-heap critical section, restoring the previous state on
/// drop.
///
/// The global allocator's lock (`buddy_system_allocator::LockedHeap`) is a
/// plain spinlock that does **not** mask interrupts. Kernel interrupt handlers
/// allocate from this same heap (the timer tick runs
/// `check_itimers_of_all_processes`, which collects a `Vec` of every process,
/// and `net::poll`). If a timer interrupt fires on a hart that is *already*
/// holding the heap lock in ordinary kernel code, the handler re-enters the
/// allocator and spins forever on the non-reentrant lock — a same-hart
/// self-deadlock that then wedges every other hart. Masking interrupts while
/// the heap lock is held closes that window, exactly like `SpinNoIrqLock`.
///
/// Only the brief buddy-lock holds are wrapped; `grow`'s page-mapping / TLB
/// shootdown work runs with interrupts in their normal state so cross-hart
/// IPIs are still serviced.
struct HeapIrqGuard {
    sie_was_enabled: bool,
}

impl HeapIrqGuard {
    #[inline]
    fn new() -> Self {
        let sie_was_enabled = sstatus::read().sie();
        unsafe { sstatus::clear_sie() };
        Self { sie_was_enabled }
    }
}

impl Drop for HeapIrqGuard {
    #[inline]
    fn drop(&mut self) {
        if self.sie_was_enabled {
            unsafe { sstatus::set_sie() };
        }
    }
}

const KERNEL_HEAP_GROW_PAGES: usize = 64;
const KERNEL_HEAP_GROW_SIZE: usize = KERNEL_HEAP_GROW_PAGES * PAGE_SIZE;
const KERNEL_HEAP_BOOTSTRAP_PAGES: usize = 64;

pub static KERNEL_HEAP_BYTES: AtomicUsize = AtomicUsize::new(0);
static KERNEL_HEAP_VIRTUAL_BYTES: AtomicUsize = AtomicUsize::new(0);
static KERNEL_HEAP_VIRTUAL_READY: AtomicBool = AtomicBool::new(false);

// The dedicated heap page-table machinery below relies on the whole virtual
// heap window living under a single Sv39 1GiB (VPN[2]) entry, i.e. one shared
// level-1 table, so that runtime heap growth never has to touch the kernel root
// page table (which other code mutates under `KERNEL_SPACE`).
const _: () = assert!(
    KERNEL_HEAP_BASE & ((1usize << 30) - 1) == 0,
    "KERNEL_HEAP_BASE must be 1GiB-aligned"
);
const _: () = assert!(
    MAX_KERNEL_HEAP_SIZE <= (1usize << 30),
    "kernel heap window must fit within a single Sv39 1GiB (VPN[2]) entry"
);

/// Physical page number of the level-1 page table that backs the entire virtual
/// kernel-heap window. Built once at boot (single-threaded) and cached here so
/// that runtime [`map_heap_pages`] can install leaf PTEs without re-walking from
/// — and re-locking — the global `KERNEL_SPACE` page table.
static KERNEL_HEAP_L1_PPN: AtomicUsize = AtomicUsize::new(0);

/// Serializes page-table edits within the kernel-heap subtree.
///
/// This is a *dedicated* lock, distinct from `KERNEL_SPACE`. The previous code
/// grew the heap by taking `KERNEL_SPACE.lock()` inside `map_heap_pages`; but a
/// heap allocation performed *while already holding* `KERNEL_SPACE` (e.g.
/// `kstack_alloc` → `insert_framed_area` → `Vma::map`, which allocates) could
/// then recurse into `grow` → `map_heap_pages` → `KERNEL_SPACE.lock()` and
/// self-deadlock on the non-reentrant lock — wedging every hart, with
/// interrupts disabled so nothing (not even an RT task) could preempt. Because
/// the heap window is a disjoint VPN[2] subtree, edits to it never alias the
/// page-table memory `KERNEL_SPACE` touches, so a separate lock is sufficient
/// and correct. `SpinNoIrqLock` keeps interrupts masked while held so a timer
/// IRQ cannot re-enter the allocator on the same hart.
static HEAP_PT_LOCK: SpinNoIrqLock<()> = SpinNoIrqLock::new(());

/// Build the kernel-heap window's level-1 page table and cache its PPN.
///
/// Must run once, single-threaded, after `KERNEL_SPACE` is active and before the
/// first virtual-window heap growth (see [`init_heap_virtual_window`]).
pub fn init_kernel_heap_mapping() {
    let base_vpn = VirtAddr::from(KERNEL_HEAP_BASE).floor();
    let l1_ppn = KERNEL_SPACE.lock().page_table.ensure_l1_table_untracked(base_vpn);
    KERNEL_HEAP_L1_PPN.store(l1_ppn.0, Ordering::Release);
}

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
            let _irq = HeapIrqGuard::new();
            self.heap.lock().add_to_heap(start, start + bytes);
        }
        true
    }
}

unsafe impl GlobalAlloc for KernelHeapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        {
            let _irq = HeapIrqGuard::new();
            if let Ok(allocation) = self.heap.lock().alloc(layout) {
                return allocation.as_ptr();
            }
        }
        let Some(required_bytes) = layout_required_bytes(layout) else {
            return null_mut();
        };
        loop {
            // debug!("Heap allocation {layout:?} failed, trying to grow heap: required_bytes = {required_bytes}");
            if !self.grow(required_bytes) {
                return null_mut();
            }
            let _irq = HeapIrqGuard::new();
            if let Ok(allocation) = self.heap.lock().alloc(layout) {
                return allocation.as_ptr();
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _irq = HeapIrqGuard::new();
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

/// Index the level-0 leaf PTE slot for a kernel-heap VA, given the (pre-built,
/// cached) level-1 table. Allocates the level-2 leaf table on demand. Returns
/// `None` only on frame exhaustion. The heap window is a single VPN[2] subtree,
/// so VPN[2] is constant and we walk straight from the cached level-1 table —
/// never touching the kernel root page table that `KERNEL_SPACE` guards.
///
/// Caller must hold [`HEAP_PT_LOCK`].
fn heap_leaf_pte(l1_ppn: PhysPageNum, vpn: super::VirtPageNum) -> Option<*mut PageTableEntry> {
    let idxs = vpn.indexes();
    let l1 = &mut l1_ppn.get_pte_array()[idxs[1]];
    if !l1.is_valid() {
        let frame = frame_alloc()?;
        *l1 = PageTableEntry::new(frame.ppn, PTEFlags::V);
        // The leaf table is a permanent kernel mapping; never reclaimed.
        core::mem::forget(frame);
    }
    let l0_ppn = l1.ppn();
    Some(&mut l0_ppn.get_pte_array()[idxs[2]] as *mut PageTableEntry)
}

fn map_heap_pages(start_va: usize, pages: usize) -> bool {
    let l1_ppn = PhysPageNum(KERNEL_HEAP_L1_PPN.load(Ordering::Acquire));
    debug_assert!(l1_ppn.0 != 0, "kernel heap mapping used before init");

    let _guard = HEAP_PT_LOCK.lock();
    for page in 0..pages {
        let va = start_va + page * PAGE_SIZE;
        let vpn = VirtAddr::from(va).floor();
        let Some(pte) = heap_leaf_pte(l1_ppn, vpn) else {
            rollback_heap_pages(l1_ppn, start_va, page);
            return false;
        };
        // SAFETY: `pte` points into a leaf table reachable only through
        // `HEAP_PT_LOCK`; concurrent grows hand out disjoint VA ranges via
        // `reserve_virtual_heap_bytes`, so no two writers target the same slot.
        let entry = unsafe { &mut *pte };
        if entry.is_valid() {
            rollback_heap_pages(l1_ppn, start_va, page);
            return false;
        }
        let Some(frame) = frame_alloc() else {
            rollback_heap_pages(l1_ppn, start_va, page);
            return false;
        };
        *entry = PageTableEntry::new(frame.ppn, PTEFlags::R | PTEFlags::W | PTEFlags::V);
        core::mem::forget(frame);
    }
    unsafe {
        core::arch::asm!("sfence.vma");
    }
    true
}

/// Tear down a partially-mapped run after a failure. Caller holds [`HEAP_PT_LOCK`].
fn rollback_heap_pages(l1_ppn: PhysPageNum, start_va: usize, pages: usize) {
    for page in 0..pages {
        let va = start_va + page * PAGE_SIZE;
        let vpn = VirtAddr::from(va).floor();
        if let Some(pte) = heap_leaf_pte(l1_ppn, vpn) {
            let entry = unsafe { &mut *pte };
            if entry.is_valid() {
                frame_dealloc(entry.ppn());
                *entry = PageTableEntry::empty();
            }
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
