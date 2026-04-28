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
mod page_table;
mod tlb_shootdown;

use address::VPNRange;
pub use address::{PhysAddr, PhysPageNum, StepByOne, VirtAddr, VirtPageNum};
pub use frame_allocator::{frame_alloc, frame_dealloc, FrameTracker};
pub use memory_set::remap_test;
pub use memory_set::{
    kernel_token, MapPermission, MemorySet, PageFaultAccess, UserSpaceLayout, Vma, VmaKind,
    KERNEL_SPACE,
};
pub use tlb_shootdown::{
    clear_deferred_kernel_recycle_state, deferred_kernel_frame_count,
    deferred_kernel_va_range_count, has_deferred_kernel_recycle_work,
    kernel_va_range_requires_flush, note_deferred_kernel_va_release,
};
use page_table::PTEFlags;
pub use page_table::{
    translated_byte_buffer, translated_ref, translated_refmut, translated_str, PageTable,
    PageTableEntry, UserBuffer, UserBufferIterator,
};

/// initiate heap allocator, frame allocator and kernel space
pub fn init() {
    heap_allocator::init_heap();
    frame_allocator::init_frame_allocator();
    KERNEL_SPACE.lock().activate();
}

/// 在当前 hart 上激活内核地址空间（写入 satp + sfence.vma）。
///
/// 此函数供 secondary harts 在 bootstrap 完成后调用，因为 `satp` 是
/// per-hart 寄存器，`mm::init()` 只激活了 bootstrap hart 的 satp。
pub fn activate_kernel_space() {
    KERNEL_SPACE.lock().activate();
}
