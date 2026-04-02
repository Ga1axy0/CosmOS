//! Constants in the kernel

#[allow(unused)]

/// user app's stack size (increased to avoid user-space stack overflow for glibc/busybox)
pub const USER_STACK_SIZE: usize = 1024 * 16; // 16 KiB
/// kernel stack size
pub const KERNEL_STACK_SIZE: usize = 4096 * 4;
/// kernel heap size
pub const KERNEL_HEAP_SIZE: usize = 0x200_0000;
/// max harts reserved by the kernel SMP bootstrap path
pub const MAX_HARTS: usize = 8;
/// physical memory end address
pub const MEMORY_END: usize = 0x88000000;
/// page size : 4KB
pub const PAGE_SIZE: usize = 0x1000;
/// page size bits: 12
pub const PAGE_SIZE_BITS: usize = 0xc;
/// default base address for anonymous mmap allocations
pub const USER_MMAP_BASE: usize = 0x1000_0000;
/// default base address for the main thread's user stack region
pub const USER_STACK_BASE: usize = 0x0800_0000;
/// the virtual addr of trapoline
pub const TRAMPOLINE: usize = usize::MAX - PAGE_SIZE + 1;
/// the virtual addr of trap context
pub const TRAP_CONTEXT_BASE: usize = TRAMPOLINE - PAGE_SIZE;
/// qemu board info
pub use crate::board::{CLOCK_FREQ, MMIO};
