use crate::syscall::ERRNO;
use crate::task::UContext;
use crate::{
    mm::{translated_ref, translated_refmut},
    syscall::errno::OrErrno,
    syscall_body,
    task::{current_process, current_task, current_user_token, SignalAction, SignalFlags, WaitReason},
};

/// rt_sigaction 系统调用
pub fn sys_sigaction(
    signum: i32,
    action: *const SignalAction,
    old_action: *mut SignalAction,
    sigsetsize: usize,
) -> isize {
    trace!(
        "kernel:pid[{}] sys_sigaction signum={} action={:#x} old_action={:#x} sigsetsize={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        signum,
        action as usize,
        old_action as usize,
        sigsetsize
    );
    syscall_body!({
        // Validate sigsetsize - we use 32-bit sigset_t
        if sigsetsize != core::mem::size_of::<u32>() {
            debug!(
                "sys_sigaction: invalid sigsetsize={}, expected 4",
                sigsetsize
            );
        }

        let signum = signum as u32;
        if signum == 0 || signum as usize > crate::task::MAX_SIG {
            warn!("sys_sigaction: invalid signum={}", signum);
            return Err(ERRNO::EINVAL);
        }
        // SIGKILL (9) and SIGSTOP (19) cannot be caught or ignored
        if signum == 9 || signum == 19 {
            warn!("sys_sigaction: cannot modify SIGKILL or SIGSTOP");
            return Err(ERRNO::EINVAL);
        }

        let token = current_user_token();
        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        let slot = &mut inner.signal_actions.table[signum as usize];

        // Return old action if requested
        if !old_action.is_null() {
            let old = translated_refmut(token, old_action).or_errno(ERRNO::EFAULT)?;
            *old = *slot;
            debug!(
                "sys_sigaction: signum={}, returning old handler={:#x}, flags={:#x}",
                signum, slot.handler, slot.sa_flags
            );
        }

        // Set new action if provided
        if !action.is_null() {
            let new_action = *translated_ref(token, action).or_errno(ERRNO::EFAULT)?;

            // Validate handler address (SIG_DFL=0, SIG_IGN=1, or valid user address)
            if new_action.handler != crate::task::SIG_DFL
                && new_action.handler != crate::task::SIG_IGN
                && new_action.handler < 0x1000
            {
                warn!(
                    "sys_sigaction: invalid handler address {:#x}",
                    new_action.handler
                );
                return Err(ERRNO::EINVAL);
            }

            *slot = new_action;
        }

        Ok(0)
    })
}

/// sigprocmask / rt_sigprocmask 系统调用
///
/// Linux 语义：
///   how == SIG_BLOCK   (0) -> mask |= set
///   how == SIG_UNBLOCK (1) -> mask &= ~set
///   how == SIG_SETMASK (2) -> mask = set
/// 参数含义遵循 rt_sigprocmask: (int how, const sigset_t *set, sigset_t *oset, size_t sigsetsize)
pub fn sys_sigprocmask(how: i32, set: *const u32, oset: *mut u32, sigsetsize: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_sigprocmask",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        // We represent sigset as a u32 bitmask in this kernel; require user-provided
        // buffer to be large enough to hold at least a u32.
        if sigsetsize < core::mem::size_of::<u32>() {
            return Err(ERRNO::EINVAL);
        }

        let process = current_process();
        let mut inner = process.inner_exclusive_access();

        // If user requested old mask, write it out first.
        if !oset.is_null() {
            let old_bits = inner.signal_mask.bits();
            let slot = translated_refmut(token, oset).ok_or(ERRNO::EFAULT)?;
            *slot = old_bits;
        }

        // If user provided a new mask, apply according to `how`.
        if !set.is_null() {
            let new_bits = *translated_ref(token, set).or_errno(ERRNO::EFAULT)?;
            let new_mask = SignalFlags::from_bits(new_bits).or_errno(ERRNO::EINVAL)?;
            match how {
                0 => inner.signal_mask.insert(new_mask), // SIG_BLOCK
                1 => inner.signal_mask.remove(new_mask), // SIG_UNBLOCK
                2 => inner.signal_mask = new_mask,       // SIG_SETMASK
                _ => return Err(ERRNO::EINVAL),
            }
        }

        Ok(0)
    })
}

