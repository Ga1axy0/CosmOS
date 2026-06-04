//! Signal number and sigset bit helpers.

use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not};

/// Highest supported Linux signal number.
pub const MAX_SIG: usize = 64;
/// First Linux realtime signal number in the kernel-visible range.
pub const FIRST_RT_SIG: usize = 32;
/// Highest Linux realtime signal number in the kernel-visible range.
pub const LAST_RT_SIG: usize = MAX_SIG;

const SUPPORTED_SIGNAL_BITS: u64 = u64::MAX;

/// Linux 信号编号。
///
/// 这里是内核中唯一维护信号编号的地方；信号集 bit 位统一由 `1 << (signum - 1)` 生成。
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SignalNum {
    /// Hangup.
    SIGHUP = 1,
    /// Interrupt.
    SIGINT = 2,
    /// Quit.
    SIGQUIT = 3,
    /// Illegal instruction.
    SIGILL = 4,
    /// Trace/breakpoint trap.
    SIGTRAP = 5,
    /// Abort.
    SIGABRT = 6,
    /// Bus error.
    SIGBUS = 7,
    /// Floating point exception.
    SIGFPE = 8,
    /// Kill.
    SIGKILL = 9,
    /// User-defined signal 1.
    SIGUSR1 = 10,
    /// Segmentation fault.
    SIGSEGV = 11,
    /// User-defined signal 2.
    SIGUSR2 = 12,
    /// Broken pipe.
    SIGPIPE = 13,
    /// Alarm clock.
    SIGALRM = 14,
    /// Termination request.
    SIGTERM = 15,
    /// Stack fault.
    SIGSTKFLT = 16,
    /// Child status changed.
    SIGCHLD = 17,
    /// Continue.
    SIGCONT = 18,
    /// Stop.
    SIGSTOP = 19,
    /// Terminal stop.
    SIGTSTP = 20,
    /// Background read from tty.
    SIGTTIN = 21,
    /// Background write to tty.
    SIGTTOU = 22,
    /// Urgent socket condition.
    SIGURG = 23,
    /// CPU time limit exceeded.
    SIGXCPU = 24,
    /// File size limit exceeded.
    SIGXFSZ = 25,
    /// Virtual timer expired.
    SIGVTALRM = 26,
    /// Profiling timer expired.
    SIGPROF = 27,
    /// Window resize.
    SIGWINCH = 28,
    /// I/O now possible.
    SIGIO = 29,
    /// Power failure.
    SIGPWR = 30,
    /// Bad system call.
    SIGSYS = 31,
}

impl SignalNum {
    /// 将 Linux 信号编号转换为 `SignalNum`。
    pub fn from_number(signum: u32) -> Option<Self> {
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

    /// 返回 Linux `sigset_t` 中该信号对应的 bit。
    pub const fn bit(self) -> u64 {
        1u64 << ((self as u8) - 1)
    }

    /// 返回 Linux 信号编号。
    pub const fn number(self) -> i32 {
        self as i32
    }
}

/// Linux 布局的信号编码集合。
///
/// 第 n 号信号对应 bit `1 << (n - 1)`；当前只支持 1..=31。
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct SignalBit(u64);

impl SignalBit {
    /// Signals that POSIX/Linux never allow user masks to block.
    pub const fn unblockable() -> Self {
        Self(Self::SIGKILL.bits() | Self::SIGSTOP.bits())
    }

    /// 空信号集。
    pub const fn empty() -> Self {
        Self(0)
    }

    /// 从单个信号号构造编码集合。
    pub const fn from_signal_num(signal: SignalNum) -> Self {
        Self(signal.bit())
    }

    /// 从 Linux 信号编号构造集合。
    pub fn from_signum(signum: u32) -> Option<Self> {
        if signum == 0 || signum as usize > MAX_SIG {
            None
        } else {
            Some(Self(1u64 << (signum - 1)))
        }
    }

    /// 从内核支持范围内的 Linux `sigset_t` 低 64 位构造集合。
    pub fn from_bits(bits: u64) -> Option<Self> {
        if bits & !SUPPORTED_SIGNAL_BITS == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// 从 Linux `sigset_t` 低 64 位构造集合，暂时忽略未支持的高位信号。
    pub fn from_user_bits(bits: u64) -> Self {
        // TODO: 当前只支持 1..=31 号信号，未来扩展实时信号后需要保留更多位。
        Self(bits & SUPPORTED_SIGNAL_BITS).without_unblockable()
    }

    /// 返回 Linux `sigset_t` 低 64 位布局。
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// 返回 Linux `sigset_t` 低 64 位布局。
    pub const fn user_bits(self) -> u64 {
        self.0
    }

    /// 判断集合是否为空。
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// 判断是否包含指定集合。
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// 插入指定集合。
    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }

    /// 移除指定集合。
    pub fn remove(&mut self, other: Self) {
        self.0 &= !other.0;
    }

    /// Strip SIGKILL/SIGSTOP from a user-provided signal mask.
    pub const fn without_unblockable(self) -> Self {
        Self(self.0 & !Self::unblockable().0)
    }

