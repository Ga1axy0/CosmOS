//! Memory management implementation
//!
//! SV39 page-based virtual-memory architecture for RV64 systems, and
//! everything about memory management, like frame allocator, page table,
//! map area and memory set, is implemented here.
//!
//! Every task or process has a memory_set to control its virtual memory.

mod address;
mod asid;
mod elf_loader;
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
    /// The requested memory-management operation is not supported for the
    /// selected mapping type.
    Unsupported,
    /// The requested address range is not available for an in-place mapping.
    AddressUnavailable,
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

pub use crate::hal::traits::PTEFlags;
pub use address::{
    phys_to_virt, virt_to_phys, PhysAddr, PhysPageNum, StepByOne, VirtAddr, VirtPageNum,
    USER_SPACE_END,
};
pub use asid::KERNEL_ASID;
pub use elf_loader::ElfLoadInfo;
pub use frame_allocator::{
    frame_alloc, frame_alloc_contiguous, frame_alloc_with_reclaim, frame_allocator_stats,
    frame_dealloc, frame_dealloc_range, ContiguousFrames, FrameAllocatorStats, FrameTracker,
};
#[cfg(feature = "cosmos-meminfo")]
pub use heap_allocator::{kernel_heap_allocator_stats, KernelHeapAllocatorStats};
pub use heap_allocator::{
    map_one_heap_page, reclaim_kernel_heap_if_needed, KERNEL_HEAP_BYTES, KERNEL_HEAP_USED_BYTES,
};
pub use memory_set::remap_test;
#[cfg(feature = "cosmos-meminfo")]
pub use memory_set::{
    anonymous_page_stats, record_anonymous_zero_page_map_hit,
    record_anonymous_zero_page_write_materialization, reset_anonymous_page_stats,
    AnonymousPageStats,
};
pub use memory_set::{
    invalidate_inode_mappings_after_truncate, kernel_token, register_file_mapping,
    unregister_file_mappings_for_process, DeferredUserReclaim, FilePageFaultPrepare, InodeKey,
    MapPermission, MemorySet, PageFaultAccess, UserSpaceLayout, Vma, VmaKind, KERNEL_SPACE,
};
pub(crate) use memory_set::{FileMappingSyncPlan, SharedMemorySetState};
pub use oom::{log_oom, warn_heap_state};
#[cfg(feature = "cosmos-meminfo")]
pub use page_table::{page_table_stats, reset_page_table_stats, PageTableStats};
pub use page_table::{
    translated_byte_buffer, translated_ref, translated_refmut, translated_str, AddressSpaceRoot,
    PageTable, PageTableEntry, UserBuffer, UserBufferIterator,
};
pub use tlb_shootdown::{
    clear_deferred, defer_release, deferred_frame_count, deferred_kstack_id_count,
    deferred_range_count, flush_deferred, handle_ipi, has_deferred, mark_online, needs_flush,
    online_mask, poll_pending_shootdown, shootdown, shootdown_asid, shootdown_asid_quiet,
    shootdown_global, shootdown_global_quiet, shootdown_page, shootdown_page_quiet,
    shootdown_range, shootdown_range_quiet, take_deferred, DeferredBatch, ShootdownKind,
};
#[cfg(feature = "cosmos-meminfo")]
pub use tlb_shootdown::{reset_tlb_shootdown_stats, tlb_shootdown_stats, TlbShootdownStats};

/// initiate heap allocator, frame allocator and kernel space
pub fn init() {
    frame_allocator::init_frame_allocator();
    #[cfg(feature = "cosmos-meminfo")]
    reset_page_table_stats();
    #[cfg(feature = "cosmos-meminfo")]
    reset_tlb_shootdown_stats();
    #[cfg(feature = "cosmos-meminfo")]
    reset_anonymous_page_stats();
    heap_allocator::init_heap();
    let kernel_space = &*KERNEL_SPACE;
    let kernel_space_guard = kernel_space.lock();
    kernel_space_guard.activate();
    drop(kernel_space_guard);
    asid::init();
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
