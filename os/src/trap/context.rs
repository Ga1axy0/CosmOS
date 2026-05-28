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
}

impl TrapContext {
    /// put the sp(stack pointer) into x\[2\] field of TrapContext
    pub fn set_sp(&mut self, sp: usize) {
        self.x[2] = sp;
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
        };
        cx.set_sp(sp); // app's user stack pointer
        cx // return initial Trap Context of app
    }
}
