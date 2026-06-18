//! HAL — re-exports arch/platform concrete types under stable aliases.
#![allow(missing_docs)]

pub mod traits;

use crate::hal::traits::{AddressSpaceToken, HartId, PTEFlags, PagingArch};

#[cfg(target_arch = "riscv64")]
pub use crate::arch::riscv::{
    RiscvHartId as ArchHart, RiscvInterruptControl as ArchInterrupt,
    RiscvSignalAbi as ArchSignalAbi, RiscvSyscallAbi as ArchSyscallAbi,
    RiscvTrapContextAbi as ArchTrapContextAbi,
    RiscvTrapMachine as ArchTrapMachine, Sv39Paging as ArchPaging,
};

#[cfg(target_arch = "loongarch64")]
pub use crate::arch::loongarch64::{
    LoongArchHartId as ArchHart, LoongArchInterruptControl as ArchInterrupt,
    LoongArchSignalAbi as ArchSignalAbi, LoongArchSyscallAbi as ArchSyscallAbi,
    LoongArchTrapContextAbi as ArchTrapContextAbi,
    LoongArchTrapMachine as ArchTrapMachine, LoongArchPaging as ArchPaging,
};

pub use crate::platform::PlatformImpl as Plat;

/// Return the current hart id.
#[inline]
pub fn hartid() -> usize {
    ArchHart::current()
}

/// Initialize the current hart-local state, including floating-point availability.
#[inline]
pub unsafe fn init_with_hartid(hart_id: usize) -> usize {
    ArchHart::init(hart_id);
    ArchHart::enable_fp();
    hart_id
}

/// Enable the local floating-point unit on the current hart.
#[inline]
pub unsafe fn enable_fp() {
    ArchHart::enable_fp();
}

/// Return whether local interrupts are currently enabled.
#[inline]
pub fn local_irqs_enabled() -> bool {
    ArchHart::irqs_enabled()
}

/// Disable local interrupts on the current hart.
#[inline]
pub unsafe fn disable_local_irqs() {
    ArchHart::disable_irqs();
}

/// Enable local interrupts on the current hart.
#[inline]
pub unsafe fn enable_local_irqs() {
    ArchHart::enable_irqs();
}

/// Wait for the next interrupt/event on the current hart.
#[inline]
pub unsafe fn wait_for_interrupt() {
    ArchHart::wait_for_interrupt();
}

/// Execute the architecture-specific scheduler idle wait sequence.
#[inline]
pub unsafe fn enable_irqs_and_wait() {
    ArchHart::enable_irqs_and_wait();
}

/// Build an architecture-specific address-space token from a root page-table PPN.
#[inline]
pub fn make_address_space_token(root_ppn: usize) -> AddressSpaceToken {
    ArchPaging::make_token(root_ppn)
}

/// Extract the root page-table PPN from an architecture-specific address-space token.
#[inline]
pub fn root_ppn_from_token(token: AddressSpaceToken) -> usize {
    ArchPaging::root_ppn(token)
}

/// Activate the given address space on the current hart.
#[inline]
pub unsafe fn activate_address_space(token: AddressSpaceToken) {
    ArchPaging::activate_token(token);
}

/// Read the current hart's active address-space token.
#[inline]
pub unsafe fn current_address_space_token() -> AddressSpaceToken {
    ArchPaging::current_token()
}

/// Flush the local TLB on the current hart.
#[inline]
pub unsafe fn flush_tlb() {
    ArchPaging::flush_tlb();
}

/// Encode a non-leaf directory PTE (must not set GNR/GNX on LoongArch).
#[inline]
pub fn make_dir_entry(ppn: usize) -> usize {
    ArchPaging::make_dir_entry(ppn)
}

/// Encode one architecture-specific PTE from a physical page number and semantic flags.
#[inline]
pub fn make_pte(ppn: usize, flags: PTEFlags) -> usize {
    ArchPaging::make_pte(ppn, flags)
}

/// Extract the pointed-to physical page number from one architecture-specific PTE.
#[inline]
pub fn pte_ppn(entry_bits: usize) -> usize {
    ArchPaging::pte_ppn(entry_bits)
}

/// Extract the semantic flags from one architecture-specific PTE.
#[inline]
pub fn pte_flags(entry_bits: usize) -> PTEFlags {
    ArchPaging::pte_flags(entry_bits)
}

/// Return whether one raw PTE/directory entry should be treated as present.
#[inline]
pub fn pte_is_valid(entry_bits: usize) -> bool {
    ArchPaging::pte_is_valid(entry_bits)
}

/// Normalize leaf PTE flags for the current architecture.
#[inline]
pub fn normalize_leaf_pte_flags(flags: PTEFlags) -> PTEFlags {
    ArchPaging::normalize_leaf_flags(flags)
}

/// Convert an arbitrary raw virtual address input into the stored form.
#[inline]
pub fn normalize_virt_addr_input(bits: usize) -> usize {
    ArchPaging::normalize_virt_addr_input(bits)
}

/// Convert an arbitrary raw virtual address input into a virtual page number.
#[inline]
pub fn virt_page_num_from_addr(bits: usize) -> usize {
    ArchPaging::virt_page_num_from_addr(bits)
}

/// Return the default trap-context page permissions for this architecture.
#[inline]
pub fn trap_context_flags() -> PTEFlags {
    ArchPaging::trap_context_flags()
}

/// Return the architecture's physical address width in bits.
#[inline]
pub const fn phys_addr_bits() -> usize {
    ArchPaging::PA_BITS
}

/// Return the architecture's virtual address width in bits.
#[inline]
pub const fn virt_addr_bits() -> usize {
    ArchPaging::VA_BITS
}

/// Return the architecture's physical page-number width in bits.
#[inline]
pub const fn phys_page_num_bits() -> usize {
    ArchPaging::PPN_BITS
}

/// Return the architecture's page-table level count.
#[inline]
pub const fn page_table_levels() -> usize {
    ArchPaging::LEVELS
}

/// Return the bit-width of one page-table index.
#[inline]
pub const fn page_table_index_bits() -> usize {
    ArchPaging::INDEX_BITS
}

/// Return the exclusive end of the user address space.
#[inline]
pub fn user_space_end() -> usize {
    ArchPaging::user_space_end()
}

/// Canonicalize one virtual address according to the current architecture.
#[inline]
pub fn canonicalize_vaddr(bits: usize) -> usize {
    ArchPaging::canonicalize_vaddr(bits)
}

/// Return the page-table index at one level for the given virtual page number.
#[inline]
pub fn vpn_index(vpn: usize, level: usize) -> usize {
    ArchPaging::vpn_index(vpn, level)
}
