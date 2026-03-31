//! Trap handling functionality
//!
//! For rCore, we have a single trap entry point, namely `__alltraps`. At
//! initialization in [`init()`], we set the `stvec` CSR to point to it.
//!
//! All traps go through `__alltraps`, which is defined in `trap.S`. The
//! assembly language code does just enough work restore the kernel space
//! context, ensuring that Rust code safely runs, and transfers control to
//! [`trap_handler()`].
//!
//! It then calls different functionality based on what exactly the exception
//! was. For example, timer interrupts trigger task preemption, and syscalls go
//! to [`syscall()`].

mod context;

use crate::config::TRAMPOLINE;
use crate::hart::hartid;
use crate::syscall::syscall;
use crate::task::{
    ExitReason, SignalFlags, check_fatal_signals_of_current, current_add_signal, current_process, current_process_is_zombie, current_trap_cx, current_trap_cx_user_va, current_user_token, exit_current_and_run_next, suspend_current_and_run_next
};
use crate::timer::{check_timer, get_time, set_next_trigger};
use core::arch::{asm, global_asm};
use riscv::register::{
    mtvec::TrapMode,
    scause::{self, Exception, Interrupt, Trap},
    sie, stval, stvec,
};

global_asm!(include_str!("trap.S"));

/// 初始化当前 hart 的 trap 相关状态。
///
/// 该函数需要每个 hart 各自执行一次，用于安装本 hart 的内核 trap 入口，
/// 并开启 supervisor external interrupt。
pub fn init() {
    init_hart()
}

/// 初始化当前 hart 的 trap 相关状态。
pub fn init_hart() {
    set_kernel_trap_entry();
    unsafe {
        sie::set_sext();
    }
    info!("hart {} trap init done", hartid());
}
/// set trap entry for traps happen in kernel(supervisor) mode
pub fn set_kernel_trap_entry() {
    extern "C" {
        fn __trap_from_kernel();
    }
    unsafe {
        stvec::write(__trap_from_kernel as usize, TrapMode::Direct);
    }
}
/// set trap entry for traps happen in user mode
pub fn set_user_trap_entry() {
    unsafe {
        stvec::write(TRAMPOLINE as usize, TrapMode::Direct);
    }
}

/// 为当前 hart 开启 supervisor timer interrupt。
pub fn enable_timer_interrupt() {
    unsafe {
        sie::set_stimer();
    }
}

/// 为当前 hart 关闭 supervisor timer interrupt。
///
/// 这用于 secondary hart 进入“纯 idle 占位”状态的场景，避免它在尚未完成
/// 全局共享状态并发化之前，进入会访问共享 `UPSafeCell` 的 timer 路径。
pub fn disable_timer_interrupt() {
    unsafe {
        sie::clear_stimer();
    }
}

/// 为当前 hart 关闭 supervisor external interrupt。
///
/// 这用于 secondary hart 暂时只作为已上线但不参与设备中断处理的 idle hart。
pub fn disable_external_interrupt() {
    unsafe {
        sie::clear_sext();
    }
}

