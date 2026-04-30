use crate::signal::signals::MAX_SIG;

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
    /// Signal mask to apply during handler execution (32-bit)
    pub sa_mask: u32,
}

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

/// siginfo_t structure (simplified for RISC-V)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SigInfo {
    /// Signal number
    pub si_signo: i32,
    /// Error number
    pub si_errno: i32,
    /// Signal code
    pub si_code: i32,
    /// Padding
    pub _pad: [u32; 29],
}

impl SigInfo {
    /// Create a new SigInfo with the given signal number
    pub fn new(signo: i32) -> Self {
        Self {
            si_signo: signo,
            si_errno: 0,
            si_code: 0, // SI_USER
            _pad: [0; 29],
        }
    }
}

/// mcontext_t structure for RISC-V (register snapshot)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct MContext {
    /// Program counter (pc/sepc)
    pub pc: usize,
    /// General-purpose registers x0-x31
    pub gregs: [usize; 32],
    /// Floating-point registers f0-f31
    pub fpregs: [u64; 32],
    /// Floating-point control and status register
    pub fcsr: usize,
}

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
    pub uc_sigmask: u32,
    /// Padding for alignment
    pub _pad: u32,
    /// Machine context (registers)
    pub uc_mcontext: MContext,
}

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
