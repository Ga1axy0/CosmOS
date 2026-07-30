//! The heap allocator.

use super::frame_allocator::{frame_alloc, frame_alloc_contiguous, frame_dealloc};
use super::{phys_to_virt, PTEFlags, PageTableEntry, PhysPageNum, VirtAddr, KERNEL_SPACE};
use crate::config::{KERNEL_HEAP_BASE, MAX_KERNEL_HEAP_SIZE, PAGE_SIZE, PAGE_SIZE_BITS};
use crate::sync::SpinNoIrqLock;
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
/// Keep reclaim bookkeeping off the heap: this path runs from the global
/// allocator itself.  A small stack batch also avoids growing kernel stack use
/// with the size of a coalesced tail block.
const KERNEL_HEAP_UNMAP_BATCH_PAGES: usize = 256;
const HEAP_ORDER_COUNT: usize = 32;
const MIN_BUDDY_BLOCK_SIZE: usize = 2 * size_of::<usize>();
const MIN_BUDDY_ORDER: usize = MIN_BUDDY_BLOCK_SIZE.trailing_zeros() as usize;
const FREE_INDEX_CAPACITY: usize = 65536;
const FREE_INDEX_MASK: usize = FREE_INDEX_CAPACITY - 1;
const FREE_INDEX_EMPTY: u8 = 0;
const FREE_INDEX_TOMBSTONE: u8 = 1;
const FREE_INDEX_USED: u8 = 2;
const SLAB_CHUNK_SIZE: usize = PAGE_SIZE;
const SLAB_CLASS_COUNT: usize = 5;
const SLAB_MAX_SIZE: usize = 256;
const SLAB_MAGIC: usize = 0x534c_4142_4348_4b31;

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

#[derive(Copy, Clone)]
struct FreeList {
    head: *mut usize,
}

unsafe impl Send for FreeList {}

impl FreeList {
    const fn new() -> Self {
        Self {
            head: core::ptr::null_mut(),
        }
    }

    fn is_empty(&self) -> bool {
        self.head.is_null()
    }

    unsafe fn push(&mut self, block: *mut usize) {
        *block = self.head as usize;
        *block.add(1) = 0;
        if !self.head.is_null() {
            *self.head.add(1) = block as usize;
        }
        self.head = block;
    }

    unsafe fn remove(&mut self, block: *mut usize) {
        let next = *block as *mut usize;
        let previous = *block.add(1) as *mut usize;
        if previous.is_null() {
            debug_assert_eq!(self.head, block);
            self.head = next;
        } else {
            *previous = next as usize;
        }
        if !next.is_null() {
            *next.add(1) = previous as usize;
        }
        *block = 0;
        *block.add(1) = 0;
    }

    fn pop(&mut self) -> Option<*mut usize> {
        let block = self.head;
        if block.is_null() {
            None
        } else {
            unsafe { self.remove(block) };
            Some(block)
        }
    }

    fn iter(&self) -> FreeListIter {
        FreeListIter { current: self.head }
    }
}

struct FreeListIter {
    current: *mut usize,
}

impl Iterator for FreeListIter {
    type Item = *mut usize;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current.is_null() {
            return None;
        }
        let current = self.current;
        self.current = unsafe { *current as *mut usize };
        Some(current)
    }
}

#[derive(Copy, Clone)]
struct FreeIndexEntry {
    key: usize,
    order: u8,
    state: u8,
}

impl FreeIndexEntry {
    const fn empty() -> Self {
        Self {
            key: 0,
            order: 0,
            state: FREE_INDEX_EMPTY,
        }
    }
}

struct FreeBlockIndex {
    entries: [FreeIndexEntry; FREE_INDEX_CAPACITY],
}

impl FreeBlockIndex {
    const fn empty() -> Self {
        Self {
            entries: [FreeIndexEntry::empty(); FREE_INDEX_CAPACITY],
        }
    }