/// trap handler
#[no_mangle]
pub fn trap_handler() -> ! {
    set_kernel_trap_entry();
    current_process().enter_kernel(get_time());
    let scause = scause::read();
    let stval = stval::read();
    // trace!("into {:?}", scause.cause());
    match scause.cause() {
        Trap::Exception(Exception::UserEnvCall) => {
            // jump to next instruction anyway
            let mut cx = current_trap_cx();
            cx.sepc += 4;
            // get system call return value
            let result = syscall(
                cx.x[17],
                [cx.x[10], cx.x[11], cx.x[12], cx.x[13], cx.x[14], cx.x[15]],
            );
            // cx is changed during sys_execve, so we have to call it again
            cx = current_trap_cx();
            cx.x[10] = result as usize;
        }
        Trap::Exception(Exception::StoreFault)
        | Trap::Exception(Exception::StorePageFault)
        | Trap::Exception(Exception::InstructionFault)
        | Trap::Exception(Exception::InstructionPageFault)
        | Trap::Exception(Exception::LoadFault)
        | Trap::Exception(Exception::LoadPageFault) => {
            error!(
                "[kernel] trap_handler: {:?} in application, bad addr = {:#x}, bad instruction = {:#x}, kernel killed it.",
                scause.cause(),
                stval,
                current_trap_cx().sepc,
            );
            current_add_signal(SignalFlags::SIGSEGV);
        }
        Trap::Exception(Exception::IllegalInstruction) => {
            error!(
                "[kernel] trap_handler: Illegal instruction in application, bad addr = {:#x}, bad instruction = {:#x}, kernel killed it.",
                stval,
                current_trap_cx().sepc,
            );
            current_add_signal(SignalFlags::SIGILL);
        }
        Trap::Interrupt(Interrupt::SupervisorTimer) => {
            // trace!("hart {} timer tick", hartid());
            set_next_trigger();
            check_timer();
            suspend_current_and_run_next();
        }
        Trap::Interrupt(Interrupt::SupervisorExternal) => {
            crate::drivers::plic::handle_supervisor_external();
            // crate::net::poll();
        }
        _ => {
            panic!(
                "Unsupported trap {:?}, stval = {:#x}!",
                scause.cause(),
                stval
            );
        }
    }
    // check signals
    if let Some((signum, msg)) = check_fatal_signals_of_current() {
        trace!("[kernel] trap_handler: .. check signals {}", msg);
        exit_current_and_run_next(ExitReason::Signal(signum as u32));
    }
    if current_process_is_zombie() {
        trace!("[kernel] trap_handler: .. current process is zombie");
        // 非主进程才会进入这个分支，此时退出的reason是不重要的。
        exit_current_and_run_next(ExitReason::Exit(0));
    }
    trap_return();
}

/// return to user space
#[no_mangle]
pub fn trap_return() -> ! {
    //disable_supervisor_interrupt();
    set_user_trap_entry();
    let trap_cx_user_va = current_trap_cx_user_va();
    current_trap_cx().kernel_hartid = hartid();
    let user_satp = current_user_token();
    extern "C" {
        fn __alltraps();
        fn __restore();
    }
    let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;
    // trace!("[kernel] trap_return: ..before return");
    current_process().enter_user(get_time());
    unsafe {
        asm!(
            "fence.i",
            "jr {restore_va}",         // jump to new addr of __restore asm function
            restore_va = in(reg) restore_va,
            in("a0") trap_cx_user_va,      // a0 = virt addr of Trap Context
            in("a1") user_satp,        // a1 = user satp token
            options(noreturn)
        );
    }
}

/// handle trap from kernel
#[no_mangle]
pub fn trap_from_kernel() {
    // debug!("Trap from kernel: scause = {:?}, stval = {:#x}", scause::read(), stval::read());
    let scause = scause::read();
    let stval = stval::read();
    let cause: Trap = scause
        .cause()
        .try_into()
        .unwrap_or_else(|_| panic!("Invalid trap {:?}, stval = {:#x}!", scause.cause(), stval));
    match cause.try_into() {
        Ok(Trap::Interrupt(Interrupt::SupervisorExternal)) => {
            // debug!("External interrupt from kernel: scause = {:?}, stval = {:#x}", scause, stval);
            crate::drivers::plic::handle_supervisor_external();
            // crate::net::poll(); // 处理完外部中断后立即poll，让smoltcp响应ARP等请求
        }
        Ok(Trap::Interrupt(Interrupt::SupervisorTimer)) => {
            // trace!("hart {} timer tick", hartid());
            set_next_trigger();
            check_timer();
            // crate::net::poll();
        }
        _ => {
            panic!("Kernel trap: {:?}, stval = {:#x}", scause.cause(), stval);
        }
    }
    // check_timer();
}

pub use context::TrapContext;
