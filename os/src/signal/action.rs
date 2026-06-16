use crate::signal::signals::MAX_SIG;
use crate::syscall::Pod;
use crate::trap::TrapContext;

/// Action for a signal (Linux rt_sigaction layout)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SignalAction {
    /// Signal handler address (or SIG_DFL/SIG_IGN)
    pub handler: usize,
    /// Signal flags (SA_*)
    pub sa_flags: u32,
    /// Restorer function address (used when SA_RESTORER is set)
    pub sa_restorer: usize,
    /// Signal mask to apply during handler execution (64-bit Linux sigset layout)
    pub sa_mask: u64,
}

impl Pod for SignalAction {}

impl Default for SignalAction {
    fn default() -> Self {
        Self {
            handler: super::SIG_DFL,
            sa_flags: 0,
            sa_restorer: 0,
            sa_mask: 0,
        }
    }
}

/// Signal actions
#[derive(Clone)]
pub struct SignalActions {
    /// Signal actions table
    pub table: [SignalAction; MAX_SIG + 1],
}

impl Default for SignalActions {
    fn default() -> Self {
        Self {
            table: [SignalAction::default(); MAX_SIG + 1],
        }
    }
}

/// Linux-compatible `siginfo_t` subset for 64-bit user space.
///
/// We keep the fixed 128-byte ABI size and place `si_pid/si_uid` at the same
/// offsets expected by glibc/musl on 64-bit targets.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy)]
pub struct SigInfo {
    /// Signal number
    pub si_signo: i32,
    /// Error number
    pub si_errno: i32,
    /// Signal code
    pub si_code: i32,
    /// Explicit 64-bit alignment padding before the union payload.
    pub _pad0: i32,
    /// Sending process ID (`kill`, `tkill`, `tgkill`, `sigqueue`, ...)
    pub si_pid: i32,
    /// Sending real user ID.
    pub si_uid: u32,
    /// Remaining union payload bytes we do not currently model.
    pub _pad: [u32; 26],
}

impl Pod for SigInfo {}

impl Default for SigInfo {
    fn default() -> Self {
        Self::new(0)
    }
}

impl SigInfo {
    /// `si_code` used for user-originated `kill(2)` delivery.
    pub const SI_USER: i32 = 0;
    /// `si_code` used for kernel-originated signals.
    pub const SI_KERNEL: i32 = 0x80;
    /// `si_code` used for `tkill(2)`/`tgkill(2)` delivery.
    pub const SI_TKILL: i32 = -6;

    /// Create a new SigInfo with the given signal number
    pub const fn new(signo: i32) -> Self {
        Self {
            si_signo: signo,
            si_errno: 0,
            si_code: Self::SI_USER,
            _pad0: 0,
            si_pid: 0,
            si_uid: 0,
            _pad: [0; 26],
        }
    }

    /// Construct a sender-tagged siginfo using the kill/tkill union layout.
    pub const fn with_sender(signo: i32, si_code: i32, pid: usize, uid: u32) -> Self {
        Self {
            si_signo: signo,
            si_errno: 0,
            si_code,
            _pad0: 0,
            si_pid: pid as i32,
            si_uid: uid,
            _pad: [0; 26],
        }
    }

    /// Construct one `siginfo_t` matching `kill(2)` delivery semantics.
    pub const fn for_kill(signo: i32, pid: usize, uid: u32) -> Self {
        Self::with_sender(signo, Self::SI_USER, pid, uid)
    }

    /// Construct one `siginfo_t` matching `tkill(2)`/`tgkill(2)` delivery semantics.
    pub const fn for_tkill(signo: i32, pid: usize, uid: u32) -> Self {
        Self::with_sender(signo, Self::SI_TKILL, pid, uid)
    }

    /// Construct one kernel-originated `siginfo_t`.
    pub const fn for_kernel(signo: i32) -> Self {
        Self::with_sender(signo, Self::SI_KERNEL, 0, 0)
    }
}

/// Linux `sigset_t` layout used inside `ucontext_t` on 64-bit Linux ABIs.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SigSetT {
    /// 1024 signal bits stored as 16 64-bit words.
    pub bits: [u64; 16],
}

impl SigSetT {
    /// Construct a zeroed signal set.
    pub const fn empty() -> Self {
        Self { bits: [0; 16] }
    }

    /// Construct a signal set whose low 64 bits come from the kernel mask.
    pub const fn from_signal_bits(bits: u64) -> Self {
        let mut sigset = Self::empty();
        sigset.bits[0] = bits;
        sigset
    }

    /// Return the low 64 bits used by the current kernel signal implementation.
    pub const fn low_bits(self) -> u64 {
        self.bits[0]
    }
}

impl Default for SigSetT {
    fn default() -> Self {
        Self::empty()
    }
}

impl Pod for SigSetT {}

/// stack_t structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct StackT {
    /// Stack base address
    pub ss_sp: usize,
    /// Stack flags
    pub ss_flags: i32,
    /// Stack size
    pub ss_size: usize,
}

impl Pod for StackT {}

/// Architecture-specific Linux signal ABI hooks.
///
/// Concrete `rt_sigaction` and `ucontext_t` layouts differ across Linux
/// architectures. The common signal code owns delivery policy, while each
/// architecture owns the byte layout written to and read from userspace.
pub trait SignalAbi {
    /// Raw userspace layout accepted by `rt_sigaction(2)`.
    type UserSigAction: Copy + core::fmt::Debug + Pod;
    /// Raw userspace `ucontext_t` layout used by signal frames.
    type UContext: Copy + Pod;

    /// Convert a raw userspace `rt_sigaction` payload into kernel state.
    fn decode_user_sigaction(action: Self::UserSigAction) -> SignalAction;
    /// Convert kernel signal action state back into userspace layout.
    fn encode_user_sigaction(action: SignalAction) -> Self::UserSigAction;
    /// Return debug-friendly raw `rt_sigaction` fields: handler, flags, restorer, mask.
    fn user_sigaction_parts(action: &Self::UserSigAction) -> (usize, usize, usize, u64);
    /// Build a userspace `ucontext_t` for the interrupted trap context.
    fn build_ucontext(trap_cx: &TrapContext, old_mask: u64) -> Self::UContext;
    /// Return the low signal-mask bits stored in a userspace context.
    fn signal_mask(ucontext: &Self::UContext) -> u64;
    /// Restore the saved machine context into the trap context.
    fn restore_ucontext(ucontext: &Self::UContext, trap_cx: &mut TrapContext);
    /// Read the saved PC used for syscall-restart diagnostics.
    fn saved_pc(ucontext: &Self::UContext) -> usize;
    /// Update the saved PC before writing the context to userspace.
    fn set_saved_pc(ucontext: &mut Self::UContext, pc: usize);
    /// Read the saved first argument / return register.
    fn saved_arg0(ucontext: &Self::UContext) -> usize;
    /// Update the saved first argument / return register.
    fn set_saved_arg0(ucontext: &mut Self::UContext, value: usize);
}