    fn hash(key: usize) -> usize {
        let mut value = key >> MIN_BUDDY_ORDER;
        value ^= value >> 17;
        value ^= value >> 9;
        value & FREE_INDEX_MASK
    }

    fn find(&self, key: usize, order: usize) -> Option<usize> {
        let start = Self::hash(key);
        for offset in 0..FREE_INDEX_CAPACITY {
            let slot = (start + offset) & FREE_INDEX_MASK;
            let entry = self.entries[slot];
            if entry.state == FREE_INDEX_EMPTY {
                return None;
            }
            if entry.state == FREE_INDEX_USED && entry.key == key && entry.order as usize == order {
                return Some(slot);
            }
        }
        None
    }

    fn insert(&mut self, key: usize, order: usize) -> bool {
        let start = Self::hash(key);
        let mut tombstone = None;
        for offset in 0..FREE_INDEX_CAPACITY {
            let slot = (start + offset) & FREE_INDEX_MASK;
            let entry = self.entries[slot];
            match entry.state {
                FREE_INDEX_EMPTY => {
                    let slot = tombstone.unwrap_or(slot);
                    self.entries[slot] = FreeIndexEntry {
                        key,
                        order: order as u8,
                        state: FREE_INDEX_USED,
                    };
                    return true;
                }
                FREE_INDEX_TOMBSTONE => {
                    if tombstone.is_none() {
                        tombstone = Some(slot);
                    }
                }
                FREE_INDEX_USED => {
                    if entry.key == key && entry.order as usize == order {
                        return true;
                    }
                }
                _ => unreachable!(),
            }
        }
        if let Some(slot) = tombstone {
            self.entries[slot] = FreeIndexEntry {
                key,
                order: order as u8,
                state: FREE_INDEX_USED,
            };
            true
        } else {
            false
        }
    }

    fn remove(&mut self, key: usize, order: usize) -> bool {
        let Some(slot) = self.find(key, order) else {
            return false;
        };
        self.entries[slot].state = FREE_INDEX_TOMBSTONE;
        true
    }
}

struct ReclaimingHeap {
    free_list: [FreeList; HEAP_ORDER_COUNT],
    free_index: FreeBlockIndex,
    free_index_overflow: bool,
    user: usize,
    allocated: usize,
    total: usize,
    /// Cumulative number of free-list nodes inspected while coalescing a
    /// deallocation. This exposes the linear-search cost of the current buddy
    /// implementation.
    #[cfg(feature = "cosmos-meminfo")]
    free_scan_steps: usize,
    /// Cumulative number of buddy-level deallocations.
    #[cfg(feature = "cosmos-meminfo")]
    buddy_free_calls: usize,
}

/// Snapshot of the kernel heap allocator's fragmentation and free-list state.
#[cfg(feature = "cosmos-meminfo")]
#[derive(Clone, Copy, Debug)]
pub struct KernelHeapAllocatorStats {
    /// Bytes requested by live allocations.
    pub requested_bytes: usize,
    /// Bytes occupied by live allocations after size-class rounding, including
    /// occupied slab slots but excluding free slab slots.
    pub allocated_bytes: usize,
    /// Bytes currently available from buddy free blocks and slab free slots.
    pub actual_free_bytes: usize,
    /// Size of the largest currently available free block.
    pub largest_free_bytes: usize,
    /// Cumulative free-list nodes inspected during deallocation coalescing.
    pub free_scan_steps: usize,
    /// Cumulative number of deallocations.
    pub free_calls: usize,
    /// Cumulative number of deallocations that reached the buddy allocator.
    pub buddy_free_calls: usize,
    /// Bytes reserved for slab chunks.
    pub slab_reserved_bytes: usize,
    /// Bytes currently available in slab slots.
    pub slab_free_bytes: usize,
}