    /// Map currently pending fatal signals to a signal number and log string.
    /// User handler dispatch is intentionally left for a later step.
    pub fn check_error(&self) -> Option<(i32, &'static str)> {
        if self.contains(Self::SIGKILL) {
            Some((SignalNum::SIGKILL.number(), "Killed, SIGKILL=9"))
        } else if self.contains(Self::SIGINT) {
            Some((SignalNum::SIGINT.number(), "Killed, SIGINT=2"))
        } else if self.contains(Self::SIGILL) {
            Some((SignalNum::SIGILL.number(), "Illegal Instruction, SIGILL=4"))
        } else if self.contains(Self::SIGABRT) {
            Some((SignalNum::SIGABRT.number(), "Aborted, SIGABRT=6"))
        } else if self.contains(Self::SIGBUS) {
            Some((SignalNum::SIGBUS.number(), "Bus Error, SIGBUS=7"))
        } else if self.contains(Self::SIGFPE) {
            Some((SignalNum::SIGFPE.number(), "Erroneous Arithmetic Operation, SIGFPE=8"))
        } else if self.contains(Self::SIGSEGV) {
            Some((SignalNum::SIGSEGV.number(), "Segmentation Fault, SIGSEGV=11"))
        } else if self.contains(Self::SIGTERM) {
            Some((SignalNum::SIGTERM.number(), "Terminated, SIGTERM=15"))
        } else {
            None
        }
    }

    /// SIGHUP 对应的信号集合。
    pub const SIGHUP: Self = Self::from_signal_num(SignalNum::SIGHUP);
    /// SIGINT 对应的信号集合。
    pub const SIGINT: Self = Self::from_signal_num(SignalNum::SIGINT);
    /// SIGQUIT 对应的信号集合。
    pub const SIGQUIT: Self = Self::from_signal_num(SignalNum::SIGQUIT);
    /// SIGILL 对应的信号集合。
    pub const SIGILL: Self = Self::from_signal_num(SignalNum::SIGILL);
    /// SIGTRAP 对应的信号集合。
    pub const SIGTRAP: Self = Self::from_signal_num(SignalNum::SIGTRAP);
    /// SIGABRT 对应的信号集合。
    pub const SIGABRT: Self = Self::from_signal_num(SignalNum::SIGABRT);
    /// SIGBUS 对应的信号集合。
    pub const SIGBUS: Self = Self::from_signal_num(SignalNum::SIGBUS);
    /// SIGFPE 对应的信号集合。
    pub const SIGFPE: Self = Self::from_signal_num(SignalNum::SIGFPE);
    /// SIGKILL 对应的信号集合。
    pub const SIGKILL: Self = Self::from_signal_num(SignalNum::SIGKILL);
    /// SIGUSR1 对应的信号集合。
    pub const SIGUSR1: Self = Self::from_signal_num(SignalNum::SIGUSR1);
    /// SIGSEGV 对应的信号集合。
    pub const SIGSEGV: Self = Self::from_signal_num(SignalNum::SIGSEGV);
    /// SIGUSR2 对应的信号集合。
    pub const SIGUSR2: Self = Self::from_signal_num(SignalNum::SIGUSR2);
    /// SIGPIPE 对应的信号集合。
    pub const SIGPIPE: Self = Self::from_signal_num(SignalNum::SIGPIPE);
    /// SIGALRM 对应的信号集合。
    pub const SIGALRM: Self = Self::from_signal_num(SignalNum::SIGALRM);
    /// SIGTERM 对应的信号集合。
    pub const SIGTERM: Self = Self::from_signal_num(SignalNum::SIGTERM);
    /// SIGSTKFLT 对应的信号集合。
    pub const SIGSTKFLT: Self = Self::from_signal_num(SignalNum::SIGSTKFLT);
    /// SIGCHLD 对应的信号集合。
    pub const SIGCHLD: Self = Self::from_signal_num(SignalNum::SIGCHLD);
    /// SIGCONT 对应的信号集合。
    pub const SIGCONT: Self = Self::from_signal_num(SignalNum::SIGCONT);
    /// SIGSTOP 对应的信号集合。
    pub const SIGSTOP: Self = Self::from_signal_num(SignalNum::SIGSTOP);
    /// SIGTSTP 对应的信号集合。
    pub const SIGTSTP: Self = Self::from_signal_num(SignalNum::SIGTSTP);
    /// SIGTTIN 对应的信号集合。
    pub const SIGTTIN: Self = Self::from_signal_num(SignalNum::SIGTTIN);
    /// SIGTTOU 对应的信号集合。
    pub const SIGTTOU: Self = Self::from_signal_num(SignalNum::SIGTTOU);
    /// SIGURG 对应的信号集合。
    pub const SIGURG: Self = Self::from_signal_num(SignalNum::SIGURG);
    /// SIGXCPU 对应的信号集合。
    pub const SIGXCPU: Self = Self::from_signal_num(SignalNum::SIGXCPU);
    /// SIGXFSZ 对应的信号集合。
    pub const SIGXFSZ: Self = Self::from_signal_num(SignalNum::SIGXFSZ);
    /// SIGVTALRM 对应的信号集合。
    pub const SIGVTALRM: Self = Self::from_signal_num(SignalNum::SIGVTALRM);
    /// SIGPROF 对应的信号集合。
    pub const SIGPROF: Self = Self::from_signal_num(SignalNum::SIGPROF);
    /// SIGWINCH 对应的信号集合。
    pub const SIGWINCH: Self = Self::from_signal_num(SignalNum::SIGWINCH);
    /// SIGIO 对应的信号集合。
    pub const SIGIO: Self = Self::from_signal_num(SignalNum::SIGIO);
    /// SIGPWR 对应的信号集合。
    pub const SIGPWR: Self = Self::from_signal_num(SignalNum::SIGPWR);
    /// SIGSYS 对应的信号集合。
    pub const SIGSYS: Self = Self::from_signal_num(SignalNum::SIGSYS);
}

impl Default for SignalBit {
    fn default() -> Self {
        Self::empty()
    }
}

impl BitOr for SignalBit {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for SignalBit {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for SignalBit {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for SignalBit {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl Not for SignalBit {
    type Output = Self;

    fn not(self) -> Self::Output {
        Self(!self.0 & SUPPORTED_SIGNAL_BITS)
    }
}
