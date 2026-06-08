//! HAL — re-exports arch/platform concrete types under stable aliases.
#![allow(missing_docs)]

pub mod traits;

use crate::hal::traits::{AddressSpaceToken, HartId, PagingArch};

#[cfg(target_arch = "riscv64")]
pub use crate::arch::riscv::{
    RiscvHartId as ArchHart, RiscvInterruptControl as ArchInterrupt,
    RiscvTrapContextAbi as ArchTrapContextAbi,
    RiscvTrapMachine as ArchTrapMachine, Sv39Paging as ArchPaging,
};

#[cfg(feature = "platform-qemu-virt")]
pub use crate::platform::qemu_virt::SbiPlatform as Plat;

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
