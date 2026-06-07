//! RISC-V interrupt control, implementing [`InterruptControl`](crate::hal::traits::InterruptControl).

use core::arch::asm;
use riscv::register::{
    mtvec::TrapMode,
    scause::{self, Exception, Interrupt, Trap},
    sie, stval, stvec,
};
use crate::config::TRAMPOLINE;
use crate::hal::traits::{InterruptControl, TrapCause, TrapInfo, TrapMachine};

/// RISC-V implementation of [`InterruptControl`](crate::hal::traits::InterruptControl).
pub struct RiscvInterruptControl;

/// RISC-V implementation of trap decoding and user-return operations.
pub struct RiscvTrapMachine;

/// 用户态 `rt_sigreturn` trampoline 机器码。
const USER_VDSO_CODE: [u8; 8] = [
    0x93, 0x08, 0xb0, 0x08, // addi a7, zero, 139
    0x73, 0x00, 0x00, 0x00, // ecall
];

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

impl TrapMachine for RiscvTrapMachine {
    fn read_trap_info() -> TrapInfo {
        let cause = match scause::read().cause() {
            Trap::Exception(Exception::UserEnvCall) => TrapCause::UserSyscall,
            Trap::Exception(Exception::StorePageFault) => TrapCause::StorePageFault,
            Trap::Exception(Exception::LoadPageFault) => TrapCause::LoadPageFault,
            Trap::Exception(Exception::InstructionPageFault) => TrapCause::InstructionPageFault,
            Trap::Exception(Exception::StoreFault) => TrapCause::StoreFault,
            Trap::Exception(Exception::InstructionFault) => TrapCause::InstructionFault,
            Trap::Exception(Exception::LoadFault) => TrapCause::LoadFault,
            Trap::Exception(Exception::IllegalInstruction) => TrapCause::IllegalInstruction,
            Trap::Interrupt(Interrupt::SupervisorTimer) => TrapCause::TimerInterrupt,
            Trap::Interrupt(Interrupt::SupervisorSoft) => TrapCause::SoftwareInterrupt,
            Trap::Interrupt(Interrupt::SupervisorExternal) => TrapCause::ExternalInterrupt,
            _ => TrapCause::Unknown,
        };
        TrapInfo {
            cause,
            fault_addr: stval::read(),
        }
    }

    unsafe fn return_to_user(trap_cx_user_va: usize, user_token: usize) -> ! {
        extern "C" {
            fn __alltraps();
            fn __restore();
        }
        let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;
        asm!(
            "fence.i",
            "jr {restore_va}",
            restore_va = in(reg) restore_va,
            in("a0") trap_cx_user_va,
            in("a1") user_token,
            options(noreturn)
        );
    }

    fn syscall_instruction_len() -> usize {
        4
    }

    fn rt_sigreturn_trampoline() -> &'static [u8] {
        &USER_VDSO_CODE
    }
}