impl ReclaimingHeap {
    const fn empty() -> Self {
        Self {
            free_list: [FreeList::new(); HEAP_ORDER_COUNT],
            free_index: FreeBlockIndex::empty(),
            free_index_overflow: false,
            user: 0,
            allocated: 0,
            total: 0,
            #[cfg(feature = "cosmos-meminfo")]
            free_scan_steps: 0,
            #[cfg(feature = "cosmos-meminfo")]
            buddy_free_calls: 0,
        }
    }

    fn block_size(layout: Layout) -> usize {
        max(
            layout.size().next_power_of_two(),
            max(layout.align(), MIN_BUDDY_BLOCK_SIZE),
        )
    }

    fn rounded_size(size: usize, align: usize) -> usize {
        max(size.next_power_of_two(), max(align, MIN_BUDDY_BLOCK_SIZE))
    }

    unsafe fn push_free_block(&mut self, order: usize, block: *mut usize) {
        debug_assert!(order < HEAP_ORDER_COUNT);
        debug_assert!(order >= MIN_BUDDY_ORDER);
        self.free_list[order].push(block);
        if !self.free_index.insert(block as usize, order) {
            self.free_index_overflow = true;
        }
    }

    fn pop_free_block(&mut self, order: usize) -> Option<*mut usize> {
        let block = self.free_list[order].pop()?;
        self.free_index.remove(block as usize, order);
        Some(block)
    }

    fn remove_free_block(&mut self, order: usize, block: *mut usize) -> bool {
        if self.free_index.remove(block as usize, order) {
            unsafe { self.free_list[order].remove(block) };
            return true;
        }
        if self.free_index_overflow {
            let mut current = self.free_list[order].head;
            while !current.is_null() {
                #[cfg(feature = "cosmos-meminfo")]
                {
                    self.free_scan_steps += 1;
                }
                if current == block {
                    unsafe { self.free_list[order].remove(current) };
                    return true;
                }
                current = unsafe { *current as *mut usize };
            }
        }
        false
    }

    unsafe fn add_to_heap(&mut self, mut start: usize, mut end: usize) {
        start = align_up_usize(start, MIN_BUDDY_BLOCK_SIZE);
        end &= !(MIN_BUDDY_BLOCK_SIZE - 1);
        assert!(start <= end);

        let mut total = 0;
        let mut current_start = start;
        while current_start + MIN_BUDDY_BLOCK_SIZE <= end {
            let lowbit = current_start & (!current_start + 1);
            let size = min(lowbit, prev_power_of_two(end - current_start));
            total += size;
            assert!(size >= MIN_BUDDY_BLOCK_SIZE);
            unsafe {
                self.push_free_block(size.trailing_zeros() as usize, current_start as *mut usize);
            }
            current_start += size;
        }
        self.total += total;
    }

    fn alloc(&mut self, layout: Layout) -> Result<NonNull<u8>, ()> {
        let size = Self::block_size(layout);
        let result = self.alloc_rounded(size)?;
        self.user += layout.size();
        Ok(result)
    }

    fn alloc_raw(&mut self, size: usize, align: usize) -> Result<NonNull<u8>, ()> {
        self.alloc_rounded(Self::rounded_size(size, align))
    }

    fn alloc_rounded(&mut self, size: usize) -> Result<NonNull<u8>, ()> {
        let class = size.trailing_zeros() as usize;
        for i in class..self.free_list.len() {
            if self.free_list[i].is_empty() {
                continue;
            }
            for j in (class + 1..=i).rev() {
                let Some(block) = self.pop_free_block(j) else {
                    return Err(());
                };
                unsafe {
                    self.push_free_block(j - 1, (block as usize + (1 << (j - 1))) as *mut usize);
                    self.push_free_block(j - 1, block);
                }
            }
            let result = NonNull::new(
                self.pop_free_block(class)
                    .expect("current block should have free space now") as *mut u8,
            );
            let Some(result) = result else {
                return Err(());
            };
            self.allocated += size;
            return Ok(result);
        }
        Err(())
    }

    fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) {
        let size = Self::block_size(layout);
        self.dealloc_rounded(ptr, size);
        self.user -= layout.size();
    }

    fn dealloc_raw(&mut self, ptr: NonNull<u8>, size: usize, align: usize) {
        self.dealloc_rounded(ptr, Self::rounded_size(size, align));
    }

    fn dealloc_rounded(&mut self, ptr: NonNull<u8>, size: usize) {
        #[cfg(feature = "cosmos-meminfo")]
        {
            self.buddy_free_calls += 1;
        }
        let class = size.trailing_zeros() as usize;

        unsafe {
            self.push_free_block(class, ptr.as_ptr() as *mut usize);

            let mut current_ptr = ptr.as_ptr() as usize;
            let mut current_class = class;
            while current_class + 1 < self.free_list.len() {
                let buddy = current_ptr ^ (1 << current_class);
                if !self.remove_free_block(current_class, buddy as *mut usize) {
                    break;
                }
                assert!(self.remove_free_block(current_class, current_ptr as *mut usize));
                current_ptr = min(current_ptr, buddy);
                current_class += 1;
                self.push_free_block(current_class, current_ptr as *mut usize);
            }
        }

        self.allocated -= size;
    }

    fn free_actual_bytes(&self) -> usize {
        self.total.saturating_sub(self.allocated)
    }

    #[cfg(feature = "cosmos-meminfo")]
    fn largest_free_block(&self) -> usize {
        for class in (0..HEAP_ORDER_COUNT).rev() {
            if !self.free_list[class].is_empty() {
                return 1usize << class;
            }
        }
        0
    }

    #[cfg(feature = "cosmos-meminfo")]
    fn stats(&self) -> KernelHeapAllocatorStats {
        KernelHeapAllocatorStats {
            requested_bytes: self.user,
            allocated_bytes: self.allocated,
            actual_free_bytes: self.free_actual_bytes(),
            largest_free_bytes: self.largest_free_block(),
            free_scan_steps: self.free_scan_steps,
            free_calls: self.buddy_free_calls,
            buddy_free_calls: self.buddy_free_calls,
            slab_reserved_bytes: 0,
            slab_free_bytes: 0,
        }
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
            for block in self.free_list[class].iter() {
                let start = block as usize;
                let end = start.saturating_add(size);
                if start >= range_start && end == range_end && start & (PAGE_SIZE - 1) == 0 {
                    assert!(self.remove_free_block(class, block));
                    self.total = self.total.saturating_sub(size);
                    return Some((start, size));
                }
            }
        }
        None
    }
}

#[repr(C)]
struct SlabChunkHeader {
    magic: usize,
    class: usize,
    slot_size: usize,
    total_slots: usize,
    free_slots: usize,
    free_head: *mut usize,
    previous: *mut SlabChunkHeader,
    next: *mut SlabChunkHeader,
}

struct SmallSlabClass {
    slot_size: usize,
    available_head: *mut SlabChunkHeader,
    available_chunks: usize,
    #[cfg(feature = "cosmos-meminfo")]
    reserved_bytes: usize,
    #[cfg(feature = "cosmos-meminfo")]
    free_bytes: usize,
    #[cfg(feature = "cosmos-meminfo")]
    requested_bytes: usize,
}

unsafe impl Send for SmallSlabClass {}

impl SmallSlabClass {
    const fn new(slot_size: usize) -> Self {
        Self {
            slot_size,
            available_head: core::ptr::null_mut(),
            available_chunks: 0,
            #[cfg(feature = "cosmos-meminfo")]
            reserved_bytes: 0,
            #[cfg(feature = "cosmos-meminfo")]
            free_bytes: 0,
            #[cfg(feature = "cosmos-meminfo")]
            requested_bytes: 0,
        }
    }

    unsafe fn add_available(&mut self, chunk: *mut SlabChunkHeader) {
        (*chunk).previous = core::ptr::null_mut();
        (*chunk).next = self.available_head;
        if !self.available_head.is_null() {
            (*self.available_head).previous = chunk;
        }
        self.available_head = chunk;
        self.available_chunks += 1;
    }

