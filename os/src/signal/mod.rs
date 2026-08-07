//! Signal handling implementation.

use crate::{
    config::USER_VDSO_RT_SIGRETURN,
    hal::{
        traits::{TrapContextAbi, TrapMachine},
        ArchSignalAbi, ArchTrapContextAbi, ArchTrapMachine,
    },
    syscall::write_pod_to_user,
    task::{current_process, current_task, current_trap_cx, TaskControlBlock},
};

mod action;
mod signals;
mod wait;

pub use action::{SigInfo, SigSetT, SignalAbi, SignalAction, SignalActions, StackT};
pub use signals::{SignalBit, SignalNum, FIRST_RT_SIG, LAST_RT_SIG, MAX_SIG};
pub(crate) use wait::{
    cleanup_signal_wait, cleanup_signal_wait_for_task, handle_signal_wait_timeout,
    has_pending_signal_in_set, has_signalfd_pending_in_set, has_unmasked_pending_signal,
    notify_signal_wait_pid, notify_signal_wait_task, register_signal_wait, signal_wait_should_skip,
    signal_wait_state, take_pending_signal_in_set, take_signalfd_signal_in_set, SignalTimerTag,
    SignalWaitHandle, SignalWakeState,
};

bitflags! {
    /// Signal action flags (`sa_flags`) used by `rt_sigaction`.
    pub struct SaFlags: u32 {
        /// Don't send SIGCHLD when children stop
        const SA_NOCLDSTOP = 0x00000001;
        /// Don't create zombie on child death
        const SA_NOCLDWAIT = 0x00000002;
        /// Invoke signal-catching function with three arguments instead of one
        const SA_SIGINFO = 0x00000004;
        /// Use signal stack by calling sa_restorer
        const SA_RESTORER = 0x04000000;
        /// Call signal handler on alternate signal stack
        const SA_ONSTACK = 0x08000000;
        /// Restart syscall on signal return
        const SA_RESTART = 0x10000000;
        /// Don't automatically block the signal when its handler is being executed
        const SA_NODEFER = 0x40000000;
        /// Reset to SIG_DFL on entry to handler
        const SA_RESETHAND = 0x80000000;
    }
}

/// Default signal handler
pub const SIG_DFL: usize = 0;
/// Ignore signal
pub const SIG_IGN: usize = 1;

#[inline]
fn first_pending_signum(pending: SignalBit) -> Option<usize> {
    let bits = pending.bits();
    (bits != 0).then(|| bits.trailing_zeros() as usize + 1)
}

/// Return whether one task needs the locked signal slow path before user
/// return.  Callers supply a consistent snapshot protected by process/task
/// signal locks.
pub(crate) fn signal_work_needed(
    thread_pending: SignalBit,
    process_pending: SignalBit,
    signal_mask: SignalBit,
    restore_mask: bool,
) -> bool {
    restore_mask
        || !((thread_pending | process_pending) & !signal_mask.without_unblockable()).is_empty()
}

/// Recompute the current task's lock-free signal-work hint.
pub(crate) fn refresh_current_signal_work_pending() {
    let Some(task) = current_task() else {
        return;
    };
    let Some(process) = task.process.upgrade() else {
        task.set_signal_work_pending(false);
        return;
    };
    let process_inner = process.inner_exclusive_access();
    let task_inner = task.inner_exclusive_access();
    task.set_signal_work_pending(signal_work_needed(
        task_inner.pending_signals,
        process_inner.pending_signals,
        task_inner.signal_mask,
        task_inner.signal_mask_backup.is_some(),
    ));
}

#[inline]
fn refresh_signal_work_pending_locked(
    task: &TaskControlBlock,
    thread_pending: SignalBit,
    process_pending: SignalBit,
    signal_mask: SignalBit,
    restore_mask: bool,
) {
    task.set_signal_work_pending(signal_work_needed(
        thread_pending,
        process_pending,
        signal_mask,
        restore_mask,
    ));
}

