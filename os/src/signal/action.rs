use crate::signal::signals::MAX_SIG;
use crate::syscall::Pod;

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

/// Linux `sigset_t` layout used inside `ucontext_t` on riscv64 glibc.
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

/// Floating-point state area embedded in riscv64 Linux `mcontext_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FpState {
    /// 32 double-precision FP registers.
    pub fpregs: [u64; 32],
    /// Floating-point control/status register.
    pub fcsr: u32,
    /// Padding that preserves the glibc-visible size of the FP-state union.
    pub reserved: [u32; 67],
}

impl Default for FpState {
    fn default() -> Self {
        Self {
            fpregs: [0; 32],
            fcsr: 0,
            reserved: [0; 67],
        }
    }
}

impl Pod for FpState {}

/// riscv64 Linux `mcontext_t`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MContext {
    /// General-purpose register file. Slot 0 stores the saved PC on riscv64.
    pub gregs: [usize; 32],
    /// Floating-point state blob.
    pub fpstate: FpState,
}

impl Pod for MContext {}

/// ucontext_t structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UContext {
    /// Context flags
    pub uc_flags: usize,
    /// Link to next context
    pub uc_link: usize,
    /// Signal stack
    pub uc_stack: StackT,
    /// Signal mask
    pub uc_sigmask: SigSetT,
    /// Machine context (registers)
    pub uc_mcontext: MContext,
}

impl Pod for UContext {}

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
