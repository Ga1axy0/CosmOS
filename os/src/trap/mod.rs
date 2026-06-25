//! Trap handling functionality
//!
//! For rCore, we have a single trap entry point, namely `__alltraps`. At
//! initialization in [`init()`], we set the `stvec` CSR to point to it.
//!
//! All traps go through an architecture-defined trampoline. The assembly code
//! does just enough work restore the kernel space context, ensuring that Rust
//! code safely runs, and transfers control to [`trap_handler()`].
//!
//! It then calls different functionality based on what exactly the exception
//! was. For example, timer interrupts trigger task preemption, and syscalls go
//! to [`syscall()`].

mod context;
mod irq;

use crate::config::PAGE_SIZE;
use crate::hal::hartid;
use crate::mm::{handle_ipi, MmError, PageFaultAccess, PageFaultHandled};
use crate::signal::{SignalBit, SignalNum, handle_signals};
use crate::syscall::{syscall, syscall_supports_sa_restart};
use crate::sched::{on_timer_tick, request_current_task_resched, schedule_if_needed, ReschedReason};
use crate::task::{
    ExitReason, check_fatal_signals_of_current, check_itimers_of_all_processes,
    current_add_signal, current_process, current_process_is_zombie, current_task, current_trap_cx,
    current_trap_cx_user_va, current_user_token, exit_current_and_run_next,
    exit_group_current_and_run_next,
};
use crate::timer::{get_realtime_ns, get_time, get_time_ns, handle_timer_interrupt};
use crate::hal::{ArchInterrupt, ArchTrapMachine};
use crate::hal::traits::{InterruptControl, TrapCause, TrapMachine};

/// 输出用户态致命异常现场，区分 fault 地址、用户 PC 与关键寄存器。
fn log_user_fault(reason: &str, access: &str, fault_addr: usize, signal: &str) {
    let cx = current_trap_cx();
    let summary = cx.fault_dump_summary();
    let detail = cx.fault_dump_detail();
    error!(
        "[kernel] user fault: reason={}, access={}, pid={}, fault_addr={:#x}, user_pc={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, signal={}",
        reason,
        access,
        current_process().getpid(),
        fault_addr,
        cx.user_pc(),
        summary[0].name,
        summary[0].value,
        summary[1].name,
        summary[1].value,
        summary[2].name,
        summary[2].value,
        summary[3].name,
        summary[3].value,
        summary[4].name,
        summary[4].value,
        summary[5].name,
        summary[5].value,
        summary[6].name,
        summary[6].value,
        signal,
    );
    error!(
        "[kernel] user fault regs: {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}",
        detail[0].name,
        detail[0].value,
        detail[1].name,
        detail[1].value,
        detail[2].name,
        detail[2].value,
        detail[3].name,
        detail[3].value,
        detail[4].name,
        detail[4].value,
        detail[5].name,
        detail[5].value,
        detail[6].name,
        detail[6].value,
        detail[7].name,
        detail[7].value,
        detail[8].name,
        detail[8].value,
        detail[9].name,
        detail[9].value,
        detail[10].name,
        detail[10].value,
        detail[11].name,
        detail[11].value,
        detail[12].name,
        detail[12].value,
        detail[13].name,
        detail[13].value,
        detail[14].name,
        detail[14].value,
        detail[15].name,
        detail[15].value,
        detail[16].name,
        detail[16].value,
        detail[17].name,
        detail[17].value,
        detail[18].name,
        detail[18].value,
    );
}

fn handle_user_oom(path: &str, access: &str, fault_addr: usize) -> ! {
    let cx = current_trap_cx();
    let summary = cx.fault_dump_summary();
    error!(
        "[kernel] fatal lazy-fault OOM: path={}, access={}, pid={}, fault_addr={:#x}, user_pc={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}, {}={:#x}",
        path,
        access,
        current_process().getpid(),
        fault_addr,
        cx.user_pc(),
        summary[0].name,
        summary[0].value,
        summary[1].name,
        summary[1].value,
        summary[3].name,
        summary[3].value,
        summary[4].name,
        summary[4].value,
        summary[5].name,
        summary[5].value,
        summary[6].name,
        summary[6].value,
    );
    crate::mm::log_oom(path, Some(access), Some(fault_addr));
    exit_group_current_and_run_next(ExitReason::Signal(SignalNum::SIGKILL.number() as u32));
    panic!("unreachable: OOM exit_group_current_and_run_next returned");
}

