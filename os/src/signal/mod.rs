//! Signal handling implementation.

use crate::{
    syscall::write_pod_to_user,
    task::{current_process, current_trap_cx},
};

mod action;
mod signals;
mod wait;

pub use action::{MContext, SigInfo, StackT, UContext, SignalAction, SignalActions};
pub use signals::{SignalFlags, MAX_SIG};
pub(crate) use wait::{
    cleanup_signal_wait, cleanup_signal_wait_for_task, handle_signal_wait_timeout,
    has_pending_signal_in_set, has_unmasked_pending_signal, notify_signal_wait_pid,
    register_signal_wait, signal_wait_should_skip, signal_wait_state, take_pending_signal_in_set,
    SignalTimerTag, SignalWaitHandle, SignalWakeState,
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

/// Check and handle non-fatal signals for the current process.
/// Returns Some((signum, action)) if a signal needs to be handled by user handler.
/// Handles SIG_IGN by clearing the signal, and SIG_DFL by default behavior.
pub fn check_signals_of_current() -> Option<(i32, SignalAction)> {
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let pending = process_inner.pending_signals & !process_inner.signal_mask;

    // Find the first pending signal
    for signum in 1..=MAX_SIG {
        let flag = SignalFlags::from_bits(1 << signum);
        if let Some(flag) = flag {
            if pending.contains(flag) {
                let action = process_inner.signal_actions.table[signum];

                // SIG_IGN: clear the signal and continue
                if action.handler == SIG_IGN {
                    process_inner.pending_signals &= !flag;
                    debug!("check_signals: signum={} ignored (SIG_IGN)", signum);
                    continue;
                }

                // SIG_DFL: use default behavior
                if action.handler == SIG_DFL {
                    // Check if this is a fatal signal with default behavior
                    if flag.check_error().is_some() {
                        continue;
                    } else {
                        process_inner.pending_signals &= !flag;
                        debug!(
                            "check_signals: signum={} cleared (SIG_DFL, non-fatal)",
                            signum
                        );
                        continue;
                    }
                }

                // User-defined handler
                if action.handler > 1 {
                    process_inner.pending_signals &= !flag;
                    debug!(
                        "check_signals: signum={} dispatching to handler={:#x}, flags={:#x}",
                        signum, action.handler, action.sa_flags
                    );
                    return Some((signum as i32, action));
                }
            }
        }
    }
    None
}

/// Handle pending signals by setting up user-space signal handler invocation.
/// This modifies the trap context to call the signal handler when returning to user space.
/// Constructs a Linux-style sigframe with ucontext_t and siginfo_t.
///
/// Stack layout (from high to low address):
///   [original sp]
///   ... (grows down)
///   [siginfo_t] (if SA_SIGINFO)
///   [ucontext_t] <- aligned sp (this is what sp points to on entry to handler)
pub fn handle_signals() {
    let (signum, action) = match check_signals_of_current() {
        Some(signal_info) => signal_info,
        None => return,
    };

    let trap_cx = current_trap_cx();
    let process = current_process();

    // Save the current user stack pointer
    let mut user_sp = trap_cx.x[2]; // sp register

    // Construct sigframe on user stack
    // Layout: sp points to ucontext, siginfo is above it if SA_SIGINFO

    // First, allocate space for siginfo_t if SA_SIGINFO is set
    if action.sa_flags & SaFlags::SA_SIGINFO.bits() != 0 {
        user_sp -= core::mem::size_of::<SigInfo>();
    }

    // Allocate space for ucontext_t
    user_sp -= core::mem::size_of::<UContext>();

    // Align stack to 16 bytes BEFORE setting pointers
    user_sp &= !0xf;

    // Now set the pointers based on aligned sp
    let ucontext_ptr = user_sp;
    let siginfo_ptr = if action.sa_flags & SaFlags::SA_SIGINFO.bits() != 0 {
        user_sp + core::mem::size_of::<UContext>()
    } else {
        0
    };

    // Construct ucontext_t
    let old_mask = {
        let inner = process.inner_exclusive_access();
        inner.signal_mask.bits()
    };

    let mut mcontext = MContext {
        pc: trap_cx.sepc, // Save program counter
        gregs: [0; 32],
        fpregs: [0; 32],
        fcsr: 0,
    };
    // Copy general-purpose registers
    mcontext.gregs.copy_from_slice(&trap_cx.x);
    // Copy floating-point registers
    mcontext.fpregs.copy_from_slice(&trap_cx.f);
    mcontext.fcsr = trap_cx.fcsr;

    let ucontext = UContext {
        uc_flags: 0,
        uc_link: 0,
        uc_stack: StackT {
            ss_sp: 0,
            ss_flags: 0,
            ss_size: 0,
        },
        uc_sigmask: old_mask,
        _pad: 0,
        uc_mcontext: mcontext,
    };

    // Write ucontext to user stack
    if let Err(err) = write_pod_to_user(ucontext_ptr as *mut UContext, &ucontext) {
        warn!(
            "[kernel] handle_signals: failed to write ucontext for signal {}: {:?}",
            signum, err
        );
        return;
    }

    // Write siginfo if SA_SIGINFO is set
    if action.sa_flags & SaFlags::SA_SIGINFO.bits() != 0 {
        let siginfo = SigInfo::new(signum);
        if let Err(err) = write_pod_to_user(siginfo_ptr as *mut SigInfo, &siginfo) {
            warn!(
                "[kernel] handle_signals: failed to write siginfo for signal {}: {:?}",
                signum, err
            );
            return;
        }
    }

    // Apply signal mask during handler execution
    {
        let mut inner = process.inner_exclusive_access();
        // Convert sa_mask to SignalFlags
        if let Some(new_mask) = SignalFlags::from_bits(action.sa_mask) {
            // SA_NODEFER: don't automatically block the signal being handled
            if action.sa_flags & SaFlags::SA_NODEFER.bits() == 0 {
                // Block the signal being handled
                if let Some(sig_flag) = SignalFlags::from_signum(signum as u32) {
                    inner.signal_mask.insert(sig_flag);
                }
            }
            // Apply additional mask
            inner.signal_mask.insert(new_mask);
        }
    }

    // Set up trap context to call signal handler
    // sp points to ucontext (aligned)
    trap_cx.x[2] = user_sp;

    // Set up arguments based on SA_SIGINFO
    if action.sa_flags & SaFlags::SA_SIGINFO.bits() != 0 {
        // SA_SIGINFO: handler(signum, siginfo*, ucontext*)
        trap_cx.x[10] = signum as usize; // a0 = signum
        trap_cx.x[11] = siginfo_ptr; // a1 = siginfo*
        trap_cx.x[12] = ucontext_ptr; // a2 = ucontext*
    } else {
        // Traditional: handler(signum)
        trap_cx.x[10] = signum as usize; // a0 = signum
    }

    // Set return address (ra) to restorer or kernel fallback
    if action.sa_flags & SaFlags::SA_RESTORER.bits() != 0 && action.sa_restorer != 0 {
        trap_cx.x[1] = action.sa_restorer; // ra = sa_restorer
        debug!(
            "handle_signals: using user restorer at {:#x}",
            action.sa_restorer
        );
    } else {
        // Kernel fallback: write a trampoline that calls rt_sigreturn
        // For now, we expect user to provide restorer
        // TODO: implement kernel trampoline page
        warn!("handle_signals: no restorer provided, signal may not return properly");
        trap_cx.x[1] = 0; // This will likely crash, but it's user's fault
    }

    // Jump to signal handler
    trap_cx.sepc = action.handler;

    debug!(
        "handle_signals: setup complete, jumping to handler={:#x}, ra={:#x}, sp={:#x}",
        trap_cx.sepc, trap_cx.x[1], trap_cx.x[2]
    );
}