    unsafe fn remove_available(&mut self, chunk: *mut SlabChunkHeader) {
        let previous = (*chunk).previous;
        let next = (*chunk).next;
        if previous.is_null() {
            debug_assert_eq!(self.available_head, chunk);
            self.available_head = next;
        } else {
            (*previous).next = next;
        }
        if !next.is_null() {
            (*next).previous = previous;
        }
        (*chunk).previous = core::ptr::null_mut();
        (*chunk).next = core::ptr::null_mut();
        self.available_chunks -= 1;
    }

    unsafe fn add_chunk(&mut self, chunk: NonNull<u8>, class: usize) {
        let header = chunk.as_ptr() as *mut SlabChunkHeader;
        let data_start = align_up_usize(
            chunk.as_ptr() as usize + size_of::<SlabChunkHeader>(),
            self.slot_size,
        );
        let end = chunk.as_ptr() as usize + SLAB_CHUNK_SIZE;
        let total_slots = (end - data_start) / self.slot_size;
        assert!(total_slots > 0);

        *header = SlabChunkHeader {
            magic: SLAB_MAGIC,
            class,
            slot_size: self.slot_size,
            total_slots,
            free_slots: total_slots,
            free_head: core::ptr::null_mut(),
            previous: core::ptr::null_mut(),
            next: core::ptr::null_mut(),
        };
        for index in (0..total_slots).rev() {
            let slot = (data_start + index * self.slot_size) as *mut usize;
            *slot = (*header).free_head as usize;
            (*header).free_head = slot;
        }
        #[cfg(feature = "cosmos-meminfo")]
        {
            self.reserved_bytes += SLAB_CHUNK_SIZE;
        }
        #[cfg(feature = "cosmos-meminfo")]
        {
            self.free_bytes += total_slots * self.slot_size;
        }
        self.add_available(header);
    }

    unsafe fn alloc_slot(&mut self, _requested_size: usize) -> Option<NonNull<u8>> {
        let chunk = self.available_head;
        if chunk.is_null() {
            return None;
        }
        let slot = (*chunk).free_head;
        debug_assert!(!slot.is_null());
        (*chunk).free_head = *slot as *mut usize;
        (*chunk).free_slots -= 1;
        #[cfg(feature = "cosmos-meminfo")]
        {
            self.free_bytes -= self.slot_size;
        }
        if (*chunk).free_slots == 0 {
            self.remove_available(chunk);
        }
        #[cfg(feature = "cosmos-meminfo")]
        {
            self.requested_bytes += _requested_size;
        }
        Some(NonNull::new_unchecked(slot as *mut u8))
    }

    /// Return a slot and optionally identify an empty slab chunk that can be
    /// returned to the buddy allocator. The caller must hold the slab lock.
    unsafe fn dealloc_slot(
        &mut self,
        ptr: NonNull<u8>,
        _requested_size: usize,
    ) -> Option<NonNull<u8>> {
        let chunk = (ptr.as_ptr() as usize & !(SLAB_CHUNK_SIZE - 1)) as *mut SlabChunkHeader;
        assert_eq!((*chunk).magic, SLAB_MAGIC);
        assert_eq!((*chunk).slot_size, self.slot_size);
        let was_full = (*chunk).free_slots == 0;
        let slot = ptr.as_ptr() as *mut usize;
        *slot = (*chunk).free_head as usize;
        (*chunk).free_head = slot;
        (*chunk).free_slots += 1;
        #[cfg(feature = "cosmos-meminfo")]
        {
            self.free_bytes += self.slot_size;
        }
        #[cfg(feature = "cosmos-meminfo")]
        {
            self.requested_bytes -= _requested_size;
        }
        if was_full {
            self.add_available(chunk);
        }

        if (*chunk).free_slots == (*chunk).total_slots && self.available_chunks > 1 {
            #[cfg(feature = "cosmos-meminfo")]
            let bytes = (*chunk).total_slots * self.slot_size;
            self.remove_available(chunk);
            #[cfg(feature = "cosmos-meminfo")]
            {
                self.reserved_bytes -= SLAB_CHUNK_SIZE;
            }
            #[cfg(feature = "cosmos-meminfo")]
            {
                self.free_bytes -= bytes;
            }
            Some(NonNull::new_unchecked(chunk as *mut u8))
        } else {
            None
        }
    }
}