/// 初始化当前 hart 的 trap 相关状态。
///
/// 该函数需要每个 hart 各自执行一次，用于安装本 hart 的内核 trap 入口，
/// 并开启 supervisor external interrupt。
pub fn init() {
    init_hart()
}

/// 初始化当前 hart 的 trap 相关状态。
pub fn init_hart() {
    unsafe {
        ArchInterrupt::set_kernel_trap_entry();
        ArchInterrupt::enable_external();
        ArchInterrupt::enable_software();
    }
    info!("hart {} trap init done", hartid());
}
/// set trap entry for traps happen in kernel(supervisor) mode
pub fn set_kernel_trap_entry() {
    unsafe { ArchInterrupt::set_kernel_trap_entry(); }
}
/// set trap entry for traps happen in user mode
pub fn set_user_trap_entry() {
    unsafe { ArchInterrupt::set_user_trap_entry(); }
}

/// 为当前 hart 开启 supervisor timer interrupt。
pub fn enable_timer_interrupt() {
    unsafe { ArchInterrupt::enable_timer(); }
}

/// 为当前 hart 关闭 supervisor timer interrupt。
pub fn disable_timer_interrupt() {
    unsafe { ArchInterrupt::disable_timer(); }
}

/// 为当前 hart 关闭 supervisor external interrupt。
pub fn disable_external_interrupt() {
    unsafe { ArchInterrupt::disable_external(); }
}

/// 为当前 hart 开启 supervisor software interrupt。
pub fn enable_software_interrupt() {
    unsafe { ArchInterrupt::enable_software(); }
}

/// 清除当前 hart 挂起的 supervisor software interrupt。
pub fn clear_software_interrupt_pending() {
    unsafe { ArchInterrupt::clear_software_pending(); }
}

/// Handle a scheduler reschedule IPI.
///
/// On a running hart, the IPI requests deferred rescheduling of the current
/// task. On an idle hart, clearing the pending bit is enough to wake `wfi`
/// so the idle loop can observe newly queued work on the next iteration.
fn handle_reschedule_ipi() {
    handle_ipi();
    crate::platform::clear_ipi();
    clear_software_interrupt_pending();
    request_current_task_resched(ReschedReason::HigherRtPriority);
}

