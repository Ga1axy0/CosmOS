//! SV39 paging implementation of [`PagingArch`](crate::hal::traits::PagingArch).

use crate::hal::traits::PagingArch;
use crate::mm::PageTableEntry;

/// RISC-V Sv39 three-level paging implementation.
pub struct Sv39Paging;

impl PagingArch for Sv39Paging {
    type Entry = PageTableEntry;
    const SATP_MODE: usize = 8; // MODE=8 → Sv39
    const LEVELS: usize = 3;

    unsafe fn activate(root_ppn: usize) {
        use riscv::register::satp;
        satp::write(Self::SATP_MODE << 60 | root_ppn);
        core::arch::asm!("sfence.vma");
    }

    unsafe fn current_token() -> usize {
        riscv::register::satp::read().bits()
    }

    unsafe fn flush_tlb() {
        core::arch::asm!("sfence.vma");
    }
}
