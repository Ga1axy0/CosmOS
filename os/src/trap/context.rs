//! Implementation of [`TrapContext`]
use crate::hal::ArchTrapContextAbi;
use crate::hal::traits::{NamedReg, TrapContextAbi};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
/// trap context structure containing sstatus, sepc and registers
pub struct TrapContext {
    /// Architecture-specific trap-frame payload consumed by trampoline assembly.
    pub arch: <ArchTrapContextAbi as TrapContextAbi>::Frame,
    /// Whether the current trap originated from a syscall (UserEnvCall).
    /// Used by signal delivery to implement syscall restart (SA_RESTART).
    pub in_syscall: bool,
    /// Original a0 value before the syscall overwrote it with the return value.
    /// Used to restore a0 when restarting a syscall after signal delivery.
    pub orig_a0: usize,
    /// Whether the interrupted syscall is eligible for Linux-style SA_RESTART.
    /// Default to false and only opt-in well-understood blocking syscalls.
    pub restartable_syscall: bool,
}

impl TrapContext {
    /// Return the raw general-purpose register value at `index`.
    pub fn reg(&self, index: usize) -> usize {
        ArchTrapContextAbi::reg(&self.arch, index)
    }

    /// Update the raw general-purpose register value at `index`.
    pub fn set_reg(&mut self, index: usize, value: usize) {
        ArchTrapContextAbi::set_reg(&mut self.arch, index, value);
    }

    /// Return the saved user-mode PC.
    pub fn user_pc(&self) -> usize {
        ArchTrapContextAbi::user_pc(&self.arch)
    }

    /// Overwrite the saved user-mode PC.
    pub fn set_user_pc(&mut self, pc: usize) {
        ArchTrapContextAbi::set_user_pc(&mut self.arch, pc);
    }

    /// Advance the saved user-mode PC by `delta` bytes.
    pub fn advance_user_pc(&mut self, delta: usize) {
        let next = self.user_pc().wrapping_add(delta);
        self.set_user_pc(next);
    }

    /// Return the saved user stack pointer.
    pub fn user_sp(&self) -> usize {
        ArchTrapContextAbi::user_sp(&self.arch)
    }

    /// put the sp(stack pointer) into x\[2\] field of TrapContext
    pub fn set_sp(&mut self, sp: usize) {
        ArchTrapContextAbi::set_user_sp(&mut self.arch, sp);
    }

    /// Set the saved user stack pointer.
    pub fn set_user_sp(&mut self, sp: usize) {
        self.set_sp(sp);
    }

    /// Return the saved return address register.
    pub fn ra(&self) -> usize {
        ArchTrapContextAbi::ra(&self.arch)
    }

    /// Set the saved return address register.
    pub fn set_ra(&mut self, ra: usize) {
        ArchTrapContextAbi::set_ra(&mut self.arch, ra);
    }

    /// Return the saved user TLS/thread-pointer register.
    pub fn tls(&self) -> usize {
        ArchTrapContextAbi::tls(&self.arch)
    }

    /// Set the saved user TLS/thread-pointer register.
    pub fn set_tls(&mut self, tls: usize) {
        ArchTrapContextAbi::set_tls(&mut self.arch, tls);
    }

    /// Return the architecture syscall number register.
    pub fn syscall_nr(&self) -> usize {
        ArchTrapContextAbi::syscall_nr(&self.arch)
    }

    /// Return the six syscall arguments from the saved user context.
    pub fn syscall_args(&self) -> [usize; 6] {
        ArchTrapContextAbi::syscall_args(&self.arch)
    }

    /// Return the saved syscall return register.
    pub fn syscall_ret(&self) -> usize {
        ArchTrapContextAbi::syscall_ret(&self.arch)
    }

    /// Set the saved syscall return register.
    pub fn set_syscall_ret(&mut self, ret: usize) {
        ArchTrapContextAbi::set_syscall_ret(&mut self.arch, ret);
    }

    /// Set one user-call argument register.
    pub fn set_user_arg(&mut self, index: usize, value: usize) {
        ArchTrapContextAbi::set_user_arg(&mut self.arch, index, value);
    }

    /// Save the original first syscall argument for possible restart.
    pub fn save_syscall_arg0_for_restart(&mut self) {
        self.orig_a0 = self.syscall_ret();
    }

    /// Set the kernel hart id restored by the trap trampoline.
    pub fn set_kernel_hartid(&mut self, hartid: usize) {
        ArchTrapContextAbi::set_kernel_hartid(&mut self.arch, hartid);
    }

    /// Set the kernel stack pointer restored on the next trap entry.
    pub fn set_kernel_sp(&mut self, kernel_sp: usize) {
        ArchTrapContextAbi::set_kernel_sp(&mut self.arch, kernel_sp);
    }

    /// Export the saved register file using the riscv64 Linux signal ABI layout.
    pub fn export_signal_gprs(&self) -> [usize; 32] {
        ArchTrapContextAbi::export_signal_gprs(&self.arch)
    }

    /// Restore the saved register file using the riscv64 Linux signal ABI layout.
    pub fn import_signal_gprs(&mut self, gregs: &[usize; 32]) {
        ArchTrapContextAbi::import_signal_gprs(&mut self.arch, gregs);
    }

    /// Copy floating-point state into an external signal frame.
    pub fn copy_fp_state_to(&self, fpregs: &mut [u64; 32], fcsr: &mut u32) {
        ArchTrapContextAbi::copy_fp_state_to(&self.arch, fpregs, fcsr);
    }

    /// Restore floating-point state from an external signal frame.
    pub fn restore_fp_state(&mut self, fpregs: &[u64; 32], fcsr: u32) {
        ArchTrapContextAbi::restore_fp_state(&mut self.arch, fpregs, fcsr);
    }

    /// Export an architecture-labeled summary used by common fault logs.
    pub fn fault_dump_summary(&self) -> [NamedReg; 7] {
        ArchTrapContextAbi::fault_dump_summary(&self.arch)
    }

    /// Export a wider architecture-labeled register snapshot used by fault logs.
    pub fn fault_dump_detail(&self) -> [NamedReg; 19] {
        ArchTrapContextAbi::fault_dump_detail(&self.arch)
    }

    /// init the trap context of an application
    pub fn app_init_context(
        entry: usize,
        sp: usize,
        kernel_satp: usize,
        kernel_sp: usize,
        trap_handler: usize,
    ) -> Self {
        unsafe { crate::hal::enable_fp() };
        let cx = Self {
            arch: ArchTrapContextAbi::new_user_frame(
                entry,
                sp,
                kernel_satp,
                kernel_sp,
                trap_handler,
            ),
            in_syscall: false,
            orig_a0: 0,
            restartable_syscall: false,
        };
        cx // return initial Trap Context of app
    }
}
