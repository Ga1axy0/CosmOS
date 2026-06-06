//! RISC-V interrupt control, implementing [`InterruptControl`](crate::hal::traits::InterruptControl).

use core::arch::asm;
use riscv::register::{mtvec::TrapMode, sie, stvec};
use crate::config::TRAMPOLINE;
use crate::hal::traits::InterruptControl;

/// RISC-V implementation of [`InterruptControl`](crate::hal::traits::InterruptControl).
pub struct RiscvInterruptControl;

impl InterruptControl for RiscvInterruptControl {
    unsafe fn enable_timer() { sie::set_stimer(); }
    unsafe fn disable_timer() { sie::clear_stimer(); }
    unsafe fn enable_external() { sie::set_sext(); }
    unsafe fn disable_external() { sie::clear_sext(); }
    unsafe fn enable_software() { sie::set_ssoft(); }

    unsafe fn clear_software_pending() {
        asm!("csrc sip, {}", in(reg) 1usize << 1);
    }

    unsafe fn set_kernel_trap_entry() {
        extern "C" { fn __trap_from_kernel(); }
        stvec::write(__trap_from_kernel as usize, TrapMode::Direct);
    }

    unsafe fn set_user_trap_entry() {
        stvec::write(TRAMPOLINE, TrapMode::Direct);
    }
}
