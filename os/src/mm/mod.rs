//! Memory management implementation
//!
//! SV39 page-based virtual-memory architecture for RV64 systems, and
//! everything about memory management, like frame allocator, page table,
//! map area and memory set, is implemented here.
//!
//! Every task or process has a memory_set to control its virtual memory.

mod address;
mod frame_allocator;
mod heap_allocator;
mod memory_set;
mod oom;
mod page_table;
mod tlb_shootdown;

use address::VPNRange;

/// Internal memory-management error used below syscall/trap ABI boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MmError {
    /// No frame or page-table memory is available.
    OutOfMemory,
    /// The requested virtual-memory range is malformed.
    InvalidRange,
    /// The requested mapping conflicts with an existing VMA or PTE state.
    Conflict,
    /// No matching mapping or page-table entry exists.
    NoMapping,
    /// The attempted access violates mapping permissions.
    PermissionDenied,
    /// A file-backed fault reached beyond the file's logical end.
    BeyondFileEnd,
    /// ELF metadata is invalid during address-space construction.
    InvalidElf,
}

/// Outcome of one page-fault sub-handler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageFaultHandled {
    /// The fault matched this handler and was resolved.
    Handled,
    /// The fault does not belong to this handler.
    NotHandled,
}

pub use address::{phys_to_virt, virt_to_phys, PhysAddr, PhysPageNum, StepByOne, USER_SPACE_END, VirtAddr, VirtPageNum};
pub use frame_allocator::{
    frame_alloc, frame_alloc_contiguous, frame_alloc_with_reclaim, frame_allocator_stats,
    frame_dealloc, frame_dealloc_range, ContiguousFrames, FrameAllocatorStats, FrameTracker,
};
pub use heap_allocator::{map_one_heap_page, KERNEL_HEAP_USED_BYTES};
pub use memory_set::remap_test;
pub use memory_set::{
    invalidate_inode_mappings_after_truncate, kernel_token, register_file_mapping,
    unregister_file_mappings_for_process, DeferredUserReclaim, ElfLoadInfo, InodeKey,
    MapPermission, MemorySet, PageFaultAccess, UserSpaceLayout, Vma, VmaKind, KERNEL_SPACE,
};
pub use oom::{log_oom, warn_heap_state, warn_heap_state_lockfree};
pub use tlb_shootdown::{
    clear_deferred, deferred_frame_count, deferred_kstack_id_count, deferred_range_count,
    defer_release, flush_deferred, handle_ipi, has_deferred, mark_online, needs_flush, online_mask, shootdown,
    shootdown_global, shootdown_global_quiet, take_deferred, DeferredBatch, ShootdownKind,
};
pub use page_table::{
    translated_byte_buffer, translated_ref, translated_refmut, translated_str, PageTable,
    PageTableEntry, UserBuffer, UserBufferIterator,
};
pub use crate::hal::traits::PTEFlags;

/// initiate heap allocator, frame allocator and kernel space
pub fn init() {
    frame_allocator::init_frame_allocator();
    heap_allocator::init_heap();
    KERNEL_SPACE.lock().activate();
    heap_allocator::init_kernel_heap_mapping();
    heap_allocator::init_heap_virtual_window();
}

/// 在当前 hart 上激活内核地址空间（写入 satp + sfence.vma）。
///
/// 此函数供 secondary harts 在 bootstrap 完成后调用，因为 `satp` 是
/// per-hart 寄存器，`mm::init()` 只激活了 bootstrap hart 的 satp。
pub fn activate_kernel_space() {
    KERNEL_SPACE.lock().activate();
}
