//! Implementation of [`TrapContext`]
use riscv::register::sstatus::{self, Sstatus, SPP};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
/// trap context structure containing sstatus, sepc and registers
pub struct TrapContext {
    /// General-Purpose Register x0-31
    pub x: [usize; 32],
    /// Supervisor Status Register
    pub sstatus: Sstatus,
    /// Supervisor Exception Program Counter
    pub sepc: usize,
    /// 当前任务上次返回用户态前所在的 hart id。
    ///
    /// 用户态可能会把 `tp` 当作 TLS 指针或普通寄存器使用，因此内核不能再假设
    /// trap 进入时 `tp` 里仍然保存着 hart-local 信息。这里单独记录内核需要恢复
    /// 的 hart id，供 trap 入口在切回内核上下文前重新写回 `tp`。
    pub kernel_hartid: usize,
    /// Token of kernel address space
    pub kernel_satp: usize,
    /// Kernel stack pointer of the current application
    pub kernel_sp: usize,
    /// Virtual address of trap handler entry point in kernel
    pub trap_handler: usize,
    /// Floating-point registers f0-f31
    pub f: [u64; 32],
    /// Floating-point control and status register
    pub fcsr: usize,
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
        self.x[index]
    }

    /// Update the raw general-purpose register value at `index`.
    pub fn set_reg(&mut self, index: usize, value: usize) {
        if index != 0 {
            self.x[index] = value;
        }
    }

    /// Return the saved user-mode PC.
    pub fn user_pc(&self) -> usize {
        self.sepc
    }

    /// Overwrite the saved user-mode PC.
    pub fn set_user_pc(&mut self, pc: usize) {
        self.sepc = pc;
    }

    /// Advance the saved user-mode PC by `delta` bytes.
    pub fn advance_user_pc(&mut self, delta: usize) {
        self.sepc = self.sepc.wrapping_add(delta);
    }

    /// Return the saved user stack pointer.
    pub fn user_sp(&self) -> usize {
        self.x[2]
    }

    /// put the sp(stack pointer) into x\[2\] field of TrapContext
    pub fn set_sp(&mut self, sp: usize) {
        self.x[2] = sp;
    }

    /// Set the saved user stack pointer.
    pub fn set_user_sp(&mut self, sp: usize) {
        self.set_sp(sp);
    }

    /// Return the saved return address register.
    pub fn ra(&self) -> usize {
        self.x[1]
    }

    /// Set the saved return address register.
    pub fn set_ra(&mut self, ra: usize) {
        self.x[1] = ra;
    }

    /// Return the saved user TLS/thread-pointer register.
    pub fn tls(&self) -> usize {
        self.x[4]
    }

    /// Set the saved user TLS/thread-pointer register.
    pub fn set_tls(&mut self, tls: usize) {
        self.x[4] = tls;
    }

    /// Return the architecture syscall number register.
    pub fn syscall_nr(&self) -> usize {
        self.x[17]
    }

    /// Return the six syscall arguments from the saved user context.
    pub fn syscall_args(&self) -> [usize; 6] {
        [self.x[10], self.x[11], self.x[12], self.x[13], self.x[14], self.x[15]]
    }

    /// Return the saved syscall return register.
    pub fn syscall_ret(&self) -> usize {
        self.x[10]
    }

    /// Set the saved syscall return register.
    pub fn set_syscall_ret(&mut self, ret: usize) {
        self.x[10] = ret;
    }

    /// Set one user-call argument register.
    pub fn set_user_arg(&mut self, index: usize, value: usize) {
        self.x[10 + index] = value;
    }

    /// Save the original first syscall argument for possible restart.
    pub fn save_syscall_arg0_for_restart(&mut self) {
        self.orig_a0 = self.x[10];
    }

    /// Set the kernel hart id restored by the trap trampoline.
    pub fn set_kernel_hartid(&mut self, hartid: usize) {
        self.kernel_hartid = hartid;
    }

    /// Set the kernel stack pointer restored on the next trap entry.
    pub fn set_kernel_sp(&mut self, kernel_sp: usize) {
        self.kernel_sp = kernel_sp;
    }

    /// Export the saved register file using the riscv64 Linux signal ABI layout.
    pub fn export_signal_gprs(&self) -> [usize; 32] {
        let mut gregs = [0usize; 32];
        gregs[0] = self.sepc;
        gregs[1..].copy_from_slice(&self.x[1..]);
        gregs
    }

    /// Restore the saved register file using the riscv64 Linux signal ABI layout.
    pub fn import_signal_gprs(&mut self, gregs: &[usize; 32]) {
        self.x[0] = 0;
        self.x[1..].copy_from_slice(&gregs[1..]);
        self.sepc = gregs[0];
    }

    /// Copy floating-point state into an external signal frame.
    pub fn copy_fp_state_to(&self, fpregs: &mut [u64; 32], fcsr: &mut u32) {
        fpregs.copy_from_slice(&self.f);
        *fcsr = self.fcsr as u32;
    }

    /// Restore floating-point state from an external signal frame.
    pub fn restore_fp_state(&mut self, fpregs: &[u64; 32], fcsr: u32) {
        self.f.copy_from_slice(fpregs);
        self.fcsr = fcsr as usize;
    }

    /// init the trap context of an application
    pub fn app_init_context(
        entry: usize,
        sp: usize,
        kernel_satp: usize,
        kernel_sp: usize,
        trap_handler: usize,
    ) -> Self {
        let mut sstatus = sstatus::read();
        // set CPU privilege to User after trapping back
        sstatus.set_spp(SPP::User);
        unsafe { riscv::register::sstatus::set_fs(riscv::register::mstatus::FS::Initial) };
        let mut cx = Self {
            x: [0; 32],
            sstatus,
            sepc: entry,  // entry point of app
            kernel_hartid: 0,
            kernel_satp,  // addr of page table
            kernel_sp,    // kernel stack
            trap_handler, // addr of trap_handler function
            f: [0; 32],
            fcsr: 0,
            in_syscall: false,
            orig_a0: 0,
            restartable_syscall: false,
        };
        cx.set_user_sp(sp); // app's user stack pointer
        cx // return initial Trap Context of app
    }
}
