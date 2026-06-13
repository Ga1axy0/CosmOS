//! SV39 paging implementation of [`PagingArch`](crate::hal::traits::PagingArch).

use crate::hal::traits::{AddressSpaceToken, PTEFlags, PagingArch};
use crate::mm::PageTableEntry;

/// RISC-V Sv39 three-level paging implementation.
pub struct Sv39Paging;

impl PagingArch for Sv39Paging {
    type Entry = PageTableEntry;
    const PA_BITS: usize = 56;
    const VA_BITS: usize = 39;
    const PPN_BITS: usize = Self::PA_BITS - 12;
    const ROOT_TOKEN_MODE: usize = 8; // MODE=8 → Sv39
    const LEVELS: usize = 3;
    const INDEX_BITS: usize = 9;

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

    fn make_pte(ppn: usize, flags: PTEFlags) -> usize {
        ppn << 10 | flags.bits() as usize
    }

    fn pte_ppn(entry_bits: usize) -> usize {
        entry_bits >> 10 & ((1usize << 44) - 1)
    }

    fn pte_flags(entry_bits: usize) -> PTEFlags {
        PTEFlags::from_bits_truncate(entry_bits as u16)
    }

    fn normalize_virt_addr_input(bits: usize) -> usize {
        bits & ((1usize << Self::VA_BITS) - 1)
    }

    fn vpn_index(vpn: usize, level: usize) -> usize {
        debug_assert!(level < Self::LEVELS);
        let mask = (1usize << Self::INDEX_BITS) - 1;
        let shift = (Self::LEVELS - 1 - level) * Self::INDEX_BITS;
        (vpn >> shift) & mask
    }
}
