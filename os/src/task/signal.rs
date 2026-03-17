//! Signal flags and function for convert signal flag to integer & string

use bitflags::*;

/// Highest supported signal number for per-process signal tables.
pub const MAX_SIG: usize = 31;

bitflags! {
    /// Signal flags
    pub struct SignalFlags: u32 {
        /// Hangup.
        const SIGHUP    = 1 << 1;
        /// Interrupt
        const SIGINT    = 1 << 2;
        /// Quit.
        const SIGQUIT   = 1 << 3;
        /// Illegal instruction
        const SIGILL    = 1 << 4;
        /// Trace/breakpoint trap.
        const SIGTRAP   = 1 << 5;
        /// Abort
        const SIGABRT   = 1 << 6;
        /// Bus error.
        const SIGBUS    = 1 << 7;
        /// Floating point exception
        const SIGFPE    = 1 << 8;
        /// Kill.
        const SIGKILL   = 1 << 9;
        /// User-defined signal 1.
        const SIGUSR1   = 1 << 10;
        /// Segmentation fault
        const SIGSEGV   = 1 << 11;
        /// User-defined signal 2.
        const SIGUSR2   = 1 << 12;
        /// Broken pipe.
        const SIGPIPE   = 1 << 13;
        /// Alarm clock.
        const SIGALRM   = 1 << 14;
        /// Termination request.
        const SIGTERM   = 1 << 15;
        /// Stack fault.
        const SIGSTKFLT = 1 << 16;
        /// Child status changed.
        const SIGCHLD   = 1 << 17;
        /// Continue.
        const SIGCONT   = 1 << 18;
        /// Stop.
        const SIGSTOP   = 1 << 19;
        /// Terminal stop.
        const SIGTSTP   = 1 << 20;
        /// Background read from tty.
        const SIGTTIN   = 1 << 21;
        /// Background write to tty.
        const SIGTTOU   = 1 << 22;
        /// Urgent socket condition.
        const SIGURG    = 1 << 23;
        /// CPU time limit exceeded.
        const SIGXCPU   = 1 << 24;
        /// File size limit exceeded.
        const SIGXFSZ   = 1 << 25;
        /// Virtual timer expired.
        const SIGVTALRM = 1 << 26;
        /// Profiling timer expired.
        const SIGPROF   = 1 << 27;
        /// Window resize.
        const SIGWINCH  = 1 << 28;
        /// I/O now possible.
        const SIGIO     = 1 << 29;
        /// Power failure.
        const SIGPWR    = 1 << 30;
        /// Bad system call.
        const SIGSYS    = 1 << 31;
    }
}

impl SignalFlags {
    /// Convert a Linux signal number into the corresponding pending bit.
    pub fn from_signum(signum: u32) -> Option<Self> {
        match signum {
            1 => Some(Self::SIGHUP),
            2 => Some(Self::SIGINT),
            3 => Some(Self::SIGQUIT),
            4 => Some(Self::SIGILL),
            5 => Some(Self::SIGTRAP),
            6 => Some(Self::SIGABRT),
            7 => Some(Self::SIGBUS),
            8 => Some(Self::SIGFPE),
            9 => Some(Self::SIGKILL),
            10 => Some(Self::SIGUSR1),
            11 => Some(Self::SIGSEGV),
            12 => Some(Self::SIGUSR2),
            13 => Some(Self::SIGPIPE),
            14 => Some(Self::SIGALRM),
            15 => Some(Self::SIGTERM),
            16 => Some(Self::SIGSTKFLT),
            17 => Some(Self::SIGCHLD),
            18 => Some(Self::SIGCONT),
            19 => Some(Self::SIGSTOP),
            20 => Some(Self::SIGTSTP),
            21 => Some(Self::SIGTTIN),
            22 => Some(Self::SIGTTOU),
            23 => Some(Self::SIGURG),
            24 => Some(Self::SIGXCPU),
            25 => Some(Self::SIGXFSZ),
            26 => Some(Self::SIGVTALRM),
            27 => Some(Self::SIGPROF),
            28 => Some(Self::SIGWINCH),
            29 => Some(Self::SIGIO),
            30 => Some(Self::SIGPWR),
            31 => Some(Self::SIGSYS),
            _ => None,
        }
    }

    /// Map currently pending fatal signals to a signal number and log string.
    /// User handler dispatch is intentionally left for a later step.
    pub fn check_error(&self) -> Option<(i32, &'static str)> {
        if self.contains(Self::SIGKILL) {
            Some((9, "Killed, SIGKILL=9"))
        } else if self.contains(Self::SIGINT) {
            Some((2, "Killed, SIGINT=2"))
        } else if self.contains(Self::SIGILL) {
            Some((4, "Illegal Instruction, SIGILL=4"))
        } else if self.contains(Self::SIGABRT) {
            Some((6, "Aborted, SIGABRT=6"))
        } else if self.contains(Self::SIGFPE) {
            Some((8, "Erroneous Arithmetic Operation, SIGFPE=8"))
        } else if self.contains(Self::SIGSEGV) {
            Some((11, "Segmentation Fault, SIGSEGV=11"))
        } else if self.contains(Self::SIGTERM) {
            Some((15, "Terminated, SIGTERM=15"))
        } else {
            None
        }
    }
}
