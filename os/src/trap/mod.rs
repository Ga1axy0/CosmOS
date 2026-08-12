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
use crate::hal::traits::{InterruptControl, TrapCause, TrapMachine};
use crate::hal::{ArchInterrupt, ArchTrapMachine};
use crate::mm::{handle_ipi, MmError, PageFaultAccess, PageFaultHandled};
use crate::sched::{
    on_timer_tick, request_current_task_resched, schedule_if_needed, ReschedReason,
};
use crate::signal::{handle_signals, SignalBit, SignalNum};
#[cfg(target_arch = "riscv64")]
use crate::syscall::translated_byte_buffer_with_access;
use crate::syscall::{syscall, syscall_supports_sa_restart};
use crate::task::{
    check_fatal_signals_of_current, check_itimers_of_all_processes, current_add_signal,
    current_process, current_process_is_zombie, current_task, current_trap_cx,
    current_trap_cx_user_va, current_user_token, exit_current_and_run_next,
    exit_group_current_and_run_next, ExitReason,
};
use crate::timer::{get_realtime_ns, get_time, handle_timer_interrupt};

/// Diagnostic-only lmbench-null/getppid path.
///
/// This deliberately bypasses accounting, TLB mailbox polling, interrupt
/// enablement, syscall dispatch, signal delivery and scheduler exit work.  It
/// is not a production syscall implementation: its only purpose is to measure
/// how much of `lat_syscall null` is outside the architectural trap entry/exit
/// plus one snapshot of the current task/process state.
#[cfg(all(feature = "getpid_path_probe", target_arch = "riscv64"))]
fn try_getpid_path_probe(trap_info: &crate::hal::traits::TrapInfo) {
    if !matches!(trap_info.cause, TrapCause::UserSyscall) {
        return;
    }

    let task = current_task().expect("getpid probe without a current task");
    let process = task
        .process
        .upgrade()
        .expect("getpid probe without a current process");
    let (trap_cx_user_va, trap_cx) = {
        let task_inner = task.inner_exclusive_access();
        let trap_cx_user_va = task_inner
            .res
            .as_ref()
            .expect("getpid probe without user resources")
            .trap_cx_user_va();
        (trap_cx_user_va, task_inner.get_trap_cx())
    };

    if trap_cx.syscall_nr() != crate::syscall::SYSCALL_GETPPID {
        return;
    }

    let parent_pid = {
        let parent = process.inner_exclusive_access().parent.clone();
        parent
            .and_then(|parent| parent.upgrade())
            .map_or(0, |parent| parent.getpid())
    };
    trap_cx.advance_user_pc(ArchTrapMachine::syscall_instruction_len());
    trap_cx.set_syscall_ret(parent_pid);
    trap_cx.in_syscall = true;
    trap_cx.restartable_syscall = false;
    trap_cx.set_kernel_hartid(hartid());

    let user_token = process.inner_exclusive_access().get_user_token();
    #[cfg(not(feature = "trap_stvec_probe"))]
    set_user_trap_entry();
    unsafe { ArchTrapMachine::return_to_user(trap_cx_user_va, user_token) }
}

#[cfg(target_arch = "riscv64")]
fn faulting_user_instruction(stval: usize, pc: usize) -> Option<u32> {
    if stval != 0 {
        let instruction = stval as u32;
        return Some(if instruction & 0b11 == 0b11 {
            instruction
        } else {
            instruction & 0xffff
        });
    }

    // `stval` is allowed to be zero for illegal-instruction traps.  Read with
    // execute permission rather than assuming an executable page is also
    // readable, and collect through the sliced translation so an instruction
    // spanning two pages remains supported.
    let read_instruction_bytes = |len: usize| -> Option<u32> {
        let buffers =
            translated_byte_buffer_with_access(pc as *const u8, len, PageFaultAccess::Exec).ok()?;
        let mut bytes = [0u8; 4];
        let mut copied = 0usize;
        for buffer in buffers {
            let copy_len = buffer.len().min(len.saturating_sub(copied));
            bytes[copied..copied + copy_len].copy_from_slice(&buffer[..copy_len]);
            copied += copy_len;
            if copied == len {
                break;
            }
        }
        (copied == len).then(|| u32::from_le_bytes(bytes))
    };

    let low = read_instruction_bytes(2)? as u16;
    if low & 0b11 != 0b11 {
        Some(low as u32)
    } else {
        read_instruction_bytes(4)
    }
}