/// Returns whether the current task has an unmasked pending signal that should
/// interrupt a blocking syscall with `EINTR`.
///
/// A blocking read/wait is only interrupted by a signal that is actually going
/// to be acted upon: one delivered to a user handler, or one whose default
/// action terminates/stops the process. Signals whose default action is to be
/// ignored (`SIGCHLD`, `SIGURG`, `SIGCONT`, `SIGWINCH`) — or that are explicitly
/// `SIG_IGN` — do not interrupt, matching Linux `signal_pending()` semantics for
/// restartable syscalls. This is the predicate the tty line discipline uses to
/// turn a Ctrl+C-induced wakeup into an `EINTR` return.
pub fn has_interrupting_signal() -> bool {
    let task = current_task().unwrap();
    if !task.signal_work_pending() {
        return false;
    }
    let process = crate::task::current_process();
    let process_inner = process.inner_exclusive_access();
    let task_inner = task.inner_exclusive_access();
    let pending =
        (task_inner.pending_signals | process_inner.pending_signals) & !task_inner.signal_mask;
    if pending.is_empty() {
        return false;
    }
    let mut remaining = pending;
    while let Some(signum) = first_pending_signum(remaining) {
        let flag = SignalBit::from_signum(signum as u32).unwrap();
        remaining &= !flag;
        let handler = process_inner.signal_actions.table[signum].handler;
        if handler == SIG_IGN {
            continue;
        }
        if handler == SIG_DFL {
            // Default action: everything interrupts except the signals whose
            // default disposition is "ignore".
            match SignalNum::from_number(signum as u32) {
                Some(SignalNum::SIGCHLD)
                | Some(SignalNum::SIGURG)
                | Some(SignalNum::SIGCONT)
                | Some(SignalNum::SIGWINCH) => continue,
                _ => return true,
            }
        }
        // A user handler is installed.
        return true;
    }
    false
}

/// Check and handle non-fatal signals for the current process.
/// Returns Some((signum, action, siginfo)) if a signal needs to be handled by user handler.
/// Handles SIG_IGN by clearing the signal, and SIG_DFL by default behavior.
pub fn check_signals_of_current() -> Option<(i32, SignalAction, SigInfo)> {
    let task = current_task().unwrap();
    if !task.signal_work_pending() {
        return None;
    }
    let process = crate::task::current_process();
    let mut process_inner = process.inner_exclusive_access();
    let mut task_inner = task.inner_exclusive_access();
    let thread_pending = task_inner.pending_signals;
    let pending = (thread_pending | process_inner.pending_signals)
        & !task_inner.signal_mask.without_unblockable();

    // Visit only signals that are actually pending instead of scanning all 64
    // signal numbers on every slow-path entry.
    let mut remaining = pending;
    while let Some(signum) = first_pending_signum(remaining) {
        let flag = SignalBit::from_signum(signum as u32).unwrap();
        remaining &= !flag;
        let from_thread = thread_pending.contains(flag);
        let action = process_inner.signal_actions.table[signum];

        // SIG_IGN: clear the signal and continue.
        if action.handler == SIG_IGN {
            if from_thread {
                task_inner.pending_signals &= !flag;
            } else {
                process_inner.pending_signals &= !flag;
            }
            debug!("check_signals: signum={} ignored (SIG_IGN)", signum);
            continue;
        }

        // Fatal default actions are handled by the preceding fatal-signal
        // check.  Non-fatal defaults are consumed here.
        if action.handler == SIG_DFL {
            if flag.check_error().is_some() {
                continue;
            }
            if from_thread {
                task_inner.pending_signals &= !flag;
            } else {
                process_inner.pending_signals &= !flag;
            }
            debug!(
                "check_signals: signum={} cleared (SIG_DFL, non-fatal)",
                signum
            );
            continue;
        }

        // User-defined handler.
        if action.handler > 1 {
            let siginfo = if from_thread {
                task_inner.pending_siginfo[signum]
            } else {
                process_inner.pending_siginfo[signum]
            };
            if from_thread {
                task_inner.pending_signals &= !flag;
            } else {
                process_inner.pending_signals &= !flag;
            }
            refresh_signal_work_pending_locked(
                &task,
                task_inner.pending_signals,
                process_inner.pending_signals,
                task_inner.signal_mask,
                task_inner.signal_mask_backup.is_some(),
            );
            debug!(
                "check_signals: signum={} dispatching to handler={:#x}, flags={:#x}, si_code={}, si_pid={}, si_uid={}, from_thread={}",
                signum,
                action.handler,
                action.sa_flags,
                siginfo.si_code,
                siginfo.si_pid,
                siginfo.si_uid,
                from_thread
            );
            return Some((signum as i32, action, siginfo));
        }
    }
    if let Some(old_mask) = task_inner.signal_mask_backup.take() {
        task_inner.signal_mask = old_mask;
    }
    refresh_signal_work_pending_locked(
        &task,
        task_inner.pending_signals,
        process_inner.pending_signals,
        task_inner.signal_mask,
        task_inner.signal_mask_backup.is_some(),
    );
    None
}

