//! SV39 paging implementation of [`PagingArch`](crate::hal::traits::PagingArch).

use crate::hal::traits::{AddressSpaceToken, PagingArch};
use crate::mm::PageTableEntry;

/// RISC-V Sv39 three-level paging implementation.
pub struct Sv39Paging;

impl PagingArch for Sv39Paging {
    type Entry = PageTableEntry;
    const ROOT_TOKEN_MODE: usize = 8; // MODE=8 → Sv39
    const LEVELS: usize = 3;

    fn make_token(root_ppn: usize) -> AddressSpaceToken {
        Self::ROOT_TOKEN_MODE << 60 | root_ppn
    }

    fn root_ppn(token: AddressSpaceToken) -> usize {
        token & ((1usize << 44) - 1)
    }

    unsafe fn activate_token(token: AddressSpaceToken) {
        use riscv::register::satp;
        satp::write(token);
        core::arch::asm!("sfence.vma");
    }

    unsafe fn current_token() -> AddressSpaceToken {
        riscv::register::satp::read().bits()
    }

    unsafe fn flush_tlb() {
        core::arch::asm!("sfence.vma");
    }
}
