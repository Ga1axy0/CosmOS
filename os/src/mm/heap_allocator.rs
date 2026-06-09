//! The heap allocator.

use super::frame_allocator::{frame_alloc, frame_alloc_contiguous, frame_dealloc};
use super::{phys_to_virt, PageTableEntry, PhysPageNum, PTEFlags, VirtAddr, KERNEL_SPACE};
use crate::config::{
    KERNEL_HEAP_BASE, MAX_KERNEL_HEAP_SIZE, MEMORY_END, PAGE_SIZE, PAGE_SIZE_BITS,
};
use crate::sync::SpinNoIrqLock;
use buddy_system_allocator::LockedHeap;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
        let sie_was_enabled = crate::hal::local_irqs_enabled();
        unsafe { crate::hal::disable_local_irqs() };
        Self { sie_was_enabled }
    }
}

impl Drop for HeapIrqGuard {
    #[inline]
    fn drop(&mut self) {
        if self.sie_was_enabled {
            unsafe { crate::hal::enable_local_irqs() };
        }
    }
}

const KERNEL_HEAP_GROW_PAGES: usize = 64;
const KERNEL_HEAP_GROW_SIZE: usize = KERNEL_HEAP_GROW_PAGES * PAGE_SIZE;
const KERNEL_HEAP_BOOTSTRAP_PAGES: usize = 64;

pub static KERNEL_HEAP_BYTES: AtomicUsize = AtomicUsize::new(0);
static KERNEL_HEAP_VIRTUAL_BYTES: AtomicUsize = AtomicUsize::new(0);
static KERNEL_HEAP_VIRTUAL_READY: AtomicBool = AtomicBool::new(false);

const ROOT_ENTRY_SPAN: usize = 1usize
    << (PAGE_SIZE_BITS
        + (crate::hal::page_table_levels() - 1) * crate::hal::page_table_index_bits());

const _: () = assert!(
    crate::hal::page_table_levels() >= 2,
    "kernel heap virtual window requires a multi-level page table"
);

// The dedicated heap page-table machinery below relies on the whole virtual
// heap window living under a single root page-table entry, i.e. one shared
// first-level subtree, so that runtime heap growth never has to touch the
// kernel root page table (which other code mutates under `KERNEL_SPACE`).
const _: () = assert!(
    KERNEL_HEAP_BASE & (ROOT_ENTRY_SPAN - 1) == 0,
    "KERNEL_HEAP_BASE must be aligned to one root page-table entry span"
);
const _: () = assert!(
    MAX_KERNEL_HEAP_SIZE <= ROOT_ENTRY_SPAN,
    "kernel heap window must fit within a single root page-table entry span"
);

/// Physical page number of the first-level subtree table that backs the entire
/// virtual kernel-heap window. Built once at boot (single-threaded) and cached
/// here so that runtime [`map_heap_pages`] can install leaf PTEs without
/// re-walking from — and re-locking — the global `KERNEL_SPACE` page table.
static KERNEL_HEAP_SUBTREE_ROOT_PPN: AtomicUsize = AtomicUsize::new(0);

/// Serializes page-table edits within the kernel-heap subtree.
///
/// This is a *dedicated* lock, distinct from `KERNEL_SPACE`. The previous code
/// grew the heap by taking `KERNEL_SPACE.lock()` inside `map_heap_pages`; but a
/// heap allocation performed *while already holding* `KERNEL_SPACE` (e.g.
/// `kstack_alloc` → `insert_framed_area` → `Vma::map`, which allocates) could
/// then recurse into `grow` → `map_heap_pages` → `KERNEL_SPACE.lock()` and
/// self-deadlock on the non-reentrant lock — wedging every hart, with
/// interrupts disabled so nothing (not even an RT task) could preempt. Because
/// the heap window is a disjoint root-entry subtree, edits to it never alias the
/// page-table memory `KERNEL_SPACE` touches, so a separate lock is sufficient
/// and correct. `SpinNoIrqLock` keeps interrupts masked while held so a timer
/// IRQ cannot re-enter the allocator on the same hart.
static HEAP_PT_LOCK: SpinNoIrqLock<()> = SpinNoIrqLock::new(());

/// Build the kernel-heap window's first-level subtree table and cache its PPN.
///
/// Must run once, single-threaded, after `KERNEL_SPACE` is active and before the
/// first virtual-window heap growth (see [`init_heap_virtual_window`]).
pub fn init_kernel_heap_mapping() {
    let base_vpn = VirtAddr::from(KERNEL_HEAP_BASE).floor();
    let subtree_root_ppn = KERNEL_SPACE
        .lock()
        .page_table
        .ensure_subtree_root_untracked(base_vpn);
    if subtree_root_ppn.0 == 0 {
        panic!("ensure_subtree_root_untracked returned PPN 0");
    }
    KERNEL_HEAP_SUBTREE_ROOT_PPN.store(subtree_root_ppn.0, Ordering::Release);
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
    if !crate::platform::kernel_heap_virtual_window_supported() {
        // LA64 bring-up still faults on the first access into the low-half
        // heap window even after the leaf PTE is installed and TLB state is
        // refreshed. Keep using the already-working DMW-backed bootstrap heap
        // path for now so the kernel can continue booting on LoongArch.
        crate::platform::early_console_write("[heap] virtual window disabled on loongarch64\r\n");
        return;
    }
    KERNEL_HEAP_VIRTUAL_READY.store(true, Ordering::Release);
    assert!(
        HEAP_ALLOCATOR.grow(KERNEL_HEAP_GROW_SIZE),
        "failed to initialize virtual kernel heap"
    );
}