/// Handle pending signals by setting up user-space signal handler invocation.
/// This modifies the trap context to call the signal handler when returning to user space.
/// Constructs a Linux-style sigframe with ucontext_t and siginfo_t.
///
/// Returns `Some(signum)` when signal delivery cannot be completed and the
/// current thread should be terminated with that signal instead of returning to
/// the faulting instruction. This mirrors Linux's behavior for cases like a
/// corrupted user stack where the kernel cannot build a signal frame.
///
/// Stack layout (from high to low address):
///   [original sp]
///   ... (grows down)
///   [siginfo_t] (if SA_SIGINFO)
///   [ucontext_t] <- aligned sp (this is what sp points to on entry to handler)
pub fn handle_signals() -> Option<i32> {
    let (signum, action, siginfo) = match check_signals_of_current() {
        Some(signal_info) => signal_info,
        None => return None,
    };

    let trap_cx = current_trap_cx();
    // Save the current user stack pointer
    let original_pc = trap_cx.user_pc();
    let original_sp = trap_cx.user_sp();
    let mut user_sp = original_sp;

    // Construct sigframe on user stack
    // Layout: sp points to ucontext, siginfo is above it if SA_SIGINFO

    // First, allocate space for siginfo_t if SA_SIGINFO is set
    if action.sa_flags & SaFlags::SA_SIGINFO.bits() != 0 {
        user_sp -= core::mem::size_of::<SigInfo>();
    }

    // Allocate space for ucontext_t
    user_sp -= core::mem::size_of::<<ArchSignalAbi as SignalAbi>::UContext>();

    // Align stack to 16 bytes BEFORE setting pointers
    user_sp &= !0xf;

    // Now set the pointers based on aligned sp
    let ucontext_ptr = user_sp;
    let siginfo_ptr = if action.sa_flags & SaFlags::SA_SIGINFO.bits() != 0 {
        user_sp + core::mem::size_of::<<ArchSignalAbi as SignalAbi>::UContext>()
    } else {
        0
    };

    // Construct ucontext_t
    let old_mask = {
        let task = current_task().unwrap();
        let mut inner = task.inner_exclusive_access();
        inner
            .signal_mask_backup
            .take()
            .unwrap_or(inner.signal_mask)
            .without_unblockable()
            .bits()
    };

    let mut ucontext = ArchSignalAbi::build_ucontext(trap_cx, old_mask);

    // Syscall restart: if the signal interrupted a syscall that returned -EINTR
    // and SA_RESTART is set, back up PC to the ecall instruction and restore
    // original a0 so the syscall can be restarted after sigreturn.
    if trap_cx.in_syscall {
        let result = trap_cx.syscall_ret() as isize;
        if result == -(crate::syscall::errno::ERRNO::EINTR as isize) {
            if trap_cx.restartable_syscall && action.sa_flags & SaFlags::SA_RESTART.bits() != 0 {
                let a0_idx = <ArchTrapContextAbi as TrapContextAbi>::signal_gpr_arg0_index();
                debug!(
                    "handle_signals: syscall restart: backing up PC from {:#x} to {:#x}, restoring a0 (gregs[{}]) from {:#x} to {:#x}",
                    ArchSignalAbi::saved_pc(&ucontext),
                    trap_cx
                        .user_pc()
                        .wrapping_sub(ArchTrapMachine::syscall_instruction_len()),
                    a0_idx,
                    ArchSignalAbi::saved_arg0(&ucontext), trap_cx.orig_a0
                );
                ArchSignalAbi::set_saved_pc(
                    &mut ucontext,
                    trap_cx
                        .user_pc()
                        .wrapping_sub(ArchTrapMachine::syscall_instruction_len()),
                );
                ArchSignalAbi::set_saved_arg0(&mut ucontext, trap_cx.orig_a0);
            } else if action.sa_flags & SaFlags::SA_RESTART.bits() != 0 {
                debug!(
                    "handle_signals: syscall returned EINTR but syscall is not restartable, preserving EINTR"
                );
            }
        } else {
            debug!(
                "handle_signals: in_syscall=true but result={:#x} (not EINTR), no restart",
                result as usize
            );
        }
    } else {
        debug!(
            "handle_signals: in_syscall=false, saved PC={:#x}",
            ArchSignalAbi::saved_pc(&ucontext)
        );
    }

    // Write ucontext to user stack
    if let Err(err) = write_pod_to_user(
        ucontext_ptr as *mut <ArchSignalAbi as SignalAbi>::UContext,
        &ucontext,
    ) {
        warn!(
            "[kernel] handle_signals: failed to write ucontext for signal {}: {:?}",
            signum, err
        );
        return Some(signum);
    }

    // Write siginfo if SA_SIGINFO is set
    if action.sa_flags & SaFlags::SA_SIGINFO.bits() != 0 {
        if let Err(err) = write_pod_to_user(siginfo_ptr as *mut SigInfo, &siginfo) {
            warn!(
                "[kernel] handle_signals: failed to write siginfo for signal {}: {:?}",
                signum, err
            );
            return Some(signum);
        }
    }

    // Apply signal mask during handler execution
    {
        let task = current_task().unwrap();
        let mut inner = task.inner_exclusive_access();
        // sa_mask 使用 Linux sigset_t bit 布局，可直接转为内核 SignalBit。
        if let Some(new_mask) = SignalBit::from_bits(action.sa_mask) {
            // SA_NODEFER: don't automatically block the signal being handled
            if action.sa_flags & SaFlags::SA_NODEFER.bits() == 0 {
                // Block the signal being handled
                if let Some(sig_flag) = SignalBit::from_signum(signum as u32) {
                    inner.signal_mask.insert(sig_flag);
                }
            }
            // Apply additional mask
            inner.signal_mask.insert(new_mask);
        }
    }
    refresh_current_signal_work_pending();

    // Set up trap context to call signal handler
    // sp points to ucontext (aligned)
    trap_cx.set_user_sp(user_sp);

    // Set up arguments based on SA_SIGINFO
    if action.sa_flags & SaFlags::SA_SIGINFO.bits() != 0 {
        // SA_SIGINFO: handler(signum, siginfo*, ucontext*)
        trap_cx.set_user_arg(0, signum as usize); // a0 = signum
        trap_cx.set_user_arg(1, siginfo_ptr); // a1 = siginfo*
        trap_cx.set_user_arg(2, ucontext_ptr); // a2 = ucontext*
    } else {
        // Traditional: handler(signum)
        trap_cx.set_user_arg(0, signum as usize); // a0 = signum
    }

    // Set return address (ra) to restorer or kernel fallback
    if action.sa_flags & SaFlags::SA_RESTORER.bits() != 0 && action.sa_restorer != 0 {
        trap_cx.set_ra(action.sa_restorer); // ra = sa_restorer
        debug!(
            "handle_signals: using user restorer at {:#x}",
            action.sa_restorer
        );
    } else {
        // RISC-V Linux 不要求用户态提供 SA_RESTORER，统一回到内核提供的 trampoline。
        trap_cx.set_ra(USER_VDSO_RT_SIGRETURN);
        debug!(
            "handle_signals: using kernel vdso rt_sigreturn at {:#x}",
            USER_VDSO_RT_SIGRETURN
        );
    }

    // Jump to signal handler
    trap_cx.set_user_pc(action.handler);

    // SIGINT/SIGSEGV/SIGBUS delivery is intentionally visible at WARN level:
    // these are the signals most useful when correlating Ctrl+C, fault
    // handling, and rt_sigreturn/context-restoration failures.  Ordinary
    // signal delivery remains at the existing DEBUG level.
    if matches!(signum, 2 | 7 | 11) {
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
        warn!(
            "[signal] deliver signum={} hart={} pid={} tid={:?} thread_id={:?} \
             old_pc={:#x} old_sp={:#x} handler={:#x} restorer={:#x} \
             new_sp={:#x} ucontext={:#x} siginfo={:#x} flags={:#x}",
            signum,
            crate::hal::hartid(),
            process.getpid(),
            tid,
            thread_id,
            original_pc,
            original_sp,
            action.handler,
            trap_cx.ra(),
            user_sp,
            ucontext_ptr,
            siginfo_ptr,
            action.sa_flags,
        );
    }

    debug!(
        "handle_signals: setup complete, jumping to handler={:#x}, ra={:#x}, sp={:#x}",
        trap_cx.user_pc(),
        trap_cx.ra(),
        trap_cx.user_sp()
    );
    None
}
