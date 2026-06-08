//! RISC-V hart-local register access, implementing [`HartId`](crate::hal::traits::HartId).

use core::arch::asm;
use crate::hal::traits::HartId;

/// RISC-V implementation of [`HartId`](crate::hal::traits::HartId) via the `tp` register.
pub struct RiscvHartId;

impl HartId for RiscvHartId {
    #[inline]
    fn current() -> usize {
        let id;
        unsafe { asm!("mv {}, tp", out(reg) id) }
        id
    }

    #[inline]
    unsafe fn init(id: usize) {
        asm!("mv tp, {}", in(reg) id);
    }
}