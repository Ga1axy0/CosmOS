//! RISC-V hart-local register access, implementing [`HartId`](crate::hal::traits::HartId).

use crate::hal::traits::HartId;
use core::arch::asm;
use riscv::{
    asm::wfi,
    register::{mstatus::FS, sstatus},
};

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

    #[inline]
    unsafe fn enable_fp() {
        // Do not erase Dirty: trap return uses the live FS state to decide
        // whether userspace FP registers can be reused without reloading them.
        if sstatus::read().fs() == FS::Off {
            sstatus::set_fs(FS::Initial);
        }
    }

    #[inline]
    fn irqs_enabled() -> bool {
        sstatus::read().sie()
    }

    #[inline]
    unsafe fn disable_irqs() {
        sstatus::clear_sie();
    }

    #[inline]
    unsafe fn enable_irqs() {
        sstatus::set_sie();
    }

    #[inline]
    unsafe fn wait_for_interrupt() {
        wfi();
    }
}