/// trap handler
#[no_mangle]
pub fn trap_handler() -> ! {
    set_kernel_trap_entry();
    current_process().enter_kernel(get_time());
    current_trap_cx().in_syscall = false;
    current_trap_cx().restartable_syscall = false;
    let trap_info = ArchTrapMachine::read_trap_info();
    match trap_info.cause {
        TrapCause::UserSyscall => {
            let _kernel_irq = irq::KernelIrqEnableGuard::new();
            // jump to next instruction anyway
            let mut cx = current_trap_cx();
            let syscall_id = cx.syscall_nr();
            let syscall_args = cx.syscall_args();
            cx.save_syscall_arg0_for_restart();
            cx.restartable_syscall = syscall_supports_sa_restart(syscall_id);
            cx.advance_user_pc(ArchTrapMachine::syscall_instruction_len());
            // get system call return value
            let result = syscall(syscall_id, syscall_args);
            // cx is changed during sys_execve, so we have to call it again
            cx = current_trap_cx();
            cx.set_syscall_ret(result as usize);
            cx.in_syscall = true;
        }
        TrapCause::StorePageFault => {
            let _kernel_irq = irq::KernelIrqEnableGuard::new();
            debug!(
                "[mmap] trap store page fault: bad_addr={:#x} sepc={:#x}",
                trap_info.fault_addr,
                current_trap_cx().user_pc()
            );
            let process = current_process();
            let mut handled = false;
            match process.handle_private_cow_fault(trap_info.fault_addr) {
                Ok(PageFaultHandled::Handled) => handled = true,
                Ok(PageFaultHandled::NotHandled) => {}
                Err(MmError::OutOfMemory) => {
                    handle_user_oom("private_cow", "write", trap_info.fault_addr);
                }
                Err(_) => {}
            }
            if !handled {
                match process.handle_lazy_user_fault(trap_info.fault_addr, PageFaultAccess::Write) {
                    Ok(PageFaultHandled::Handled) => handled = true,
                    Ok(PageFaultHandled::NotHandled) => {}
                    Err(MmError::OutOfMemory) => {
                        handle_user_oom("lazy_user", "write", trap_info.fault_addr);
                    }
                    Err(_) => {}
                }
            }
            if !handled {
                match current_process().handle_file_page_fault(trap_info.fault_addr, PageFaultAccess::Write) {
                    Ok(PageFaultHandled::Handled) => {}
                    Ok(PageFaultHandled::NotHandled) => {
                        log_user_fault("store page fault", "write", trap_info.fault_addr, "SIGSEGV");
                        current_add_signal(SignalBit::SIGSEGV);
                    }
                    Err(MmError::BeyondFileEnd) => {
                        log_user_fault("store page fault beyond file EOF", "write", trap_info.fault_addr, "SIGBUS");
                        current_add_signal(SignalBit::SIGBUS);
                    }
                    Err(MmError::OutOfMemory) => {
                        handle_user_oom("file_mmap", "write", trap_info.fault_addr);
                    }
                    Err(_) => {
                        log_user_fault("store page fault", "write", trap_info.fault_addr, "SIGSEGV");
                        current_add_signal(SignalBit::SIGSEGV);
                    }
                }
            } else if process.exec_path().ends_with("entry-static.exe") {
                let start_brk = {
                    let inner = process.inner_exclusive_access();
                    inner.vm_layout.start_brk
                };
                let tls_page = start_brk & !(PAGE_SIZE - 1);
                if (trap_info.fault_addr & !(PAGE_SIZE - 1)) == tls_page {
                    debug!(
                        "[entry-static errno] store fault mapped tls page: fault_addr={:#x} tls_page={:#x}",
                        trap_info.fault_addr,
                        tls_page
                    );
                }
            }
        }
        TrapCause::LoadPageFault => {
            let _kernel_irq = irq::KernelIrqEnableGuard::new();
            // debug!(
            //     "[mmap] trap load page fault: bad_addr={:#x} sepc={:#x}",
            //     trap_info.fault_addr,
            //     current_trap_cx().user_pc()
            // );
            let mut handled = false;
            match current_process().handle_lazy_user_fault(trap_info.fault_addr, PageFaultAccess::Read) {
                Ok(PageFaultHandled::Handled) => handled = true,
                Ok(PageFaultHandled::NotHandled) => {}
                Err(MmError::OutOfMemory) => {
                    handle_user_oom("lazy_user", "read", trap_info.fault_addr);
                }
                Err(_) => {}
            }
            if !handled {
                match current_process().handle_file_page_fault(trap_info.fault_addr, PageFaultAccess::Read) {
                    Ok(PageFaultHandled::Handled) => {}
                    Ok(PageFaultHandled::NotHandled) => {
                        log_user_fault("load page fault", "read", trap_info.fault_addr, "SIGSEGV");
                        current_add_signal(SignalBit::SIGSEGV);
                    }
                    Err(MmError::BeyondFileEnd) => {
                        log_user_fault("load page fault beyond file EOF", "read", trap_info.fault_addr, "SIGBUS");
                        current_add_signal(SignalBit::SIGBUS);
                    }
                    Err(MmError::OutOfMemory) => {
                        handle_user_oom("file_mmap", "read", trap_info.fault_addr);
                    }
                    Err(_) => {
                        log_user_fault("load page fault", "read", trap_info.fault_addr, "SIGSEGV");
                        current_add_signal(SignalBit::SIGSEGV);
                    }
                }
            }
        }
        TrapCause::InstructionPageFault => {
            let _kernel_irq = irq::KernelIrqEnableGuard::new();
            debug!(
                "[mmap] trap instruction page fault: bad_addr={:#x} sepc={:#x}",
                trap_info.fault_addr,
                current_trap_cx().user_pc()
            );
            let mut handled = false;
            match current_process().handle_lazy_user_fault(trap_info.fault_addr, PageFaultAccess::Exec) {
                Ok(PageFaultHandled::Handled) => handled = true,
                Ok(PageFaultHandled::NotHandled) => {}
                Err(MmError::OutOfMemory) => {
                    handle_user_oom("lazy_user", "exec", trap_info.fault_addr);
                }
                Err(_) => {}
            }
            if !handled {
                match current_process().handle_file_page_fault(trap_info.fault_addr, PageFaultAccess::Exec) {
                    Ok(PageFaultHandled::Handled) => {}
                    Ok(PageFaultHandled::NotHandled) => {
                        log_user_fault("instruction page fault", "exec", trap_info.fault_addr, "SIGSEGV");
                        current_add_signal(SignalBit::SIGSEGV);
                    }
                    Err(MmError::BeyondFileEnd) => {
                        log_user_fault("instruction page fault beyond file EOF", "exec", trap_info.fault_addr, "SIGBUS");
                        current_add_signal(SignalBit::SIGBUS);
                    }
                    Err(MmError::OutOfMemory) => {
                        handle_user_oom("file_mmap", "exec", trap_info.fault_addr);
                    }
                    Err(_) => {
                        log_user_fault("instruction page fault", "exec", trap_info.fault_addr, "SIGSEGV");
                        current_add_signal(SignalBit::SIGSEGV);
                    }
                }
            }
        }
        TrapCause::StoreFault
        | TrapCause::InstructionFault
        | TrapCause::LoadFault
        | TrapCause::DataAddressFault => {
            log_user_fault("access fault", "unknown", trap_info.fault_addr, "SIGSEGV");
            current_add_signal(SignalBit::SIGSEGV);
        }
        TrapCause::IllegalInstruction => {
            log_user_fault("illegal instruction", "exec", trap_info.fault_addr, "SIGILL");
            current_add_signal(SignalBit::SIGILL);
        }
        TrapCause::TimerInterrupt => {
            let _hardirq = irq::HardIrqGuard::enter();
            // trace!("hart {} timer tick", hartid());
            if handle_timer_interrupt() {
                let now_raw = get_time();
                check_itimers_of_all_processes(now_raw, get_realtime_ns());
                crate::net::poll_timer_tick();
                on_timer_tick();
            }
        }
        TrapCause::SoftwareInterrupt => {
            let _hardirq = irq::HardIrqGuard::enter();
            handle_reschedule_ipi();
        }
        TrapCause::ExternalInterrupt => {
            let _hardirq = irq::HardIrqGuard::enter();
            crate::platform::handle_external_irq();
            crate::net::poll();
        }
        _ => {
            panic!(
                "Unsupported trap {:?}, fault_addr = {:#x}!",
                trap_info.cause,
                trap_info.fault_addr
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
    // Handle user-installed signal handlers before returning to user space.
    // If the kernel cannot build a signal frame (for example, because the user
    // stack is already invalid), terminate the task instead of re-executing the
    // same faulting instruction forever.
    if let Some(signum) = handle_signals() {
        exit_current_and_run_next(ExitReason::Signal(signum as u32));
    }
    trap_return();
}

/// return to user space
#[no_mangle]
pub fn trap_return() -> ! {
    set_user_trap_entry();
    let now_ns = get_time_ns();
    current_task().unwrap().note_first_user_return(now_ns);
    let trap_cx_user_va = current_trap_cx_user_va();
    current_trap_cx().set_kernel_hartid(hartid());
    let user_token = current_user_token();
    current_process().enter_user(get_time());
    unsafe { ArchTrapMachine::return_to_user(trap_cx_user_va, user_token) }
}

/// handle trap from kernel
#[no_mangle]
pub fn trap_from_kernel() {
    let _hardirq = irq::HardIrqGuard::enter();
    let trap_info = ArchTrapMachine::read_trap_info();
    match trap_info.cause {
        TrapCause::ExternalInterrupt => {
            crate::platform::handle_external_irq();
            crate::net::poll(); // 处理完外部中断后立即poll，让smoltcp响应ARP等请求
        }
        TrapCause::TimerInterrupt => {
            // trace!("hart {} timer tick", hartid());
            if handle_timer_interrupt() {
                let now_raw = get_time();
                check_itimers_of_all_processes(now_raw, get_realtime_ns());
                crate::net::poll_timer_tick();
                // Account CPU time spent while the current task executes in kernel
                // context as part of its RR quantum as well. This matches Linux's
                // "running on CPU" notion more closely than charging only
                // user-mode ticks.
                on_timer_tick();
            }
        }
        TrapCause::SoftwareInterrupt => {
            handle_reschedule_ipi();
        }
        _ => {
            panic!(
                "Kernel trap: {:?}, fault_addr = {:#x}",
                trap_info.cause,
                trap_info.fault_addr
            );
        }
    }
    // check_timer();
}

pub use context::TrapContext;
pub use irq::{
    enter_noirq_lock, exit_noirq_lock, HardIrqGuard, KernelIrqEnableGuard,
};
