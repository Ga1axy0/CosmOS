//! Constants in the kernel

#[allow(unused)]

/// user app's stack size (increased to avoid user-space stack overflow for glibc/busybox)
pub const USER_STACK_SIZE: usize = 1024 * 512; // 512 KiB
/// kernel stack size
pub const KERNEL_STACK_SIZE: usize = 4096 * 32;
/// kernel heap size
pub const MAX_KERNEL_HEAP_SIZE: usize = 0x4000_0000;
/// max harts reserved by the kernel SMP bootstrap path
pub const MAX_HARTS: usize = 8;
/// Fallback physical memory end used only when firmware memory discovery fails.
pub const MEMORY_END: usize = 0xC0000000;
/// page size : 4KB
pub const PAGE_SIZE: usize = 0x1000;
/// page size bits: 12
pub const PAGE_SIZE_BITS: usize = 0xc;
/// fixed load bias used for PIE main executables without an interpreter
pub const USER_PIE_BASE: usize = 0x0020_0000;

/// qemu board info
pub use crate::platform::{
    CLOCK_FREQ, INTERP_BASE, KERNEL_HEAP_BASE, MMIO, TRAMPOLINE, USER_MMAP_BASE, USER_STACK_BASE,
};

/// the virtual addr of trap context
pub const TRAP_CONTEXT_BASE: usize = TRAMPOLINE - PAGE_SIZE;
/// 用户态 signal trampoline 页起始地址。
pub const USER_VDSO_BASE: usize = USER_MMAP_BASE - PAGE_SIZE;
/// 用户态 rt_sigreturn trampoline 入口地址。
pub const USER_VDSO_RT_SIGRETURN: usize = USER_VDSO_BASE;