/// Map a single heap VA page. Called from trap_from_kernel on LoongArch.
pub fn map_one_heap_page(va: usize) -> bool {
    map_heap_pages(va, 1)
}

fn early_put_hex(label: &str, value: usize) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut buf = [0u8; 2 + 16 + 2];
    buf[0] = b'0';
    buf[1] = b'x';
    for (idx, slot) in buf[2..18].iter_mut().enumerate() {
        let shift = (15 - idx) * 4;
        *slot = HEX[(value >> shift) & 0xf];
    }
    buf[18] = b'\r';
    buf[19] = b'\n';
    crate::platform::early_console_write(label);
    // SAFETY: ASCII hex buffer is always valid UTF-8.
    crate::platform::early_console_write(core::str::from_utf8(&buf).unwrap());
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
    Some(phys_to_virt(start_pa.into()))
}

/// Index the leaf PTE slot for a kernel-heap VA, given the pre-built cached
/// subtree root table. Allocates lower-level page tables on demand. Returns
/// `None` only on frame exhaustion. The heap window is a single root-entry
/// subtree, so we walk straight from that cached subtree root — never touching
/// the kernel root page table that `KERNEL_SPACE` guards.
///
/// Caller must hold [`HEAP_PT_LOCK`].
/// Walk from `subtree_root_ppn` (which PGDL's root[0] already points to) down
/// to the leaf PTE slot for `vpn`.  `subtree_root_ppn` is at depth 1, so we
/// walk exactly `levels - 2` more directory hops before reaching the leaf table.
///
/// Caller must hold [`HEAP_PT_LOCK`].
fn heap_leaf_pte(
    subtree_root_ppn: PhysPageNum,
    vpn: super::VirtPageNum,
) -> Option<*mut PageTableEntry> {
    let levels = crate::hal::page_table_levels();
    let mut ppn = subtree_root_ppn;
    // Walk levels 1 .. levels-1 (directories), then return the leaf slot at level levels-1.
    for level in 1..levels {
        let idx = crate::hal::vpn_index(vpn.0, level);
        let pte = &mut ppn.get_pte_array()[idx];
        if level + 1 == levels {
            // This pte slot IS the leaf PTE (will be filled by map_heap_pages).
            return Some(pte as *mut PageTableEntry);
        }
        if !pte.is_valid() {
            let frame = frame_alloc()?;
            frame.ppn.get_bytes_array().fill(0);
            pte.bits = crate::hal::make_dir_entry(frame.ppn.0);
            core::mem::forget(frame);
        }
        ppn = pte.ppn();
    }
    None
}

fn map_heap_pages(start_va: usize, pages: usize) -> bool {
    if crate::platform::heap_debug_enabled() {
        crate::platform::early_console_write("[heap] map_heap_pages\r\n");
    }
    let subtree_root_ppn = PhysPageNum(KERNEL_HEAP_SUBTREE_ROOT_PPN.load(Ordering::Acquire));
    if subtree_root_ppn.0 == 0 {
        panic!("map_heap_pages: subtree root ppn is 0");
    }
    if crate::platform::heap_debug_enabled() {
        crate::platform::early_console_write("[heap] locking HEAP_PT_LOCK\r\n");
    }
    let _guard = HEAP_PT_LOCK.lock();
    if crate::platform::heap_debug_enabled() {
        crate::platform::early_console_write("[heap] HEAP_PT_LOCK locked\r\n");
    }
    for page in 0..pages {
        let va = start_va + page * PAGE_SIZE;
        let vpn = VirtAddr::from(va).floor();
        let Some(pte) = heap_leaf_pte(subtree_root_ppn, vpn) else {
            rollback_heap_pages(subtree_root_ppn, start_va, page);
            return false;
        };
        // SAFETY: `pte` points into a leaf table reachable only through
        // `HEAP_PT_LOCK`; concurrent grows hand out disjoint VA ranges via
        // `reserve_virtual_heap_bytes`, so no two writers target the same slot.
        let entry = unsafe { &mut *pte };
        if entry.is_valid() {
            rollback_heap_pages(subtree_root_ppn, start_va, page);
            return false;
        }
        let Some(frame) = frame_alloc() else {
            rollback_heap_pages(subtree_root_ppn, start_va, page);
            return false;
        };
        *entry = PageTableEntry::new(
            frame.ppn,
            PTEFlags::R | PTEFlags::W | PTEFlags::V | PTEFlags::A | PTEFlags::D,
        );
        core::mem::forget(frame);
    }
    if crate::platform::heap_debug_enabled() && pages > 0 {
        let vpn = VirtAddr::from(start_va).floor();
        let root_idx = crate::hal::vpn_index(vpn.0, 0);
        let mid_idx = crate::hal::vpn_index(vpn.0, 1);
        let leaf_idx = crate::hal::vpn_index(vpn.0, 2);
        let root = PhysPageNum(crate::hal::root_ppn_from_token(crate::mm::kernel_token()));
        let root_pte = root.get_pte_array()[root_idx].bits;
        let mid_ppn = PhysPageNum(crate::hal::pte_ppn(root_pte));
        let mid_pte = mid_ppn.get_pte_array()[mid_idx].bits;
        let leaf_ppn = PhysPageNum(crate::hal::pte_ppn(mid_pte));
        let leaf_pte = leaf_ppn.get_pte_array()[leaf_idx].bits;
        early_put_hex("[heap] root_pte=", root_pte);
        early_put_hex("[heap] mid_pte=", mid_pte);
        early_put_hex("[heap] leaf_pte=", leaf_pte);
    }
    unsafe { crate::hal::flush_tlb() };
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
    unsafe { crate::hal::flush_tlb() };
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
