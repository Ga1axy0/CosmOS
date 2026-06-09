//! LoongArch64 paging implementation of [`PagingArch`](crate::hal::traits::PagingArch).

use core::arch::asm;

use crate::hal::traits::{AddressSpaceToken, PTEFlags, PagingArch};
use crate::mm::PageTableEntry;

const CSR_PGDL: usize = 0x19;
const CSR_ASID: usize = 0x18;
const PTE_V: usize = 1 << 0;
const PTE_D: usize = 1 << 1;
const PTE_PLV_USER: usize = 0b11 << 2;
const PTE_MAT_CC: usize = 0b01 << 4;
const PTE_G: usize = 1 << 6;
const PTE_P: usize = 1 << 7;
const PTE_W: usize = 1 << 8;
const PTE_A: usize = 1 << 10;
const PTE_GNX: usize = 1 << 62;
const PTE_GNR: usize = 1 << 61;
const PPN_SHIFT: usize = 12;

/// LoongArch64 three-level paging (39-bit VA, matching PWCL Dir1/Dir2 setup).
pub struct LoongArchPaging;

impl PagingArch for LoongArchPaging {
    type Entry = PageTableEntry;
    const PA_BITS: usize = 48;
    const VA_BITS: usize = 39;
    const PPN_BITS: usize = Self::PA_BITS - 12;
    const ROOT_TOKEN_MODE: usize = 0;
    const LEVELS: usize = 3;
    const INDEX_BITS: usize = 9;

    fn make_token(root_ppn: usize) -> AddressSpaceToken {
        root_ppn << PPN_SHIFT
    }

    fn root_ppn(token: AddressSpaceToken) -> usize {
        token >> PPN_SHIFT
    }

    unsafe fn activate_token(token: AddressSpaceToken) {
        asm!(
            "dbar 0",
            "csrwr {pgd}, {pgdl}",
            "csrwr $zero, {asid}",
            "invtlb 0x00, $zero, $zero",
            "ibar 0",
            pgd = in(reg) token,
            pgdl = const CSR_PGDL,
            asid = const CSR_ASID,
        );
    }

    unsafe fn current_token() -> AddressSpaceToken {
        let token: usize;
        asm!("csrrd {}, {}", out(reg) token, const CSR_PGDL);
        token
    }

    unsafe fn flush_tlb() {
        asm!(
            "dbar 0",
            "invtlb 0x00, $zero, $zero",
            "ibar 0",
        );
    }

    fn make_pte(ppn: usize, flags: PTEFlags) -> usize {
        let mut bits = (ppn << PPN_SHIFT) | PTE_P | PTE_V | PTE_MAT_CC;
        if flags.contains(PTEFlags::A) {
            bits |= PTE_A;
        }
        if flags.contains(PTEFlags::W) {
            bits |= PTE_W | PTE_D;
        }
        if flags.contains(PTEFlags::U) {
            bits |= PTE_PLV_USER;
        }
        if flags.contains(PTEFlags::G) {
            bits |= PTE_G;
        }
        if !flags.contains(PTEFlags::R) {
            bits |= PTE_GNR;
        }
        if !flags.contains(PTEFlags::X) {
            bits |= PTE_GNX;
        }
        bits
    }

    fn make_dir_entry(ppn: usize) -> usize {
        // LoongArch hardware walkers consume non-leaf directory entries as the
        // physical address of the next-level table. Keep them as a bare next
        // table pointer instead of reusing leaf-style permission bits.
        ppn << PPN_SHIFT
    }

    fn pte_ppn(entry_bits: usize) -> usize {
        (entry_bits >> PPN_SHIFT) & ((1usize << Self::PPN_BITS) - 1)
    }

    fn pte_flags(entry_bits: usize) -> PTEFlags {
        let mut flags = PTEFlags::empty();
        if entry_bits & PTE_V != 0 {
            flags |= PTEFlags::V;
        }
        if entry_bits & PTE_W != 0 {
            flags |= PTEFlags::W;
        }
        if entry_bits & PTE_D != 0 {
            flags |= PTEFlags::D;
        }
        if entry_bits & PTE_A != 0 {
            flags |= PTEFlags::A;
        }
        if entry_bits & PTE_PLV_USER != 0 {
            flags |= PTEFlags::U;
        }
        if entry_bits & PTE_G != 0 {
            flags |= PTEFlags::G;
        }
        if entry_bits & PTE_GNR == 0 {
            flags |= PTEFlags::R;
        }
        if entry_bits & PTE_GNX == 0 {
            flags |= PTEFlags::X;
        }
        flags
    }

    fn vpn_index(vpn: usize, level: usize) -> usize {
        debug_assert!(level < Self::LEVELS);
        let mask = (1usize << Self::INDEX_BITS) - 1;
        let shift = (Self::LEVELS - 1 - level) * Self::INDEX_BITS;
        (vpn >> shift) & mask
    }
}
