//! The heap allocator.

use super::frame_allocator::{frame_alloc, frame_alloc_contiguous, frame_dealloc};
use super::{phys_to_virt, PTEFlags, PageTableEntry, PhysPageNum, VirtAddr, KERNEL_SPACE};
use crate::config::{KERNEL_HEAP_BASE, MAX_KERNEL_HEAP_SIZE, PAGE_SIZE, PAGE_SIZE_BITS};
use crate::sync::SpinNoIrqLock;
use buddy_system_allocator::linked_list::LinkedList;
use core::alloc::{GlobalAlloc, Layout};
use core::cmp::{max, min};
use core::mem::size_of;
use core::ptr::null_mut;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[global_allocator]
static HEAP_ALLOCATOR: KernelHeapAllocator = KernelHeapAllocator::new();

const KERNEL_HEAP_GROW_PAGES: usize = 64;
const KERNEL_HEAP_GROW_SIZE: usize = KERNEL_HEAP_GROW_PAGES * PAGE_SIZE;
const KERNEL_HEAP_BOOTSTRAP_PAGES: usize = 64;
const KERNEL_HEAP_RECLAIM_START_FREE: usize = 8 * 1024 * 1024;
const KERNEL_HEAP_RECLAIM_TARGET_FREE: usize = 4 * 1024 * 1024;
const KERNEL_HEAP_RECLAIM_MAX_PAGES_PER_CALL: usize = 4096;
const HEAP_ORDER_COUNT: usize = 32;

/// Total capacity of the kernel heap in bytes, grown on demand in fixed
/// `KERNEL_HEAP_GROW_SIZE` increments. Compare with [`KERNEL_HEAP_USED_BYTES`]
/// to gauge internal allocator fragmentation.
pub static KERNEL_HEAP_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Approximate live heap usage (sum of `Layout::size()` on alloc minus dealloc).
/// Tracks application-level demand; compare with [`KERNEL_HEAP_BYTES`] to gauge
/// internal allocator fragmentation.
pub static KERNEL_HEAP_USED_BYTES: AtomicUsize = AtomicUsize::new(0);
static KERNEL_HEAP_VIRTUAL_BYTES: AtomicUsize = AtomicUsize::new(0);
static KERNEL_HEAP_VIRTUAL_READY: AtomicBool = AtomicBool::new(false);
static KERNEL_HEAP_VIRTUAL_LOCK: SpinNoIrqLock<()> = SpinNoIrqLock::new(());

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

struct ReclaimingHeap {
    free_list: [LinkedList; HEAP_ORDER_COUNT],
    user: usize,
    allocated: usize,
    total: usize,
}

impl ReclaimingHeap {
    const fn empty() -> Self {
        Self {
            free_list: [LinkedList::new(); HEAP_ORDER_COUNT],
            user: 0,
            allocated: 0,
            total: 0,
        }
    }

    unsafe fn add_to_heap(&mut self, mut start: usize, mut end: usize) {
        start = align_up_usize(start, size_of::<usize>());
        end &= !(size_of::<usize>() - 1);
        assert!(start <= end);

        let mut total = 0;
        let mut current_start = start;
        while current_start + size_of::<usize>() <= end {
            let lowbit = current_start & (!current_start + 1);
            let size = min(lowbit, prev_power_of_two(end - current_start));
            total += size;
            self.free_list[size.trailing_zeros() as usize].push(current_start as *mut usize);
            current_start += size;
        }
        self.total += total;
    }

    fn alloc(&mut self, layout: Layout) -> Result<NonNull<u8>, ()> {
        let size = max(
            layout.size().next_power_of_two(),
            max(layout.align(), size_of::<usize>()),
        );
        let class = size.trailing_zeros() as usize;
        for i in class..self.free_list.len() {
            if self.free_list[i].is_empty() {
                continue;
            }
            for j in (class + 1..=i).rev() {
                let Some(block) = self.free_list[j].pop() else {
                    return Err(());
                };
                unsafe {
                    self.free_list[j - 1].push((block as usize + (1 << (j - 1))) as *mut usize);
                    self.free_list[j - 1].push(block);
                }
            }
            let result = NonNull::new(
                self.free_list[class]
                    .pop()
                    .expect("current block should have free space now") as *mut u8,
            );
            let Some(result) = result else {
                return Err(());
            };
            self.user += layout.size();
            self.allocated += size;
            return Ok(result);
        }
        Err(())
    }

    fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        let size = max(
            layout.size().next_power_of_two(),
            max(layout.align(), size_of::<usize>()),
        );
        let class = size.trailing_zeros() as usize;

        unsafe {
            self.free_list[class].push(ptr.as_ptr() as *mut usize);

            let mut current_ptr = ptr.as_ptr() as usize;
            let mut current_class = class;
            while current_class + 1 < self.free_list.len() {
                let buddy = current_ptr ^ (1 << current_class);
                let mut found_buddy = false;
                for block in self.free_list[current_class].iter_mut() {
                    if block.value() as usize == buddy {
                        block.pop();
                        found_buddy = true;
                        break;
                    }
                }
                if !found_buddy {
                    break;
                }
                self.free_list[current_class].pop();
                current_ptr = min(current_ptr, buddy);
                current_class += 1;
                self.free_list[current_class].push(current_ptr as *mut usize);
            }
        }

        self.user -= layout.size();
        self.allocated -= size;
    }

    fn free_actual_bytes(&self) -> usize {
        self.total.saturating_sub(self.allocated)
    }

    fn release_one_tail_free_block(
        &mut self,
        min_size: usize,
        range_start: usize,
        range_end: usize,
    ) -> Option<(usize, usize)> {
        let min_class = min_size.next_power_of_two().trailing_zeros() as usize;
        for class in (min_class..self.free_list.len()).rev() {
            let size = 1usize << class;
            for block in self.free_list[class].iter_mut() {
                let start = block.value() as usize;
                let end = start.saturating_add(size);
                if start >= range_start && end == range_end && start & (PAGE_SIZE - 1) == 0 {
                    block.pop();
                    self.total = self.total.saturating_sub(size);
                    return Some((start, size));
                }
            }
        }
        None
    }
}

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
    heap: SpinNoIrqLock<ReclaimingHeap>,
}

impl KernelHeapAllocator {
    const fn new() -> Self {
        Self {
            heap: SpinNoIrqLock::new(ReclaimingHeap::empty()),
        }
    }

    fn grow(&self, required_bytes: usize) -> bool {
        if KERNEL_HEAP_VIRTUAL_READY.load(Ordering::Acquire) {
            return self.grow_virtual(required_bytes);
        }

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
        unsafe { self.heap.lock().add_to_heap(start, start + bytes) };
        true
    }

    fn grow_virtual(&self, required_bytes: usize) -> bool {
        let _virtual_guard = KERNEL_HEAP_VIRTUAL_LOCK.lock();
        let Some((virtual_offset, bytes)) = reserve_virtual_heap_bytes_locked(required_bytes)
        else {
            return false;
        };
        // debug!(
        //     "Growing virtual kernel heap: {} KiB -> {} KiB",
        //     virtual_offset / 1024,
        //     (virtual_offset + bytes) / 1024
        // );
        let start = KERNEL_HEAP_BASE + virtual_offset;
        if !map_heap_pages(start, bytes / PAGE_SIZE) {
            rollback_virtual_heap_reservation_locked(virtual_offset, bytes);
            return false;
        }
        unsafe { self.heap.lock().add_to_heap(start, start + bytes) };
        true
    }