struct SmallSlabHeap {
    classes: [SmallSlabClass; SLAB_CLASS_COUNT],
}

unsafe impl Send for SmallSlabHeap {}

impl SmallSlabHeap {
    const fn new() -> Self {
        Self {
            classes: [
                SmallSlabClass::new(16),
                SmallSlabClass::new(32),
                SmallSlabClass::new(64),
                SmallSlabClass::new(128),
                SmallSlabClass::new(256),
            ],
        }
    }

    unsafe fn alloc_slot(&mut self, class: usize, requested_size: usize) -> Option<NonNull<u8>> {
        self.classes[class].alloc_slot(requested_size)
    }

    unsafe fn add_chunk(&mut self, class: usize, chunk: NonNull<u8>) {
        self.classes[class].add_chunk(chunk, class);
    }

    unsafe fn dealloc_slot(
        &mut self,
        class: usize,
        ptr: NonNull<u8>,
        requested_size: usize,
    ) -> Option<NonNull<u8>> {
        self.classes[class].dealloc_slot(ptr, requested_size)
    }

    #[cfg(feature = "cosmos-meminfo")]
    fn stats(&self) -> (usize, usize, usize, usize) {
        let mut reserved = 0;
        let mut free = 0;
        let mut requested = 0;
        let mut largest_free = 0;
        for class in &self.classes {
            reserved += class.reserved_bytes;
            free += class.free_bytes;
            requested += class.requested_bytes;
            if !class.available_head.is_null() {
                largest_free = largest_free.max(class.slot_size);
            }
        }
        (reserved, free, requested, largest_free)
    }
}

fn slab_class_for_layout(layout: Layout) -> Option<usize> {
    let size = max(layout.size(), max(layout.align(), MIN_BUDDY_BLOCK_SIZE));
    if size > SLAB_MAX_SIZE {
        return None;
    }
    Some(size.next_power_of_two().trailing_zeros() as usize - MIN_BUDDY_ORDER)
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
    slabs: SpinNoIrqLock<SmallSlabHeap>,
    #[cfg(feature = "cosmos-meminfo")]
    dealloc_calls: AtomicUsize,
}

impl KernelHeapAllocator {
    const fn new() -> Self {
        Self {
            heap: SpinNoIrqLock::new(ReclaimingHeap::empty()),
            slabs: SpinNoIrqLock::new(SmallSlabHeap::new()),
            #[cfg(feature = "cosmos-meminfo")]
            dealloc_calls: AtomicUsize::new(0),
        }
    }

    fn alloc_slab_chunk(&self) -> Option<NonNull<u8>> {
        loop {
            let allocation = {
                let mut heap = self.heap.lock();
                heap.alloc_raw(SLAB_CHUNK_SIZE, SLAB_CHUNK_SIZE).ok()
            };
            if allocation.is_some() {
                return allocation;
            }
            if !self.grow(SLAB_CHUNK_SIZE) {
                return None;
            }
        }
    }

    fn alloc_slab(&self, class: usize, requested_size: usize) -> Option<NonNull<u8>> {
        let mut slabs = self.slabs.lock();
        let allocation = unsafe { slabs.alloc_slot(class, requested_size) };
        if allocation.is_some() {
            return allocation;
        }
        let chunk = self.alloc_slab_chunk()?;
        unsafe {
            slabs.add_chunk(class, chunk);
            slabs.alloc_slot(class, requested_size)
        }
    }

