use crate::signal::{SigInfo, SignalWaitHandle, SignalWakeState, register_signal_wait};
use crate::syscall::{read_pod_from_user, write_pod_to_user, Pod, ERRNO};
use crate::task::UContext;
use crate::{
    mm::translated_ref,
    syscall_body,
    task::{
        block_current_and_run_next, current_process, current_task, current_user_token,
        SignalAction, SignalBit, TaskStatus, WaitReason,
    },
};
use crate::timer::{add_timer_with_signal_tag, get_time_ms};

use crate::syscall::OldTimespec32;

#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
struct SigSet32(u32);

impl Pod for SigSet32 {}

/// Linux `sigset_t` 的低 64 位表示。
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
struct SigSet64(u64);

impl Pod for SigSet64 {}

fn read_user_sigset(mask: *const u64, sigsetsize: usize) -> Result<SignalBit, ERRNO> {
    if mask.is_null() {
        return Err(ERRNO::EFAULT);
    }
    if sigsetsize >= core::mem::size_of::<u64>() {
        let bits = read_pod_from_user(mask as *const SigSet64)?.0;
        Ok(SignalBit::from_user_bits(bits))
    } else if sigsetsize >= core::mem::size_of::<u32>() {
        let bits = read_pod_from_user(mask as *const SigSet32)?.0;
        Ok(SignalBit::from_user_bits(bits as u64))
    } else {
        Err(ERRNO::EINVAL)
    }
}

fn write_user_sigset(mask: *mut u64, sigsetsize: usize, signal_set: SignalBit) -> Result<(), ERRNO> {
    if mask.is_null() {
        return Ok(());
    }
    if sigsetsize >= core::mem::size_of::<u64>() {
        write_pod_to_user(mask as *mut SigSet64, &SigSet64(signal_set.user_bits()))
    } else if sigsetsize >= core::mem::size_of::<u32>() {
        write_pod_to_user(mask as *mut SigSet32, &SigSet32(signal_set.bits() as u32))
    } else {
        Err(ERRNO::EINVAL)
    }
}

fn parse_sigtimedwait_timeout_ms(
    uts: *const OldTimespec32,
) -> Result<Option<usize>, ERRNO> {
    if uts.is_null() {
        return Ok(None);
    }
    let timeout = read_pod_from_user(uts)?;
    if timeout.tv_sec < 0 || timeout.tv_nsec < 0 || timeout.tv_nsec >= 1_000_000_000 {
        return Err(ERRNO::EINVAL);
    }
    let sec_ms = (timeout.tv_sec as u64)
        .checked_mul(1_000)
        .ok_or(ERRNO::EINVAL)?;
    let nsec_ms = (timeout.tv_nsec as u64).div_ceil(1_000_000);
    let timeout_ms = sec_ms.checked_add(nsec_ms).ok_or(ERRNO::EINVAL)?;
    Ok(Some(timeout_ms as usize))
}

fn timeout_ms_to_deadline(timeout_ms: Option<usize>) -> Result<Option<usize>, ERRNO> {
    match timeout_ms {
        None => Ok(None),
        Some(ms) => get_time_ms().checked_add(ms).map(Some).ok_or(ERRNO::EINVAL),
    }
}

fn read_signal_wait_set(uthese: *const u64, sigsetsize: usize) -> Result<SignalBit, ERRNO> {
    if uthese.is_null() {
        return Err(ERRNO::EINVAL);
    }
    read_user_sigset(uthese, sigsetsize)
}

fn signal_wait_sleep(handle: SignalWaitHandle, signal_set: SignalBit) {
    let task = current_task().unwrap();
    {
        debug!("signal_wait_sleep: blocking task of pid {} for signals {:#x}", task.process.upgrade().unwrap().getpid(), signal_set.bits());
        let mut task_inner = task.inner_exclusive_access();
        debug_assert!(matches!(task_inner.task_status, TaskStatus::Running));
        task_inner.task_status = TaskStatus::Interruptible;
        task_inner.wait_reason = Some(WaitReason::SignalTimedWait);
    }

    if crate::signal::has_pending_signal_in_set(signal_set)
        || crate::signal::has_unmasked_pending_signal()
        || crate::signal::signal_wait_should_skip(handle)
    {
        crate::signal::cleanup_signal_wait(handle);
        let mut task_inner = task.inner_exclusive_access();
        if matches!(task_inner.task_status, TaskStatus::Interruptible) {
            task_inner.task_status = TaskStatus::Running;
            task_inner.wait_reason = None;
        }
        return;
    }

    block_current_and_run_next(WaitReason::SignalTimedWait);
}

/// RISC-V Linux `rt_sigaction` 用户态 ABI 布局。
///
/// RISC-V 不使用 `SA_RESTORER` 字段，第三个 word 是 `sigset_t` 的低 64 位。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UserSigAction {
    /// handler 地址，或 SIG_DFL/SIG_IGN。
    pub handler: usize,
    /// Linux `SA_*` 标志位。
    pub sa_flags: usize,
    /// 用户态信号掩码，当前内核按低 64 位读取并裁剪未支持的信号位。
    pub sa_mask: u64,
}