    fn reclaim_free_pages_if_needed(&self) -> usize {
        if !KERNEL_HEAP_VIRTUAL_READY.load(Ordering::Acquire) {
            return 0;
        }
        let mut reclaimed_pages = 0usize;
        loop {
            if reclaimed_pages >= KERNEL_HEAP_RECLAIM_MAX_PAGES_PER_CALL {
                break;
            }
            let _virtual_guard = KERNEL_HEAP_VIRTUAL_LOCK.lock();
            let virtual_bytes = KERNEL_HEAP_VIRTUAL_BYTES.load(Ordering::Acquire);
            let virtual_end = KERNEL_HEAP_BASE + virtual_bytes;
            let released = {
                let mut heap = self.heap.lock();
                if heap.free_actual_bytes() <= KERNEL_HEAP_RECLAIM_START_FREE {
                    None
                } else {
                    heap.release_one_tail_free_block(PAGE_SIZE, KERNEL_HEAP_BASE, virtual_end)
                }
            };
            let Some((start, bytes)) = released else {
                break;
            };
            let pages = bytes / PAGE_SIZE;
            unmap_heap_pages(start, pages);
            KERNEL_HEAP_VIRTUAL_BYTES.store(virtual_bytes - bytes, Ordering::Release);
            KERNEL_HEAP_BYTES.fetch_sub(bytes, Ordering::AcqRel);
            drop(_virtual_guard);
            reclaimed_pages += pages;
            let free_after = self.heap.lock().free_actual_bytes();
            if free_after <= KERNEL_HEAP_RECLAIM_TARGET_FREE {
                break;
            }
        }
        reclaimed_pages
    }
}

