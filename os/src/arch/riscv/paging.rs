//! SV39 paging implementation of [`PagingArch`](crate::hal::traits::PagingArch).

use crate::hal::traits::{AddressSpaceToken, PTEFlags, PagingArch};
use crate::mm::PageTableEntry;

const SATP_ASID_SHIFT: usize = 44;
const SATP_ASID_BITS: usize = 16;
const SATP_ASID_MASK: usize = ((1usize << SATP_ASID_BITS) - 1) << SATP_ASID_SHIFT;
const PAGE_SIZE: usize = 4096;
// QEMU 10.1.x lowers every SFENCE.VMA operand form to the same full local TLB
// flush. Keep a precise fence for a single page, but avoid repeating that full
// flush for every page in a range. The current RISC-V platform is QEMU virt.
const TLB_RANGE_PAGE_LIMIT: usize = 1;

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

    fn with_address_space_id(token: AddressSpaceToken, asid: usize) -> AddressSpaceToken {
        (token & !SATP_ASID_MASK) | ((asid & ((1usize << SATP_ASID_BITS) - 1)) << SATP_ASID_SHIFT)
    }

    fn address_space_id(token: AddressSpaceToken) -> usize {
        (token & SATP_ASID_MASK) >> SATP_ASID_SHIFT
    }

    unsafe fn probe_address_space_id_mask() -> usize {
        use riscv::register::satp;

        // `satp.ASID` is WARL.  Write ones while retaining the active mode and
        // root PPN, read back the implemented low bits, then restore the
        // original kernel token.  This runs once during bootstrap.
        let original = satp::read().bits();
        satp::write((original & !SATP_ASID_MASK) | SATP_ASID_MASK);
        let implemented = Self::address_space_id(satp::read().bits());
        satp::write(original);
        core::arch::asm!("sfence.vma x0, x0");

        let low_bits = implemented.trailing_ones() as usize;
        if low_bits == 0 {
            0
        } else {
            (1usize << low_bits) - 1
        }
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
        core::arch::asm!("sfence.vma x0, x0");
    }

    unsafe fn flush_tlb_asid(asid: usize) {
        // `rs1=x0` means every virtual address, while a non-x0 `rs2`
        // selects exactly one ASID.  Passing a numeric zero through a normal
        // register therefore still targets ASID 0 rather than all ASIDs.
        core::arch::asm!("sfence.vma x0, {asid}", asid = in(reg) asid);
    }

    unsafe fn flush_tlb_page_asid(vaddr: usize, asid: usize) {
        // Both operands use ordinary registers so numeric zero remains a valid
        // virtual address or ASID rather than selecting the x0 wildcard form.
        core::arch::asm!(
            "sfence.vma {vaddr}, {asid}",
            vaddr = in(reg) vaddr,
            asid = in(reg) asid,
        );
    }

    unsafe fn flush_tlb_range_asid(start: usize, end: usize, asid: usize) {
        if start >= end {
            return;
        }

        let first_page = start & !(PAGE_SIZE - 1);
        let last_page = end.saturating_sub(1) & !(PAGE_SIZE - 1);
        let page_count = (last_page - first_page) / PAGE_SIZE + 1;
        if page_count > TLB_RANGE_PAGE_LIMIT {
            Self::flush_tlb_asid(asid);
            return;
        }

        let mut page_addr = first_page;
        loop {
            Self::flush_tlb_page_asid(page_addr, asid);
            if page_addr == last_page {
                break;
            }
            page_addr += PAGE_SIZE;
        }
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

    fn normalize_leaf_flags(mut flags: PTEFlags) -> PTEFlags {
        // Some physical RISC-V implementations trap instead of updating A/D
        // in hardware. Seed them for resident leaves so the first fetch/load
        // after switching SATP cannot recursively fault on the trap mapping.
        flags.insert(PTEFlags::A);
        if flags.contains(PTEFlags::W) {
            flags.insert(PTEFlags::D);
        }
        flags
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