#[cfg(target_arch = "riscv64")]
fn try_handle_lazy_user_fp(stval: usize) -> bool {
    let pc = current_trap_cx().user_pc();
    let Some(instruction) = faulting_user_instruction(stval, pc) else {
        return false;
    };
    let handled =
        crate::arch::riscv::trap::try_enable_user_fp(&mut current_trap_cx().arch, instruction);
    if handled {
        trace!(
            "[trap] lazy FP enable: hart={} pc={:#x} instruction={:#010x}",
            hartid(),
            pc,
            instruction
        );
    }
    handled
}

#[cfg(not(target_arch = "riscv64"))]
fn try_handle_lazy_user_fp(_stval: usize) -> bool {
    false
}

/// Snapshot the address-space state at a user fault.
///
/// Trap entry keeps the process address space active. Record both the hardware
/// token and the process token so diagnostics can verify that invariant and
/// expose TLB/address-space races.
fn log_user_fault_mapping(fault_addr: usize) {
    let process = current_process();
    let task = current_task();
    let (tid, thread_id) = task
        .as_ref()
        .and_then(|task| {
            let inner = task.inner_exclusive_access();
            inner
                .res
                .as_ref()
                .map(|res| (Some(res.tid), Some(res.thread_id)))
        })
        .unwrap_or((None, None));
    let kernel_satp = unsafe { crate::hal::current_address_space_token() };
    let vpn = crate::mm::VirtAddr::from(fault_addr).floor();
    let page_offset = fault_addr & (PAGE_SIZE - 1);

    let (user_token, active_user_harts, pte_info, vma_info) = {
        let inner = process.inner_exclusive_access();
        let memory_set = &inner.memory_set;
        let pte_info = memory_set
            .page_table
            .translate(vpn)
            .map(|pte| (pte.bits, pte.ppn().0, pte.flags()));
        let vma_info = memory_set.find_vma_containing(vpn).map(|vma| {
            let file_info = vma
                .file
                .as_ref()
                .map(|file| (file.pgoff, file.shared, file.file.path()));
            (
                vma.start_vpn().0,
                vma.end_vpn().0,
                vma.map_perm,
                vma.kind.clone(),
                file_info,
                vma.file_page_index(vpn),
            )
        });
        (
            memory_set.token(),
            memory_set.active_user_harts(),
            pte_info,
            vma_info,
        )
    };

    error!(
        "[kernel] user fault mapping: hart={} pid={} tid={:?} thread_id={:?} \
         addr={:#x} vpn={:#x} page_offset={:#x} kernel_satp={:#x} user_token={:#x} \
         active_user_harts={:#b} pte={:?} vma={:?}",
        hartid(),
        process.getpid(),
        tid,
        thread_id,
        fault_addr,
        vpn.0,
        page_offset,
        kernel_satp,
        user_token,
        active_user_harts,
        pte_info,
        vma_info,
    );
}

/// 输出用户态致命异常现场，区分 fault 地址、用户 PC 与关键寄存器。
fn log_user_fault(reason: &str, access: &str, fault_addr: usize, signal: &str) {
    let cx = current_trap_cx();
    let summary = cx.fault_dump_summary();
    let detail = cx.fault_dump_detail();
    log_user_fault_mapping(fault_addr);
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
    unsafe {
        ArchInterrupt::set_kernel_trap_entry();
    }
}
/// set trap entry for traps happen in user mode
pub fn set_user_trap_entry() {
    unsafe {
        ArchInterrupt::set_user_trap_entry();
    }
}

/// 为当前 hart 开启 supervisor timer interrupt。
pub fn enable_timer_interrupt() {
    unsafe {
        ArchInterrupt::enable_timer();
    }
}

/// 为当前 hart 关闭 supervisor timer interrupt。
pub fn disable_timer_interrupt() {
    unsafe {
        ArchInterrupt::disable_timer();
    }
}

/// 为当前 hart 关闭 supervisor external interrupt。
pub fn disable_external_interrupt() {
    unsafe {
        ArchInterrupt::disable_external();
    }
}

/// 为当前 hart 开启 supervisor software interrupt。
pub fn enable_software_interrupt() {
    unsafe {
        ArchInterrupt::enable_software();
    }
}