    fn dealloc_slab(&self, class: usize, ptr: NonNull<u8>, requested_size: usize) {
        let mut slabs = self.slabs.lock();
        let released_chunk = unsafe { slabs.dealloc_slot(class, ptr, requested_size) };
        if let Some(chunk) = released_chunk {
            let mut heap = self.heap.lock();
            heap.dealloc_raw(chunk, SLAB_CHUNK_SIZE, SLAB_CHUNK_SIZE);
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
        // Reserve our disjoint VA range under the lock, then DROP the lock before
        // mapping. `map_heap_pages` allocates frames and takes the independent
        // page-table lock, so keeping this IRQ-disabling virtual-range lock held
        // across the mapping work would create unnecessary lock nesting.
        // Reservations hand out strictly disjoint, monotonically advancing
        // ranges, so mapping `our` range needs no exclusion against a concurrent
        // grow.
        let (virtual_offset, bytes) = {
            let _virtual_guard = KERNEL_HEAP_VIRTUAL_LOCK.lock();
            let Some((virtual_offset, bytes)) = reserve_virtual_heap_bytes_locked(required_bytes)
            else {
                return false;
            };
            (virtual_offset, bytes)
        };
        // debug!(
        //     "Growing virtual kernel heap: {} KiB -> {} KiB",
        //     virtual_offset / 1024,
        //     (virtual_offset + bytes) / 1024
        // );
        let start = KERNEL_HEAP_BASE + virtual_offset;
        if !map_heap_pages(start, bytes / PAGE_SIZE) {
            let _virtual_guard = KERNEL_HEAP_VIRTUAL_LOCK.lock();
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
            // Pop one tail block and snapshot the current high-water under the
            // lock, then DROP the lock before unmapping. `unmap_heap_pages` ends
            // with a kernel-ASID TLB shootdown that busy-waits for every online hart
            // to ack an IPI; holding this IRQ-disabling lock across that wait
            // deadlocks a concurrent grow/reclaim spinning on it with IRQs off.
            // The freed range is always the topmost [start, virtual_bytes), and a
            // concurrent grow only reserves ranges strictly above `virtual_bytes`
            // (it advances the high-water), so the unmapped range is disjoint from
            // anything a grow can touch while the lock is released.
            let (start, bytes, virtual_bytes) = {
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
                (start, bytes, virtual_bytes)
            };
            let pages = bytes / PAGE_SIZE;
            unmap_heap_pages(start, pages);
            reclaimed_pages += pages;
            // Retract the high-water mark only if no concurrent grow advanced it
            // while the lock was released. If one did, the freed range is no
            // longer the top: leave it as an unmapped hole (the frames were
            // already returned by `unmap_heap_pages`, so only VA is wasted, and
            // only on this rare interleaved path).
            {
                let _virtual_guard = KERNEL_HEAP_VIRTUAL_LOCK.lock();
                if KERNEL_HEAP_VIRTUAL_BYTES.load(Ordering::Acquire) == virtual_bytes {
                    KERNEL_HEAP_VIRTUAL_BYTES.store(virtual_bytes - bytes, Ordering::Release);
                }
            }
            KERNEL_HEAP_BYTES.fetch_sub(bytes, Ordering::AcqRel);
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
        if let Some(class) = slab_class_for_layout(layout) {
            let Some(allocation) = self.alloc_slab(class, layout.size()) else {
                return null_mut();
            };
            KERNEL_HEAP_USED_BYTES.fetch_add(layout.size(), Ordering::AcqRel);
            return allocation.as_ptr();
        }
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
        #[cfg(feature = "cosmos-meminfo")]
        {
            self.dealloc_calls.fetch_add(1, Ordering::AcqRel);
        }
        if let Some(class) = slab_class_for_layout(layout) {
            self.dealloc_slab(class, core::ptr::NonNull::new_unchecked(ptr), layout.size());
            KERNEL_HEAP_USED_BYTES.fetch_sub(layout.size(), Ordering::AcqRel);
            return;
        }
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

/// Return a consistent snapshot of the kernel heap allocator state.
#[cfg(feature = "cosmos-meminfo")]
pub fn kernel_heap_allocator_stats() -> KernelHeapAllocatorStats {
    // Allocation takes the slab lock before the buddy lock when a slab grows;
    // keep the same order here to avoid a lock-order inversion.
    let slabs = HEAP_ALLOCATOR.slabs.lock();
    let heap = HEAP_ALLOCATOR.heap.lock();
    let (slab_reserved, slab_free, slab_requested, slab_largest) = slabs.stats();
    let mut stats = heap.stats();
    stats.requested_bytes += slab_requested;
    stats.allocated_bytes = stats.allocated_bytes.saturating_sub(slab_free);
    stats.actual_free_bytes += slab_free;
    stats.largest_free_bytes = max(stats.largest_free_bytes, slab_largest);
    stats.free_calls = HEAP_ALLOCATOR.dealloc_calls.load(Ordering::Acquire);
    stats.slab_reserved_bytes = slab_reserved;
    stats.slab_free_bytes = slab_free;
    stats
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
        // Fresh heap mappings install leaf PTEs over slots that were *invalid*
        // (we rollback on any already-valid entry above), so no hart can hold a
        // stale TLB entry for these VAs. Cross-CPU invalidation is therefore
        // unnecessary — Linux likewise never IPIs for a brand-new anonymous
        // mapping; remote harts only ever touch this memory after the allocator
        // hands it out, by which time the PTE store is globally coherent.
        //
        // We still need a *local* sfence.vma so THIS hart's page-table walker
        // observes the just-installed PTEs before `add_to_heap` writes the free
        // list into the new pages.
        //
        // The previous global shootdown here was the dominant source of
        // synchronous all-CPU IPI barriers under allocation pressure
        // (iperf/fork), and — critically — a single non-acking hart would wedge
        // `TLB_SHOOTDOWN_LAUNCH_LOCK` and hang every other grower (observed SMP
        // lockup: a stuck shootdown launcher blocks all subsequent ones). The
        // synchronous global flush belongs only on the unmap/reclaim path, where
        // stale translations genuinely exist elsewhere.
        unsafe { crate::hal::flush_tlb() };
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
    let mut batch_start = 0usize;
    while batch_start < pages {
        let batch_end = min(
            batch_start.saturating_add(KERNEL_HEAP_UNMAP_BATCH_PAGES),
            pages,
        );
        let mut reclaimed_ppns = [0usize; KERNEL_HEAP_UNMAP_BATCH_PAGES];
        let mut reclaimed_count = 0usize;
        {
            let _guard = HEAP_PT_LOCK.lock();
            for page in batch_start..batch_end {
                let va = start_va + page * PAGE_SIZE;
                let vpn = VirtAddr::from(va).floor();
                let Some(pte) = existing_heap_leaf_pte(subtree_root_ppn, vpn) else {
                    continue;
                };
                let entry = unsafe { &mut *pte };
                if !entry.is_valid() {
                    continue;
                }
                reclaimed_ppns[reclaimed_count] = entry.ppn().0;
                reclaimed_count += 1;
                // Make the translation unreachable before asking every hart to
                // discard a possibly cached copy.  The frame must remain owned
                // until that shootdown has completed.
                *entry = PageTableEntry::empty();
            }
        }

        if reclaimed_count != 0 {
            // Do not hold HEAP_PT_LOCK across this synchronous IPI barrier: a
            // target hart may currently be spinning on that IRQ-disabling lock.
            // The heap subtree is shared by every process root and marked
            // global at its root entry, so stale translations must be removed
            // from every hart including global TLB entries.
            crate::mm::shootdown_global_quiet();
            for ppn in reclaimed_ppns[..reclaimed_count].iter().copied() {
                frame_dealloc(PhysPageNum(ppn));
            }
        }
        batch_start = batch_end;
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
