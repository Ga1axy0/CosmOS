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
use crate::mm::{handle_ipi, PageFaultAccess, translated_refmut};
use crate::syscall::syscall;
use crate::syscall::errno::ERRNO;
use crate::task::{
    ExitReason, SignalFlags, check_fatal_signals_of_current, check_itimers_of_all_processes,
    check_signals_of_current, current_add_signal, current_process, current_process_is_zombie,
    current_trap_cx, current_trap_cx_user_va, current_user_token, exit_current_and_run_next,
    on_timer_tick, schedule_if_needed,
};
use crate::timer::{check_timer, get_realtime_ns, get_time, set_next_trigger};
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
        sie::set_ssoft();
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

/// 为当前 hart 开启 supervisor software interrupt。
pub fn enable_software_interrupt() {
    unsafe {
        sie::set_ssoft();
    }
}

/// 清除当前 hart 挂起的 supervisor software interrupt。
pub fn clear_software_interrupt_pending() {
    unsafe {
        asm!("csrc sip, {}", in(reg) 1 << 1);
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
        Trap::Exception(Exception::StorePageFault) => {
            debug!(
                "[mmap] trap store page fault: bad_addr={:#x} sepc={:#x}",
                stval,
                current_trap_cx().sepc
            );
            if !current_process().handle_private_cow_fault(stval) {
                match current_process().handle_file_page_fault(stval, PageFaultAccess::Write) {
                    Ok(()) => {}
                    Err(ERRNO::ENXIO) => {
                        error!(
                            "[kernel] trap_handler: {:?} beyond file EOF, bad addr = {:#x}, bad instruction = {:#x}, kernel killed it with SIGBUS.",
                            scause.cause(),
                            stval,
                            current_trap_cx().sepc,
                        );
                        current_add_signal(SignalFlags::SIGBUS);
                    }
                    Err(_) => {
                        error!(
                            "[kernel] trap_handler: {:?} in application, bad addr = {:#x}, bad instruction = {:#x}, kernel killed it.",
                            scause.cause(),
                            stval,
                            current_trap_cx().sepc,
                        );
                        current_add_signal(SignalFlags::SIGSEGV);
                    }
                }
            }
        }
        Trap::Exception(Exception::LoadPageFault) => {
            debug!(
                "[mmap] trap load page fault: bad_addr={:#x} sepc={:#x}",
                stval,
                current_trap_cx().sepc
            );
            match current_process().handle_file_page_fault(stval, PageFaultAccess::Read) {
                Ok(()) => {}
                Err(ERRNO::ENXIO) => {
                    error!(
                        "[kernel] trap_handler: {:?} beyond file EOF, bad addr = {:#x}, bad instruction = {:#x}, kernel killed it with SIGBUS.",
                        scause.cause(),
                        stval,
                        current_trap_cx().sepc,
                    );
                    current_add_signal(SignalFlags::SIGBUS);
                }
                Err(_) => {
                    error!(
                        "[kernel] trap_handler: {:?} in application, bad addr = {:#x}, bad instruction = {:#x}, kernel killed it.",
                        scause.cause(),
                        stval,
                        current_trap_cx().sepc,
                    );
                    current_add_signal(SignalFlags::SIGSEGV);
                }
            }
        }
        Trap::Exception(Exception::InstructionPageFault) => {
            debug!(
                "[mmap] trap instruction page fault: bad_addr={:#x} sepc={:#x}",
                stval,
                current_trap_cx().sepc
            );
            match current_process().handle_file_page_fault(stval, PageFaultAccess::Exec) {
                Ok(()) => {}
                Err(ERRNO::ENXIO) => {
                    error!(
                        "[kernel] trap_handler: {:?} beyond file EOF, bad addr = {:#x}, bad instruction = {:#x}, kernel killed it with SIGBUS.",
                        scause.cause(),
                        stval,
                        current_trap_cx().sepc,
                    );
                    current_add_signal(SignalFlags::SIGBUS);
                }
                Err(_) => {
                    error!(
                        "[kernel] trap_handler: {:?} in application, bad addr = {:#x}, bad instruction = {:#x}, kernel killed it.",
                        scause.cause(),
                        stval,
                        current_trap_cx().sepc,
                    );
                    current_add_signal(SignalFlags::SIGSEGV);
                }
            }
        }
        Trap::Exception(Exception::StoreFault)
        | Trap::Exception(Exception::InstructionFault)
        | Trap::Exception(Exception::LoadFault) => {
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
                current_trap_cx().sepc,
                stval,
            );
            current_add_signal(SignalFlags::SIGILL);
        }
        Trap::Interrupt(Interrupt::SupervisorTimer) => {
            // trace!("hart {} timer tick", hartid());
            set_next_trigger();
            check_timer();
            let now_raw = get_time();
            check_itimers_of_all_processes(now_raw, get_realtime_ns());
            crate::net::poll();
            on_timer_tick();
        }
        Trap::Interrupt(Interrupt::SupervisorSoft) => {
            handle_ipi();
            clear_software_interrupt_pending();
        }
        Trap::Interrupt(Interrupt::SupervisorExternal) => {
            crate::drivers::plic::handle_supervisor_external();
            crate::net::poll();
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
    schedule_if_needed();
    // Handle non-fatal signals before returning to user space
    handle_signals();
    trap_return();
}

/// Handle pending signals by setting up user-space signal handler invocation.
/// This modifies the trap context to call the signal handler when returning to user space.
fn handle_signals() {
    let (signum, handler, mask) = match check_signals_of_current() {
        Some(signal_info) => signal_info,
        None => return,
    };

    let trap_cx = current_trap_cx();

    // Save the current context on user stack.
    let mut user_sp = trap_cx.x[2]; // sp register

    // We need to save the trap context on the user stack so sigreturn can restore it.
    // Allocate space for: saved trap context + signum + old mask.
    user_sp -= core::mem::size_of::<TrapContext>();
    let saved_trap_cx_ptr = user_sp;

    user_sp -= core::mem::size_of::<usize>(); // signum
    let signum_ptr = user_sp;

    user_sp -= core::mem::size_of::<SignalFlags>(); // old mask
    let old_mask_ptr = user_sp;

    // Align stack to 16 bytes.
    user_sp &= !0xf;

    // Write saved context to user stack.
    let token = current_user_token();
    let process = current_process();

    // Save the trap context.
    let saved_cx_opt = translated_refmut(token, saved_trap_cx_ptr as *mut TrapContext);
    let Some(saved_cx) = saved_cx_opt else {
        // Failed to save context, skip this signal.
        warn!("[kernel] handle_signals: failed to save trap context for signal {}", signum);
        return;
    };
    *saved_cx = *trap_cx;

    // Save signum.
    let signum_opt = translated_refmut(token, signum_ptr as *mut i32);
    let Some(signum_ref) = signum_opt else {
        warn!("[kernel] handle_signals: failed to save signum");
        return;
    };
    *signum_ref = signum;

    // Save old mask and apply new mask.
    let old_mask_opt = translated_refmut(token, old_mask_ptr as *mut SignalFlags);
    let Some(old_mask_ref) = old_mask_opt else {
        warn!("[kernel] handle_signals: failed to save old mask");
        return;
    };
    let mut inner = process.inner_exclusive_access();
    *old_mask_ref = inner.signal_mask;
    // Apply the signal mask during handler execution.
    inner.signal_mask |= mask;

    // Set up the trap context to call the signal handler.
    // When the handler returns, it should call sigreturn.
    trap_cx.x[2] = user_sp; // Update sp
    trap_cx.x[10] = signum as usize; // a0 = signum (first argument)
    trap_cx.sepc = handler; // Jump to signal handler
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
            crate::net::poll(); // 处理完外部中断后立即poll，让smoltcp响应ARP等请求
        }
        Ok(Trap::Interrupt(Interrupt::SupervisorTimer)) => {
            // trace!("hart {} timer tick", hartid());
            set_next_trigger();
            check_timer();
            let now_raw = get_time();
            check_itimers_of_all_processes(now_raw, get_realtime_ns());
            crate::net::poll();
        }
        Ok(Trap::Interrupt(Interrupt::SupervisorSoft)) => {
            handle_ipi();
            clear_software_interrupt_pending();
        }
        _ => {
            panic!("Kernel trap: {:?}, stval = {:#x}", scause.cause(), stval);
        }
    }
    // check_timer();
}

pub use context::TrapContext;