unsafe impl GlobalAlloc for KernelHeapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        {
            let mut heap = self.heap.lock();
            if let Ok(allocation) = heap.alloc(layout) {
                KERNEL_HEAP_USED_BYTES.fetch_add(layout.size(), Ordering::AcqRel);
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
            let mut heap = self.heap.lock();
            if let Ok(allocation) = heap.alloc(layout) {
                KERNEL_HEAP_USED_BYTES.fetch_add(layout.size(), Ordering::AcqRel);
                return allocation.as_ptr();
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let mut heap = self.heap.lock();
        heap.dealloc(core::ptr::NonNull::new_unchecked(ptr), layout);
        KERNEL_HEAP_USED_BYTES.fetch_sub(layout.size(), Ordering::AcqRel);
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

/// Return completely free virtual-heap pages to the frame allocator when the
/// heap retained a large short-lived allocation spike.
pub fn reclaim_kernel_heap_if_needed() -> usize {
    HEAP_ALLOCATOR.reclaim_free_pages_if_needed()
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

fn reserve_virtual_heap_bytes_locked(required_bytes: usize) -> Option<(usize, usize)> {
    let required_bytes = align_up_to_page(required_bytes)?;
    let used = KERNEL_HEAP_VIRTUAL_BYTES.load(Ordering::Acquire);
    let aligned_block_end = align_up(used, required_bytes)?.checked_add(required_bytes)?;
    let normal_grow_end = used.checked_add(KERNEL_HEAP_GROW_SIZE.max(required_bytes))?;
    let new_used = aligned_block_end.max(normal_grow_end);
    if new_used > MAX_KERNEL_HEAP_SIZE {
        return None;
    }
    let bytes = new_used - used;
    KERNEL_HEAP_VIRTUAL_BYTES.store(new_used, Ordering::Release);
    KERNEL_HEAP_BYTES.fetch_add(bytes, Ordering::AcqRel);
    Some((used, bytes))
}

fn rollback_virtual_heap_reservation_locked(virtual_offset: usize, bytes: usize) {
    if let Some(reservation_end) = virtual_offset.checked_add(bytes) {
        if KERNEL_HEAP_VIRTUAL_BYTES.load(Ordering::Acquire) == reservation_end {
            KERNEL_HEAP_VIRTUAL_BYTES.store(virtual_offset, Ordering::Release);
        }
    }
    KERNEL_HEAP_BYTES.fetch_sub(bytes, Ordering::AcqRel);
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    let mask = align.checked_sub(1)?;
    Some(value.checked_add(mask)? & !mask)
}

fn align_up_usize(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

fn prev_power_of_two(value: usize) -> usize {
    1usize << (usize::BITS as usize - 1 - value.leading_zeros() as usize)
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

fn existing_heap_leaf_pte(
    subtree_root_ppn: PhysPageNum,
    vpn: super::VirtPageNum,
) -> Option<*mut PageTableEntry> {
    let levels = crate::hal::page_table_levels();
    let mut ppn = subtree_root_ppn;
    for level in 1..levels {
        let idx = crate::hal::vpn_index(vpn.0, level);
        let pte = &mut ppn.get_pte_array()[idx];
        if level + 1 == levels {
            return Some(pte as *mut PageTableEntry);
        }
        if !pte.is_valid() {
            return None;
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
    let mut mapped_pages = 0;
    let mapped_all = {
        let _guard = HEAP_PT_LOCK.lock();
        if crate::platform::heap_debug_enabled() {
            crate::platform::early_console_write("[heap] HEAP_PT_LOCK locked\r\n");
        }
        let mut mapped_all = true;
        for page in 0..pages {
            let va = start_va + page * PAGE_SIZE;
            let vpn = VirtAddr::from(va).floor();
            let Some(pte) = heap_leaf_pte(subtree_root_ppn, vpn) else {
                rollback_heap_pages(subtree_root_ppn, start_va, mapped_pages);
                mapped_all = false;
                break;
            };
            // SAFETY: `pte` points into a leaf table reachable only through
            // `HEAP_PT_LOCK`; virtual grow transactions hand out disjoint VA
            // ranges under `KERNEL_HEAP_VIRTUAL_LOCK`, so no two writers
            // target the same slot.
            let entry = unsafe { &mut *pte };
            if entry.is_valid() {
                rollback_heap_pages(subtree_root_ppn, start_va, mapped_pages);
                mapped_all = false;
                break;
            }
            let Some(frame) = frame_alloc() else {
                rollback_heap_pages(subtree_root_ppn, start_va, mapped_pages);
                mapped_all = false;
                break;
            };
            *entry = PageTableEntry::new(
                frame.ppn,
                PTEFlags::R | PTEFlags::W | PTEFlags::V | PTEFlags::A | PTEFlags::D,
            );
            core::mem::forget(frame);
            mapped_pages += 1;
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
        mapped_all
    };
    if mapped_pages > 0 {
        crate::mm::shootdown_global_quiet();
    }
    mapped_all
}

fn unmap_heap_pages(start_va: usize, pages: usize) {
    if pages == 0 {
        return;
    }
    let subtree_root_ppn = PhysPageNum(KERNEL_HEAP_SUBTREE_ROOT_PPN.load(Ordering::Acquire));
    if subtree_root_ppn.0 == 0 {
        panic!("unmap_heap_pages: subtree root ppn is 0");
    }
    let mut unmapped_pages = 0usize;
    {
        let _guard = HEAP_PT_LOCK.lock();
        for page in 0..pages {
            let va = start_va + page * PAGE_SIZE;
            let vpn = VirtAddr::from(va).floor();
            let Some(pte) = existing_heap_leaf_pte(subtree_root_ppn, vpn) else {
                continue;
            };
            let entry = unsafe { &mut *pte };
            if !entry.is_valid() {
                continue;
            }
            frame_dealloc(entry.ppn());
            *entry = PageTableEntry::empty();
            unmapped_pages += 1;
        }
    }
    if unmapped_pages != 0 {
        crate::mm::shootdown_global_quiet();
    }
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
}

#[allow(unused)]
pub fn heap_test() {
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    extern "C" {
        fn sbss();
        fn ebss();
    }
    let bss_range = sbss as usize..ebss as usize;
    let virtual_heap_range = KERNEL_HEAP_BASE..KERNEL_HEAP_BASE + MAX_KERNEL_HEAP_SIZE;
    let a = Box::new(5);
    assert_eq!(*a, 5);
    let a_ptr = a.as_ref() as *const _ as usize;
    assert!(!bss_range.contains(&a_ptr));
    assert!(is_bootinfo_ram_va(a_ptr) || virtual_heap_range.contains(&a_ptr));
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
    assert!(is_bootinfo_ram_va(v_ptr) || virtual_heap_range.contains(&v_ptr));
    drop(v);
    println!("heap_test passed!");
}

#[allow(unused)]
fn is_bootinfo_ram_va(va: usize) -> bool {
    let pa = crate::platform::direct_map_virt_to_phys(va);
    let mut found = false;
    crate::bootinfo::for_each_usable_memory_region(|region| {
        found |= pa >= region.start && pa < region.end;
    });
    found
}