/// 清除当前 hart 挂起的 supervisor software interrupt。
pub fn clear_software_interrupt_pending() {
    unsafe {
        ArchInterrupt::clear_software_pending();
    }
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
    #[cfg(not(all(target_arch = "riscv64", feature = "trap_stvec_probe")))]
    set_kernel_trap_entry();
    #[cfg(all(target_arch = "riscv64", feature = "trap_stvec_probe"))]
    {
        let trap_info = ArchTrapMachine::read_trap_info();
        try_getpid_path_probe(&trap_info);
        // Non-getppid traps continue through the normal kernel path.
        set_kernel_trap_entry();
    }
    #[cfg(all(
        feature = "getpid_path_probe",
        not(feature = "trap_stvec_probe"),
        target_arch = "riscv64"
    ))]
    {
        let trap_info = ArchTrapMachine::read_trap_info();
        try_getpid_path_probe(&trap_info);
    }
    // The trampoline has entered kernel mode without changing the process page
    // table. Ack an older shootdown snapshot before taking locks or relying on
    // interrupt delivery.
    #[cfg(not(feature = "trap_tlb_poll_probe"))]
    crate::mm::poll_pending_shootdown();
    #[cfg(not(feature = "trap_accounting_probe"))]
    current_process().enter_kernel(get_time());
    current_trap_cx().in_syscall = false;
    current_trap_cx().restartable_syscall = false;
    let trap_info = ArchTrapMachine::read_trap_info();
    match trap_info.cause {
        TrapCause::UserSyscall => {
            #[cfg(not(feature = "trap_irq_guard_probe"))]
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
            #[cfg(all(
                target_arch = "riscv64",
                any(feature = "getpid_asm_probe", feature = "getpid_asm_satp_probe")
            ))]
            if syscall_id == crate::syscall::SYSCALL_GETPPID && result >= 0 {
                // x0 has no architectural restore action, so the probe build
                // can cache ppid+1; zero remains the "not cached" marker.
                cx.set_reg(0, result as usize + 1);
            }
            cx.set_syscall_ret(result as usize);
            cx.in_syscall = true;
        }
        TrapCause::StorePageFault => {
            let _probe = crate::probe_scope!("trap.user_page_fault.store");
            let _kernel_irq = irq::KernelIrqEnableGuard::new();
            trace!(
                "[mmap] trap store page fault: bad_addr={:#x} sepc={:#x}",
                trap_info.fault_addr,
                current_trap_cx().user_pc()
            );
            let process = current_process();
            let mut handled = false;
            match process.handle_user_store_fault(trap_info.fault_addr) {
                Ok(PageFaultHandled::Handled) => handled = true,
                Ok(PageFaultHandled::NotHandled) => {}
                Err(MmError::OutOfMemory) => {
                    handle_user_oom("user_store", "write", trap_info.fault_addr);
                }
                Err(_) => {}
            }
            if !handled {
                match current_process()
                    .handle_file_page_fault(trap_info.fault_addr, PageFaultAccess::Write)
                {
                    Ok(PageFaultHandled::Handled) => {}
                    Ok(PageFaultHandled::NotHandled) => {
                        log_user_fault(
                            "store page fault",
                            "write",
                            trap_info.fault_addr,
                            "SIGSEGV",
                        );
                        current_add_signal(SignalBit::SIGSEGV);
                    }
                    Err(MmError::BeyondFileEnd) => {
                        log_user_fault(
                            "store page fault beyond file EOF",
                            "write",
                            trap_info.fault_addr,
                            "SIGBUS",
                        );
                        current_add_signal(SignalBit::SIGBUS);
                    }
                    Err(MmError::OutOfMemory) => {
                        handle_user_oom("file_mmap", "write", trap_info.fault_addr);
                    }
                    Err(_) => {
                        log_user_fault(
                            "store page fault",
                            "write",
                            trap_info.fault_addr,
                            "SIGSEGV",
                        );
                        current_add_signal(SignalBit::SIGSEGV);
                    }
                }
            } else if log::log_enabled!(log::Level::Debug)
                && process.exec_path().ends_with("entry-static.exe")
            {
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
            let _probe = crate::probe_scope!("trap.user_page_fault.load");
            let _kernel_irq = irq::KernelIrqEnableGuard::new();
            // debug!(
            //     "[mmap] trap load page fault: bad_addr={:#x} sepc={:#x}",
            //     trap_info.fault_addr,
            //     current_trap_cx().user_pc()
            // );
            let mut handled = false;
            match current_process()
                .handle_lazy_user_fault(trap_info.fault_addr, PageFaultAccess::Read)
            {
                Ok(PageFaultHandled::Handled) => handled = true,
                Ok(PageFaultHandled::NotHandled) => {}
                Err(MmError::OutOfMemory) => {
                    handle_user_oom("lazy_user", "read", trap_info.fault_addr);
                }
                Err(_) => {}
            }
            if !handled {
                match current_process()
                    .handle_file_page_fault(trap_info.fault_addr, PageFaultAccess::Read)
                {
                    Ok(PageFaultHandled::Handled) => {}
                    Ok(PageFaultHandled::NotHandled) => {
                        log_user_fault("load page fault", "read", trap_info.fault_addr, "SIGSEGV");
                        current_add_signal(SignalBit::SIGSEGV);
                    }
                    Err(MmError::BeyondFileEnd) => {
                        log_user_fault(
                            "load page fault beyond file EOF",
                            "read",
                            trap_info.fault_addr,
                            "SIGBUS",
                        );
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
            let _probe = crate::probe_scope!("trap.user_page_fault.exec");
            let _kernel_irq = irq::KernelIrqEnableGuard::new();
            trace!(
                "[mmap] trap instruction page fault: bad_addr={:#x} sepc={:#x}",
                trap_info.fault_addr,
                current_trap_cx().user_pc()
            );
            let mut handled = false;
            match current_process()
                .handle_lazy_user_fault(trap_info.fault_addr, PageFaultAccess::Exec)
            {
                Ok(PageFaultHandled::Handled) => handled = true,
                Ok(PageFaultHandled::NotHandled) => {}
                Err(MmError::OutOfMemory) => {
                    handle_user_oom("lazy_user", "exec", trap_info.fault_addr);
                }
                Err(_) => {}
            }
            if !handled {
                match current_process()
                    .handle_file_page_fault(trap_info.fault_addr, PageFaultAccess::Exec)
                {
                    Ok(PageFaultHandled::Handled) => {}
                    Ok(PageFaultHandled::NotHandled) => {
                        log_user_fault(
                            "instruction page fault",
                            "exec",
                            trap_info.fault_addr,
                            "SIGSEGV",
                        );
                        current_add_signal(SignalBit::SIGSEGV);
                    }
                    Err(MmError::BeyondFileEnd) => {
                        log_user_fault(
                            "instruction page fault beyond file EOF",
                            "exec",
                            trap_info.fault_addr,
                            "SIGBUS",
                        );
                        current_add_signal(SignalBit::SIGBUS);
                    }
                    Err(MmError::OutOfMemory) => {
                        handle_user_oom("file_mmap", "exec", trap_info.fault_addr);
                    }
                    Err(_) => {
                        log_user_fault(
                            "instruction page fault",
                            "exec",
                            trap_info.fault_addr,
                            "SIGSEGV",
                        );
                        current_add_signal(SignalBit::SIGSEGV);
                    }
                }
            }
        }
        TrapCause::DataAddressFault => {
            #[cfg(target_arch = "loongarch64")]
            {
                match crate::arch::loongarch64::trap::emulate_user_unaligned(
                    current_trap_cx(),
                    trap_info.fault_addr,
                ) {
                    Ok(()) => {}
                    Err(err) => {
                        warn!(
                            "loongarch64 user unaligned emulation failed: ip={:#x}, fault_addr={:#x}, err={:?}",
                            current_trap_cx().user_pc(),
                            trap_info.fault_addr,
                            err,
                        );
                        log_user_fault(
                            "unaligned access",
                            "unknown",
                            trap_info.fault_addr,
                            "SIGBUS",
                        );
                        current_add_signal(SignalBit::SIGBUS);
                    }
                }
            }
            #[cfg(not(target_arch = "loongarch64"))]
            {
                log_user_fault("access fault", "unknown", trap_info.fault_addr, "SIGSEGV");
                current_add_signal(SignalBit::SIGSEGV);
            }
        }
        TrapCause::StoreFault | TrapCause::InstructionFault | TrapCause::LoadFault => {
            log_user_fault("access fault", "unknown", trap_info.fault_addr, "SIGSEGV");
            current_add_signal(SignalBit::SIGSEGV);
        }
        TrapCause::IllegalInstruction => {
            if !try_handle_lazy_user_fp(trap_info.fault_addr) {
                log_user_fault(
                    "illegal instruction",
                    "exec",
                    trap_info.fault_addr,
                    "SIGILL",
                );
                current_add_signal(SignalBit::SIGILL);
            }
        }
        TrapCause::TimerInterrupt => {
            let _hardirq = irq::HardIrqGuard::enter();
            // trace!("hart {} timer tick", hartid());
            if handle_timer_interrupt() {
                crate::probe!(
                    {
                        let now_raw = get_time();
                        check_itimers_of_all_processes(now_raw, get_realtime_ns());
                        crate::net::poll();
                        #[cfg(feature = "mm_perf_counters")]
                        crate::perf_sampler::on_tick(now_raw);
                        on_timer_tick();
                    },
                    "trap.user_timer_periodic"
                );
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
                trap_info.cause, trap_info.fault_addr
            );
        }
    }
    // check signals
    if let Some((signum, msg)) = check_fatal_signals_of_current() {
        let task = current_task();
        let (tid, thread_id) = task
            .as_ref()
            .and_then(|task| {
                let inner = task.inner_exclusive_access();
                inner
                    .res
                    .as_ref()
                    .map(|res| (Some(res.tid), Some(res.thread_id)))
            })
            .unwrap_or((None, None));
        let cx = current_trap_cx();
        warn!(
            "[signal] fatal signum={} hart={} pid={} tid={:?} thread_id={:?} \
             reason={} user_pc={:#x} user_sp={:#x}",
            signum,
            hartid(),
            current_process().getpid(),
            tid,
            thread_id,
            msg,
            cx.user_pc(),
            cx.user_sp(),
        );
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
    let trap_cx_user_va = current_trap_cx_user_va();
    current_trap_cx().set_kernel_hartid(hartid());
    let user_token = current_user_token();
    #[cfg(not(feature = "trap_accounting_probe"))]
    current_process().enter_user(get_time());
    unsafe { ArchTrapMachine::return_to_user(trap_cx_user_va, user_token) }
}

/// handle trap from kernel
#[no_mangle]
pub fn trap_from_kernel() {
    trap_from_kernel_impl(None);
}

/// RISC-V kernel-trap entry carrying the fault-time register frame saved by
/// `__trap_from_kernel`. LoongArch keeps using the frame-less compatibility
/// entry above until it grows an equivalent architecture-specific dump.
#[cfg(all(target_arch = "riscv64", feature = "kernel_trap_diagnostics"))]
#[no_mangle]
pub extern "C" fn trap_from_kernel_riscv(
    frame: *const crate::arch::riscv::trap::RiscvKernelTrapFrame,
) {
    trap_from_kernel_impl(Some(frame));
}

fn trap_from_kernel_impl(
    #[cfg(all(target_arch = "riscv64", feature = "kernel_trap_diagnostics"))] riscv_frame: Option<
        *const crate::arch::riscv::trap::RiscvKernelTrapFrame,
    >,
    #[cfg(not(all(target_arch = "riscv64", feature = "kernel_trap_diagnostics")))]
    _riscv_frame: Option<()>,
) {
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
                crate::probe!(
                    {
                        let now_raw = get_time();
                        check_itimers_of_all_processes(now_raw, get_realtime_ns());
                        crate::net::poll();
                        #[cfg(feature = "mm_perf_counters")]
                        crate::perf_sampler::on_tick(now_raw);
                        // Account CPU time spent while the current task executes in kernel
                        // context as part of its RR quantum as well. This matches Linux's
                        // "running on CPU" notion more closely than charging only
                        // user-mode ticks.
                        on_timer_tick();
                    },
                    "trap.kernel_timer_periodic"
                );
            }
        }
        TrapCause::SoftwareInterrupt => {
            handle_reschedule_ipi();
        }
        _ => {
            #[cfg(all(target_arch = "riscv64", feature = "kernel_trap_diagnostics"))]
            if let Some(frame) = riscv_frame {
                // SAFETY: the RISC-V assembly entry owns this frame until this
                // handler returns. Fatal traps panic before the frame can escape.
                unsafe { crate::arch::riscv::trap::log_kernel_trap_frame(frame) };
            }
            panic!(
                "Kernel trap: {:?}, fault_addr = {:#x}",
                trap_info.cause, trap_info.fault_addr
            );
        }
    }
    // check_timer();
}

pub use context::TrapContext;
pub use irq::{enter_noirq_lock, exit_noirq_lock, HardIrqGuard, KernelIrqEnableGuard};