/// rt_sigreturn 系统调用
///
/// 从用户栈上的 sigframe 恢复寄存器状态和信号掩码。
/// sigframe 布局（从低地址到高地址）：
///   [aligned sp] -> [siginfo_t (optional)] -> [ucontext_t]
pub fn sys_sigreturn() -> isize {
    syscall_body!({
        let trap_cx = crate::task::current_trap_cx();
        let user_sp = trap_cx.x[2]; // Current sp
        let token = current_user_token();

        debug!(
            "sys_sigreturn: ENTRY sepc={:#x}, sp={:#x}, ra={:#x}, a0={:#x}, a7={:#x}",
            trap_cx.sepc, user_sp, trap_cx.x[1], trap_cx.x[10], trap_cx.x[17]
        );

        // Read ucontext from user stack at sp
        let ucontext_ptr = user_sp;
        let ucontext_ref = crate::mm::translated_ref(token, ucontext_ptr as *const UContext);
        let Some(ucontext) = ucontext_ref else {
            error!(
                "sys_sigreturn: failed to read ucontext at {:#x}",
                ucontext_ptr
            );
            return Err(ERRNO::EFAULT);
        };

        debug!(
            "sys_sigreturn: restoring context from {:#x}, sigmask={:#x}",
            ucontext_ptr, ucontext.uc_sigmask
        );

        // Restore signal mask
        {
            let process = current_process();
            let mut inner = process.inner_exclusive_access();
            if let Some(mask) = SignalFlags::from_bits(ucontext.uc_sigmask) {
                inner.signal_mask = mask;
                debug!("sys_sigreturn: restored signal mask to {:#x}", mask.bits());
            } else {
                warn!(
                    "sys_sigreturn: invalid signal mask {:#x}",
                    ucontext.uc_sigmask
                );
            }
        }

        // Restore registers from mcontext
        let mcontext = &ucontext.uc_mcontext;

        // Restore ALL registers from saved context, including ra
        trap_cx.x.copy_from_slice(&mcontext.gregs);

        // Restore sepc (program counter)
        trap_cx.sepc = mcontext.pc;

        // Restore floating-point registers
        trap_cx.f.copy_from_slice(&mcontext.fpregs);
        trap_cx.fcsr = mcontext.fcsr;

        // Return the original a0 value (which was saved in the trap context)
        Ok(trap_cx.x[10] as isize)
    })
}

/// rt_sigsuspend / sigsuspend 系统调用
///
/// 原子地替换信号掩码并挂起进程，直到信号到达。
/// 参数：
///   mask: 指向新信号掩码的指针
///   sigsetsize: 信号集大小（支持 4 或 8 字节，但只使用前 4 字节）
///
/// 返回值：
///   总是返回 -EINTR（被信号中断）
pub fn sys_sigsuspend(mask: *const u32, sigsetsize: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_sigsuspend mask={:#x} sigsetsize={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        mask as usize,
        sigsetsize
    );
    syscall_body!({
        // Validate sigsetsize - accept 4 (32-bit) or 8 (64-bit) sigset_t
        // We only use the first 32 bits for compatibility
        if sigsetsize != 4 && sigsetsize != 8 {
            debug!(
                "sys_sigsuspend: invalid sigsetsize={}, expected 4 or 8",
                sigsetsize
            );
            return Err(ERRNO::EINVAL);
        }

        let token = current_user_token();
        let process = current_process();

        // Read new mask from user space (only use the first 32 bits)
        let new_mask_bits = *translated_ref(token, mask).or_errno(ERRNO::EFAULT)?;
        let new_mask = SignalFlags::from_bits(new_mask_bits).or_errno(ERRNO::EINVAL)?;

        // Save old mask and atomically set new mask
        let old_mask = {
            let mut inner = process.inner_exclusive_access();
            let old = inner.signal_mask;
            inner.signal_mask = new_mask;
            debug!(
                "sys_sigsuspend: changed mask from {:#x} to {:#x}",
                old.bits(),
                new_mask.bits()
            );
            old
        };

        // Block until a signal arrives.
        // Check if any unmasked signals are already pending.
        let has_pending = {
            let inner = process.inner_exclusive_access();
            let unmasked_pending = inner.pending_signals & !inner.signal_mask;
            !unmasked_pending.is_empty()
        };

        if !has_pending {
            // Block directly without enqueuing in any WaitQueue.
            // This avoids polluting wait_exit_queue (shared with waitpid)
            // because otherwise wake_one from a child exit would be
            // consumed by sigsuspend instead of the real waitpid waiter.
            //
            // Set status to Interruptible first, then re-check for
            // pending signals to close the race window where a signal
            // arrives between has_pending and actually sleeping.
            let task = current_task().unwrap();
            {
                let mut task_inner = task.inner_exclusive_access();
                task_inner.task_status = crate::task::TaskStatus::Interruptible;
                task_inner.wait_reason = Some(WaitReason::SignalSuspend);
            }

            // Re-check: did a signal arrive before we could block?
            let should_block = {
                let inner = process.inner_exclusive_access();
                let unmasked_pending = inner.pending_signals & !inner.signal_mask;
                unmasked_pending.is_empty()
            };

            if should_block {
                crate::task::block_current_and_run_next(WaitReason::SignalSuspend);
            } else {
                // Signal arrived — cancel the sleep, keep running.
                let mut task_inner = task.inner_exclusive_access();
                if matches!(task_inner.task_status, crate::task::TaskStatus::Interruptible) {
                    task_inner.task_status = crate::task::TaskStatus::Running;
                    task_inner.wait_reason = None;
                }
            }
        }

        // When we wake up, restore the old signal mask
        {
            let mut inner = process.inner_exclusive_access();
            inner.signal_mask = old_mask;
            debug!(
                "sys_sigsuspend: restored mask to {:#x}",
                old_mask.bits()
            );
        }

        // sigsuspend always returns -EINTR after signal delivery
        Err(ERRNO::EINTR)
    })
}