impl Pod for UserSigAction {}

impl From<UserSigAction> for SignalAction {
    fn from(action: UserSigAction) -> Self {
        Self {
            handler: action.handler,
            sa_flags: action.sa_flags as u32,
            sa_restorer: 0,
            sa_mask: SignalBit::from_user_bits(action.sa_mask).bits(),
        }
    }
}

impl From<SignalAction> for UserSigAction {
    fn from(action: SignalAction) -> Self {
        Self {
            handler: action.handler,
            sa_flags: action.sa_flags as usize,
            sa_mask: SignalBit::from_bits(action.sa_mask)
                .unwrap_or(SignalBit::empty())
                .user_bits(),
        }
    }
}

/// rt_sigaction 系统调用
pub fn sys_sigaction(
    signum: i32,
    action: *const UserSigAction,
    old_action: *mut UserSigAction,
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
        if sigsetsize != 0 && sigsetsize != core::mem::size_of::<u64>() {
            // TODO: 当前用户库旧封装会传 0，后续完全切到 Linux ABI 后应严格要求 8。
            warn!(
                "sys_sigaction: invalid sigsetsize={}, expected 8",
                sigsetsize
            );
            return Err(ERRNO::EINVAL);
        }
        if sigsetsize == 0 {
            warn!("sys_sigaction: legacy sigsetsize=0 accepted");
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
        let new_action = if action.is_null() {
            None
        } else {
            let user_action = read_pod_from_user(action)?;
            for i in 0..3 {
                let word_ptr = (action as usize + i * core::mem::size_of::<usize>()) as *const usize;
                match translated_ref(token, word_ptr) {
                    Some(word) => debug!(
                        "sys_sigaction signum={} raw action[{}] addr={:#x} value={:#x}",
                        signum,
                        i,
                        word_ptr as usize,
                        *word
                    ),
                    None => warn!(
                        "sys_sigaction raw action[{}] addr={:#x} unreadable",
                        i,
                        word_ptr as usize
                    ),
                }
            }
            let new_action = SignalAction::from(user_action);
            debug!(
                "sys_sigaction parsed action: handler={:#x}, flags={:#x}, mask={:#x}",
                new_action.handler,
                new_action.sa_flags,
                new_action.sa_mask
            );
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
            Some(new_action)
        };

        let old = {
            let process = current_process();
            let mut inner = process.inner_exclusive_access();
            let slot = &mut inner.signal_actions.table[signum as usize];
            let old = *slot;
            if let Some(new_action) = new_action {
                *slot = new_action;
            }
            old
        };

        // Return old action after dropping process.inner; copyout may prefault user pages.
        if !old_action.is_null() {
            let user_old = UserSigAction::from(old);
            write_pod_to_user(old_action, &user_old)?;
            debug!(
                "sys_sigaction: signum={}, returning old handler={:#x}, flags={:#x}",
                signum, old.handler, old.sa_flags
            );
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
pub fn sys_sigprocmask(how: i32, set: *const u64, oset: *mut u64, sigsetsize: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_sigprocmask",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        // Linux sigset_t 使用 `1 << (signum - 1)` 布局；内核内部也保持同一布局。
        if sigsetsize < core::mem::size_of::<u32>() {
            return Err(ERRNO::EINVAL);
        }

        let new_mask = if !set.is_null() {
            Some(read_user_sigset(set, sigsetsize)?)
        } else {
            None
        };

        let old_bits = {
            let task = current_task().unwrap();
            let mut inner = task.inner_exclusive_access();
            let old_mask = inner.signal_mask;
            if let Some(new_mask) = new_mask {
                match how {
                    0 => inner.signal_mask.insert(new_mask), // SIG_BLOCK
                    1 => inner.signal_mask.remove(new_mask), // SIG_UNBLOCK
                    2 => inner.signal_mask = new_mask,       // SIG_SETMASK
                    _ => return Err(ERRNO::EINVAL),
                }
            }
            old_mask
        };

        // If user requested old mask, write it out after dropping process.inner.
        write_user_sigset(oset, sigsetsize, old_bits)?;

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

        debug!(
            "sys_sigreturn: ENTRY sepc={:#x}, sp={:#x}, ra={:#x}, a0={:#x}, a7={:#x}",
            trap_cx.sepc, user_sp, trap_cx.x[1], trap_cx.x[10], trap_cx.x[17]
        );

        // Read ucontext from user stack at sp. The frame can cross a page boundary,
        // so this must use the byte-wise copy helper instead of translated_ref.
        let ucontext_ptr = user_sp;
        let ucontext = read_pod_from_user(ucontext_ptr as *const UContext).map_err(|err| {
            error!(
                "sys_sigreturn: failed to read ucontext at {:#x}: {:?}",
                ucontext_ptr, err
            );
            err
        })?;

        debug!(
            "sys_sigreturn: restoring context from {:#x}, sigmask={:#x}",
            ucontext_ptr, ucontext.uc_sigmask
        );

        // Restore signal mask
        {
            let task = current_task().unwrap();
            let mut inner = task.inner_exclusive_access();
            let mask = SignalBit::from_user_bits(ucontext.uc_sigmask);
            inner.signal_mask = mask;
            inner.signal_mask_backup = None;
            debug!("sys_sigreturn: restored signal mask to {:#x}", mask.bits());
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
///   sigsetsize: 信号集大小（支持 4 或 8 字节）
///
/// 返回值：
///   总是返回 -EINTR（被信号中断）
pub fn sys_sigsuspend(mask: *const u64, sigsetsize: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_sigsuspend mask={:#x} sigsetsize={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        mask as usize,
        sigsetsize
    );
    syscall_body!({
        // Validate sigsetsize - accept 4 (compat) or 8 (native) sigset_t.
        if sigsetsize != 4 && sigsetsize != 8 {
            debug!(
                "sys_sigsuspend: invalid sigsetsize={}, expected 4 or 8",
                sigsetsize
            );
            return Err(ERRNO::EINVAL);
        }

        // Read new mask from user space.
        let new_mask = read_user_sigset(mask, sigsetsize)?;

        // Save old mask and atomically set new mask
        let old_mask = {
            let task = current_task().unwrap();
            let mut inner = task.inner_exclusive_access();
            let old = inner.signal_mask;
            inner.signal_mask = new_mask;
            inner.signal_mask_backup = Some(old);
            debug!(
                "sys_sigsuspend: changed mask from {:#x} to {:#x}",
                old.bits(),
                new_mask.bits()
            );
            old
        };

        // Block until a signal arrives.
        // Check if any unmasked signals are already pending.
        let has_pending = crate::signal::has_unmasked_pending_signal();

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
            let should_block = !crate::signal::has_unmasked_pending_signal();

            if should_block {
                crate::task::block_current_and_run_next(WaitReason::SignalSuspend);
            } else {
                // Signal arrived — cancel the sleep, keep running.
                let mut task_inner = task.inner_exclusive_access();
                if matches!(
                    task_inner.task_status,
                    crate::task::TaskStatus::Interruptible
                ) {
                    task_inner.task_status = crate::task::TaskStatus::Running;
                    task_inner.wait_reason = None;
                }
            }
        }

        // sigsuspend always returns -EINTR after signal delivery
        Err(ERRNO::EINTR)
    })
}

/// `rt_sigtimedwait_time32(2)`：等待并同步消费指定信号集合中的 pending signal。
///
/// 当前内核使用 64 位 `SignalBit`，并保留 4 字节 sigsetsize 的兼容读取。
pub fn sys_rt_sigtimedwait_time32(
    uthese: *const u64,
    uinfo: *mut SigInfo,
    uts: *const OldTimespec32,
    sigsetsize: usize,
) -> isize {
    debug!(
        "kernel:pid[{}] sys_rt_sigtimedwait_time32 uthese={:#x} uinfo={:#x} uts={:#x} sigsetsize={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        uthese as usize,
        uinfo as usize,
        uts as usize,
        sigsetsize
    );
    syscall_body!({
        let signal_set = read_signal_wait_set(uthese, sigsetsize)?;
        if signal_set.is_empty() {
            return Err(ERRNO::EINVAL);
        }
        let timeout_ms = parse_sigtimedwait_timeout_ms(uts)?;
        let deadline = timeout_ms_to_deadline(timeout_ms)?;

        loop {
            if let Some(signum) = crate::signal::take_pending_signal_in_set(signal_set) {
                if !uinfo.is_null() {
                    write_pod_to_user(uinfo, &SigInfo::new(signum))?;
                }
                return Ok(signum as isize);
            }

            if crate::signal::has_unmasked_pending_signal() {
                return Err(ERRNO::EINTR);
            }

            let now_ms = get_time_ms();
            if let Some(dl) = deadline {
                if now_ms >= dl {
                    return Err(ERRNO::EAGAIN);
                }
            }

            let task = current_task().unwrap();
            let pid = task.process.upgrade().unwrap().getpid();
            let handle = register_signal_wait(pid, signal_set, &task).ok_or(ERRNO::ENOSPC)?;
            if let Some(dl) = deadline {
                add_timer_with_signal_tag(dl, task.clone(), Some(handle.timer_tag()));
            }
            signal_wait_sleep(handle, signal_set);

            let wake_state = crate::signal::signal_wait_state(handle);
            crate::signal::cleanup_signal_wait(handle);

            match wake_state {
                SignalWakeState::TimedOut => return Err(ERRNO::EAGAIN),
                SignalWakeState::Ready | SignalWakeState::Canceled => {}
            }
        }
    })
}
