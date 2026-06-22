use super::{File, Stat, StatMode};
use fs::vfs::{VfsFileType, VfsNode};
use crate::drivers::chardev::{CharDevice, UART};
use crate::mm::{translated_ref, translated_refmut, UserBuffer};
use crate::poll::{notify_poll_source, POLLIN};
use crate::signal::{has_interrupting_signal, SigInfo, SignalBit, SignalNum};
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::syscall::{write_pod_to_user, Pod};
use crate::task::{current_process, current_user_token, send_signal_to_pgrp, WaitQueue, WaitReason};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::any::Any;
use lazy_static::lazy_static;

/// `ioctl(TCGETS)`：读取当前终端配置。
const TCGETS: usize = 0x5401;
/// `ioctl(TCSETS)`：立即更新终端配置。
const TCSETS: usize = 0x5402;
/// `ioctl(TCSETSW)`：等待输出完成后更新终端配置。
const TCSETSW: usize = 0x5403;
/// `ioctl(TCSETSF)`：等待输出完成并刷新输入后更新终端配置。
const TCSETSF: usize = 0x5404;
/// `ioctl(TCFLSH)`：刷新输入/输出队列。
const TCFLSH: usize = 0x540B;
/// `ioctl(TIOCSCTTY)`：将该终端设为调用进程会话的控制终端。
const TIOCSCTTY: usize = 0x540E;
/// `ioctl(TIOCGPGRP)`：读取前台进程组。
const TIOCGPGRP: usize = 0x540F;
/// `ioctl(TIOCSPGRP)`：设置前台进程组。
const TIOCSPGRP: usize = 0x5410;
/// Device ID of the devfs-like filesystem that contains the tty node.
const TTY_FS_DEV_ID: u64 = 0;
const TTY_INO: u64 = 1;
/// Linux-compatible `/dev/tty` character device number: major 5, minor 0.
const TTY_RDEV_MAJOR: u64 = 5;
const TTY_RDEV_MINOR: u64 = 0;
const TTY_RDEV: u64 = (TTY_RDEV_MAJOR << 8) | TTY_RDEV_MINOR;
/// `ioctl(TIOCGWINSZ)`：读取窗口大小。
const TIOCGWINSZ: usize = 0x5413;
/// `ioctl(TIOCSWINSZ)`：设置窗口大小。
const TIOCSWINSZ: usize = 0x5414;
/// `ioctl(TIOCNOTTY)`：放弃控制终端。
const TIOCNOTTY: usize = 0x5422;
/// `ioctl(TIOCGSID)`：读取该终端所属会话 id。
const TIOCGSID: usize = 0x5429;

/// `TCFLSH` 参数：刷新输入队列。
const TCIFLUSH: usize = 0;
/// `TCFLSH` 参数：刷新输出队列。
const TCOFLUSH: usize = 1;
/// `TCFLSH` 参数：刷新输入与输出队列。
const TCIOFLUSH: usize = 2;

/// termios input flag: ignore CR on input.
const IFLAG_IGNCR: u32 = 0x0000_0080;
/// termios input flag: map NL -> CR on input.
const IFLAG_INLCR: u32 = 0x0000_0040;
/// termios input flag: map CR -> NL on input.
const IFLAG_ICRNL: u32 = 0x0000_0100;
/// termios input flag: enable XON/XOFF output flow control (defined, not yet honored).
const IFLAG_IXON: u32 = 0x0000_0400;

/// termios output flag: perform implementation-defined output processing.
const OFLAG_OPOST: u32 = 0x0000_0001;
/// termios output flag: map NL to CR-NL on output.
const OFLAG_ONLCR: u32 = 0x0000_0004;

/// termios control flag: 8-bit characters.
const CFLAG_CS8: u32 = 0x0000_0030;
/// termios control flag: enable receiver.
const CFLAG_CREAD: u32 = 0x0000_0080;
/// termios control flag: default console baud selector (B38400 in asm-generic/termbits.h).
const CFLAG_B38400: u32 = 0x0000_000f;

/// termios local flag: enable signal-generating characters (INTR/QUIT/SUSP).
const LFLAG_ISIG: u32 = 0x0000_0001;
/// termios local flag: canonical mode (line buffering).
const LFLAG_ICANON: u32 = 0x0000_0002;
/// termios local flag: echo input characters.
const LFLAG_ECHO: u32 = 0x0000_0008;
/// termios local flag: echo erase (backspace) as destructive backspace.
const LFLAG_ECHOE: u32 = 0x0000_0010;
/// termios local flag: echo a newline after the KILL character.
const LFLAG_ECHOK: u32 = 0x0000_0020;
/// termios local flag: do not flush the input/output queues on signal chars.
const LFLAG_NOFLSH: u32 = 0x0000_0080;
/// termios local flag: echo control characters as `^X`.
const LFLAG_ECHOCTL: u32 = 0x0000_0200;
/// termios local flag: enable extended (implementation-defined) input processing.
const LFLAG_IEXTEN: u32 = 0x0000_8000;

/// Number of control characters in `Termios::cc` (Linux `NCCS` for generic arch).
const NCCS: usize = 19;

/// `c_cc` index: interrupt character (generates SIGINT). Default `^C`.
const VINTR: usize = 0;
/// `c_cc` index: quit character (generates SIGQUIT). Default `^\`.
const VQUIT: usize = 1;
/// `c_cc` index: erase character (erases the last char). Default DEL.
const VERASE: usize = 2;
/// `c_cc` index: kill character (erases the current line). Default `^U`.
const VKILL: usize = 3;
/// `c_cc` index: end-of-file character. Default `^D`.
const VEOF: usize = 4;
/// `c_cc` index: suspend character (generates SIGTSTP). Default `^Z`.
const VSUSP: usize = 10;
/// `c_cc` index: additional end-of-line character.
const VEOL: usize = 11;
/// `c_cc` index: second additional end-of-line character.
const VEOL2: usize = 16;

/// Linux `INIT_C_CC` for the N_TTY line discipline.
///
/// Index → value: VINTR=^C, VQUIT=^\, VERASE=DEL, VKILL=^U, VEOF=^D, VMIN=1,
/// VSTART=^Q, VSTOP=^S, VSUSP=^Z, VREPRINT=^R, VDISCARD=^O, VWERASE=^W,
/// VLNEXT=^V (matching the kernel's octal `INIT_C_CC` string).
const INIT_C_CC: [u8; NCCS] = [
    0x03, // VINTR    = ^C
    0x1c, // VQUIT    = ^\
    0x7f, // VERASE   = DEL
    0x15, // VKILL    = ^U
    0x04, // VEOF     = ^D
    0x00, // VTIME
    0x01, // VMIN
    0x00, // VSWTC
    0x11, // VSTART   = ^Q
    0x13, // VSTOP    = ^S
    0x1a, // VSUSP    = ^Z
    0x00, // VEOL
    0x12, // VREPRINT = ^R
    0x0f, // VDISCARD = ^O
    0x17, // VWERASE  = ^W
    0x16, // VLNEXT   = ^V
    0x00, // VEOL2
    0x00,
    0x00,
];

/// Returns whether `ch` matches the control character configured at `cc[idx]`.
///
/// A `c_cc` slot of `0` means the function is disabled (`_POSIX_VDISABLE`),
/// so a literal NUL byte never triggers a control action.
fn cc_matches(cc: &[u8; NCCS], idx: usize, ch: u8) -> bool {
    cc[idx] != 0 && cc[idx] == ch
}

/// Render the echo of a signal-generating control character as `^X` followed by
/// CR/LF (so the terminal advances to a fresh line). Returns the number of bytes
/// written into `out` (at most 4).
fn render_signal_echo(ch: u8, echo_ctl: bool, out: &mut [u8; 8]) -> usize {
    let mut n = 0;
    if echo_ctl && (ch < 0x20 || ch == 0x7f) {
        out[0] = b'^';
        out[1] = if ch == 0x7f { b'?' } else { ch ^ 0x40 };
        n = 2;
    } else {
        out[0] = ch;
        n = 1;
    }
    out[n] = b'\r';
    out[n + 1] = b'\n';
    n + 2
}

/// Render the echo of an ordinary input character. Control characters are shown
/// as `^X` when `echo_ctl` is set; `\n` is expanded to CR/LF. Returns the number
/// of bytes written into `out`.
fn render_input_echo(ch: u8, echo_ctl: bool, out: &mut [u8; 8]) -> usize {
    if ch == b'\n' {
        out[0] = b'\r';
        out[1] = b'\n';
        return 2;
    }
    if echo_ctl && ch != b'\t' && (ch < 0x20 || ch == 0x7f) {
        out[0] = b'^';
        out[1] = if ch == 0x7f { b'?' } else { ch ^ 0x40 };
        return 2;
    }
    out[0] = ch;
    1
}

/// tty 终端配置结构（Linux `struct termios` 布局）。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Termios {
    /// Input mode flags.
    pub iflag: u32,
    /// Output mode flags.
    pub oflag: u32,
    /// Control mode flags.
    pub cflag: u32,
    /// Local mode flags.
    pub lflag: u32,
    /// Line discipline selector.
    pub line: u8,
    /// Control characters array.
    pub cc: [u8; NCCS],
}

impl Default for Termios {
    /// 返回与 Linux N_TTY 初始值一致的终端配置。
    fn default() -> Self {
        Self {
            // 贴近 Linux N_TTY 默认值：回车映射为换行，并允许软件流控字符被识别。
            iflag: IFLAG_ICRNL | IFLAG_IXON,
            // 贴近控制台默认输出：将单个 newline 映射为 CRLF。
            oflag: OFLAG_OPOST | OFLAG_ONLCR,
            // 给用户态一个正常“8-bit + receiver enabled”的串口配置，避免出现 cs5/-cread。
            cflag: CFLAG_B38400 | CFLAG_CS8 | CFLAG_CREAD,
            // 行规程默认开启信号字符、规范模式与回显。
            lflag: LFLAG_ISIG
                | LFLAG_ICANON
                | LFLAG_ECHO
                | LFLAG_ECHOE
                | LFLAG_ECHOK
                | LFLAG_ECHOCTL
                | LFLAG_IEXTEN,
            line: 0,
            cc: INIT_C_CC,
        }
    }
}

// 允许 tty ioctl 将该 C ABI 结构整体写回用户空间。
impl Pod for Termios {}

/// tty 窗口大小的最小占位结构。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WinSize {
    /// Terminal rows in characters.
    pub rows: u16,
    /// Terminal columns in characters.
    pub cols: u16,
    /// Terminal width in pixels.
    pub xpixel: u16,
    /// Terminal height in pixels.
    pub ypixel: u16,
}

// 允许 tty ioctl 将该 C ABI 结构整体写回用户空间。
impl Pod for WinSize {}

impl Default for WinSize {
    /// 返回默认终端窗口大小占位值。
    fn default() -> Self {
        Self {
            rows: 24,
            cols: 80,
            xpixel: 0,
            ypixel: 0,
        }
    }
}

/// tty 运行时状态，集中保存可被多个端点共享的元数据。
struct TtyState {
    termios: Termios,
    winsize: WinSize,
    /// Bytes ready to be returned to a reader (a complete canonical line, or
    /// raw bytes in non-canonical mode).
    input_buf: VecDeque<u8>,
    /// The line currently being edited in canonical mode (not yet readable).
    line_buf: VecDeque<u8>,
    /// Session id of the controlling session (0 = none).
    session: u32,
    /// Foreground process group; terminal-generated signals target this group
    /// (0 = none, in which case such signals are dropped).
    fg_pgrp: u32,
    /// A pending end-of-file condition (Ctrl+D on an empty line) that makes the
    /// next blocking read return 0.
    eof: bool,
}

/// One post-processing action produced by feeding a byte through the line
/// discipline, to be performed after the state lock is dropped.
enum RxOutcome {
    /// Nothing to do beyond what already happened under the lock.
    None,
    /// Echo `len` bytes from the accompanying buffer.
    Echo { buf: [u8; 8], len: usize },
    /// Deliver `signal` to the foreground process group, after echoing the
    /// leading `len` bytes (the `^C`-style echo).
    Signal {
        signal: SignalBit,
        signum: i32,
        buf: [u8; 8],
        len: usize,
    },
}

/// 共享 tty 核心，统一管理一个控制台终端的底层设备与状态。
pub struct TtyCore {
    driver: Arc<dyn CharDevice>,
    state: SpinNoIrqLock<TtyState>,
    /// 等待规范行 / 原始输入到达的读者阻塞队列。
    read_wq: WaitQueue,
    /// 串行化整次 tty 写调用，避免多进程输出在字符级互相穿插。
    tx_lock: SpinNoIrqLock<()>,
}

unsafe impl Send for TtyCore {}
unsafe impl Sync for TtyCore {}

impl TtyCore {
    /// 基于底层字符设备创建一个共享 tty 核心。
    pub fn new(driver: Arc<dyn CharDevice>) -> Self {
        Self {
            driver,
            state: SpinNoIrqLock::new(TtyState {
                termios: Termios::default(),
                winsize: WinSize::default(),
                input_buf: VecDeque::new(),
                line_buf: VecDeque::new(),
                session: 0,
                fg_pgrp: 0,
                eof: false,
            }),
            read_wq: WaitQueue::new(),
            tx_lock: SpinNoIrqLock::new(()),
        }
    }

    /// 创建基于全局 UART 的默认控制台 tty。
    pub fn new_console() -> Self {
        let driver: Arc<dyn CharDevice> = UART.clone();
        Self::new(driver)
    }

    /// 是否已有可直接返回给用户的输入字节。
    pub fn has_ready_input(&self) -> bool {
        !self.state.lock().input_buf.is_empty()
    }

    /// 读者可被唤醒的条件：已有可读字节，或存在待返回的 EOF。
    fn read_ready(&self) -> bool {
        let state = self.state.lock();
        !state.input_buf.is_empty() || state.eof
    }

    /// 轮询是否具备可读输入：canonical 下必须已有完整行（或 EOF）。
    pub fn poll_read_ready(&self) -> bool {
        let (has_ready, eof, canonical) = {
            let state = self.state.lock();
            (
                !state.input_buf.is_empty(),
                state.eof,
                (state.termios.lflag & LFLAG_ICANON) != 0,
            )
        };
        if has_ready || eof {
            return true;
        }
        if canonical {
            return false;
        }
        self.driver.has_data()
    }

    /// 把底层设备当前可读的所有字节喂入行规程。
    ///
    /// 该方法同时被 UART 中断路径与阻塞读者调用：所有共享状态都由
    /// `SpinNoIrqLock` 保护（持锁即关本 hart 中断），因此两条路径不会在同一
    /// hart 上互相死锁；每个字节只会被 `read_nonblocking` 取出一次，故也不会
    /// 重复处理。回显与发信号都在释放状态锁之后进行。
    pub fn receive_from_driver(&self) {
        let mut received = false;
        while let Some(byte) = self.driver.read_nonblocking() {
            self.receive_byte(byte);
            received = true;
        }
        if received && self.read_ready() {
            self.read_wq.wake_all();
            notify_poll_source(self.poll_source_id(), POLLIN);
        }
    }

    /// 让一个原始输入字节通过行规程（n_tty 风格）处理。
    fn receive_byte(&self, raw: u8) {
        match self.process_byte(raw) {
            RxOutcome::None => {}
            RxOutcome::Echo { buf, len } => {
                self.echo_bytes(&buf[..len]);
            }
            RxOutcome::Signal {
                signal,
                signum,
                buf,
                len,
            } => {
                if len > 0 {
                    self.echo_bytes(&buf[..len]);
                }
                self.deliver_foreground_signal(signal, signum);
            }
        }
    }

    /// 在状态锁内处理单个字节，返回需要在锁外完成的后续动作。
    fn process_byte(&self, raw: u8) -> RxOutcome {
        let mut state = self.state.lock();
        let termios = state.termios;
        let mut ch = raw;

        // --- 输入标志（iflag）映射 ---
        if ch == b'\r' {
            if termios.iflag & IFLAG_IGNCR != 0 {
                return RxOutcome::None; // 整字节丢弃
            }
            if termios.iflag & IFLAG_ICRNL != 0 {
                ch = b'\n';
            }
        } else if ch == b'\n' && termios.iflag & IFLAG_INLCR != 0 {
            ch = b'\r';
        }

        let isig = termios.lflag & LFLAG_ISIG != 0;
        let canonical = termios.lflag & LFLAG_ICANON != 0;
        let echo_enabled = termios.lflag & LFLAG_ECHO != 0;
        let echo_erase = termios.lflag & LFLAG_ECHOE != 0;
        let echo_kill = termios.lflag & LFLAG_ECHOK != 0;
        let echo_ctl = termios.lflag & LFLAG_ECHOCTL != 0;
        let noflsh = termios.lflag & LFLAG_NOFLSH != 0;
        let cc = termios.cc;

        // --- 生成信号的控制字符（ISIG）---
        if isig {
            let sig = if cc_matches(&cc, VINTR, ch) {
                Some((SignalBit::SIGINT, SignalNum::SIGINT.number()))
            } else if cc_matches(&cc, VQUIT, ch) {
                Some((SignalBit::SIGQUIT, SignalNum::SIGQUIT.number()))
            } else if cc_matches(&cc, VSUSP, ch) {
                Some((SignalBit::SIGTSTP, SignalNum::SIGTSTP.number()))
            } else {
                None
            };
            if let Some((signal, signum)) = sig {
                // 默认在收到信号字符时刷新输入/编辑队列（除非设置 NOFLSH）。
                if !noflsh {
                    state.line_buf.clear();
                    state.input_buf.clear();
                    state.eof = false;
                }
                let mut buf = [0u8; 8];
                let len = if echo_enabled {
                    render_signal_echo(ch, echo_ctl, &mut buf)
                } else {
                    0
                };
                return RxOutcome::Signal {
                    signal,
                    signum,
                    buf,
                    len,
                };
            }
        }

        let mut buf = [0u8; 8];
        let mut echo_len = 0usize;

        if canonical {
            if ch == b'\n' || cc_matches(&cc, VEOL, ch) || cc_matches(&cc, VEOL2, ch) {
                // 行结束：把整行交付给读者。
                state.line_buf.push_back(ch);
                Self::flush_line(&mut state);
                if echo_enabled {
                    buf[0] = b'\r';
                    buf[1] = b'\n';
                    echo_len = 2;
                }
            } else if cc_matches(&cc, VEOF, ch) {
                // Ctrl+D：交付当前行；空行则产生 EOF（读返回 0）。VEOF 不回显。
                if state.line_buf.is_empty() {
                    state.eof = true;
                } else {
                    Self::flush_line(&mut state);
                }
            } else if cc_matches(&cc, VERASE, ch) || ch == 0x08 || ch == 0x7f {
                if state.line_buf.pop_back().is_some() && echo_enabled && echo_erase {
                    buf[0] = 0x08;
                    buf[1] = b' ';
                    buf[2] = 0x08;
                    echo_len = 3;
                }
            } else if cc_matches(&cc, VKILL, ch) {
                state.line_buf.clear();
                if echo_enabled && echo_kill {
                    buf[0] = b'\r';
                    buf[1] = b'\n';
                    echo_len = 2;
                }
            } else {
                state.line_buf.push_back(ch);
                if echo_enabled {
                    echo_len = render_input_echo(ch, echo_ctl, &mut buf);
                }
            }
        } else {
            // 非规范模式：字节立即可读。
            state.input_buf.push_back(ch);
            if echo_enabled {
                echo_len = render_input_echo(ch, echo_ctl, &mut buf);
            }
        }

        if echo_len > 0 {
            RxOutcome::Echo { buf, len: echo_len }
        } else {
            RxOutcome::None
        }
    }

    /// 把已编辑完成的一行从 `line_buf` 转移到 `input_buf`。
    fn flush_line(state: &mut TtyState) {
        while let Some(b) = state.line_buf.pop_front() {
            state.input_buf.push_back(b);
        }
    }

    /// 向控制终端的前台进程组投递终端生成的信号。
    fn deliver_foreground_signal(&self, signal: SignalBit, signum: i32) {
        let pgrp = self.state.lock().fg_pgrp;
        if pgrp != 0 {
            send_signal_to_pgrp(pgrp, signal, SigInfo::for_kernel(signum));
            if signal == SignalBit::SIGINT {
                crate::task::arm_debug_pgrp_task_dump(pgrp);
            }
        }
    }

    /// 阻塞读取一个经行规程处理后的字节。
    ///
    /// 返回 `Ok(Some(b))` 表示读到一个字节，`Ok(None)` 表示读到 EOF（Ctrl+D），
    /// `Err(EINTR)` 表示在没有任何可读数据时被信号中断。
    fn read_blocking(&self) -> Result<Option<u8>, ERRNO> {
        loop {
            // 1. 快速路径：已处理好的输入或待返回的 EOF。
            if let Some(byte) = self.take_ready_byte() {
                return byte;
            }
            // 2. 主动抽干底层设备（覆盖丢失中断 / 提前缓冲的数据）。
            self.receive_from_driver();
            if let Some(byte) = self.take_ready_byte() {
                return byte;
            }
            if !crate::platform::console_rx_irq_ready() {
                // Fall back to cooperative polling only before the platform
                // external IRQ path has finished setup.
                crate::task::yield_current_and_run_next();
                continue;
            }
            // 3. 阻塞，直到中断路径补满输入或有信号到来。
            self.read_wq
                .wait_with_reason_or_skip(WaitReason::UartRx, || {
                    self.read_ready() || has_interrupting_signal()
                });
            // 4. 被唤醒：数据优先于信号；否则上报 EINTR。
            if !self.read_ready() && has_interrupting_signal() {
                return Err(ERRNO::EINTR);
            }
        }
    }

    /// 取走一个立即可用的字节或 EOF；没有则返回 `None`。
    fn take_ready_byte(&self) -> Option<Result<Option<u8>, ERRNO>> {
        let mut state = self.state.lock();
        if let Some(ch) = state.input_buf.pop_front() {
            return Some(Ok(Some(ch)));
        }
        if state.eof {
            state.eof = false;
            return Some(Ok(None));
        }
        None
    }

    /// 若该终端尚无前台进程组，则由当前读者（通常是会话首领 / shell）接管为
    /// 控制终端并成为前台进程组，对应 Linux “会话首领打开终端即获得控制终端”。
    /// 一旦前台进程组被设置（读者接管或 `tcsetpgrp`），便不再自动改写。
    fn adopt_controlling_if_unset(&self) {
        if self.state.lock().fg_pgrp != 0 {
            return;
        }
        let process = current_process();
        let pgid = process.getpgid();
        let sid = process.getsid();
        if pgid == 0 {
            return;
        }
        let mut state = self.state.lock();
        if state.fg_pgrp == 0 {
            state.fg_pgrp = pgid;
            state.session = sid;
        }
    }

    fn echo_bytes(&self, bytes: &[u8]) {
        let _guard = self.tx_lock.lock();
        for &b in bytes {
            self.driver.write(b);
        }
    }

    /// 向底层终端写入一个字节。
    pub fn write_byte(&self, ch: u8) {
        // TODO: 后续在这里接入输出后处理（OPOST/ONLCR 等）。
        self.driver.write(ch);
    }

    /// 返回该 tty 共享底层设备的 poll 事件源标识。
    pub fn poll_source_id(&self) -> usize {
        Arc::as_ptr(&self.driver) as *const () as usize
    }

    /// 读取当前 tty 配置快照。
    pub fn termios(&self) -> Termios {
        self.state.lock().termios
    }

    /// 更新当前 tty 配置。
    pub fn set_termios(&self, termios: Termios) {
        self.state.lock().termios = termios;
    }

    /// 读取当前窗口大小快照。
    pub fn winsize(&self) -> WinSize {
        self.state.lock().winsize
    }

    /// 更新当前窗口大小。
    pub fn set_winsize(&self, winsize: WinSize) {
        self.state.lock().winsize = winsize;
    }

    /// 读取前台进程组。
    pub fn foreground_pgrp(&self) -> u32 {
        self.state.lock().fg_pgrp
    }

    /// 设置前台进程组（`tcsetpgrp` / `TIOCSPGRP`）。
    pub fn set_foreground_pgrp(&self, pgrp: u32) {
        self.state.lock().fg_pgrp = pgrp;
    }

    /// 读取控制会话 id。
    pub fn session(&self) -> u32 {
        self.state.lock().session
    }

    /// 将该终端设为指定会话的控制终端，并把前台进程组设为该进程组。
    pub fn set_controlling(&self, session: u32, pgrp: u32) {
        let mut state = self.state.lock();
        state.session = session;
        state.fg_pgrp = pgrp;
    }

    /// 放弃控制终端。
    pub fn drop_controlling(&self) {
        let mut state = self.state.lock();
        state.session = 0;
        state.fg_pgrp = 0;
    }

    /// 刷新输入与编辑队列。
    fn flush_input(&self) {
        let mut state = self.state.lock();
        state.input_buf.clear();
        state.line_buf.clear();
        state.eof = false;
    }
}

lazy_static! {
    /// 全局控制台 tty 单例。
    ///
    /// 它由 init 进程的 stdio 与 UART 中断路径共享：中断路径据此在输入到达时
    /// 立刻运行行规程（识别 Ctrl+C 等信号字符），而无需有进程正在 `read`。
    pub static ref CONSOLE_TTY: Arc<TtyCore> = Arc::new(TtyCore::new_console());
}

/// 返回全局控制台 tty。
pub fn console_tty() -> Arc<TtyCore> {
    CONSOLE_TTY.clone()
}

/// 由 UART 中断路径调用：把刚到达的输入喂入控制台行规程。
pub fn console_receive() {
    CONSOLE_TTY.receive_from_driver();
}

/// 挂接在 fd 表中的 tty 文件端点。
pub struct TtyFile {
    core: Arc<TtyCore>,
    readable: bool,
    writable: bool,
}

impl TtyFile {
    /// 创建一个绑定到共享 tty 核心的文件端点。
    pub fn new(core: Arc<TtyCore>, readable: bool, writable: bool) -> Self {
        Self {
            core,
            readable,
            writable,
        }
    }

    /// 返回此文件端点背后的共享 tty 核心。
    pub fn core(&self) -> Arc<TtyCore> {
        Arc::clone(&self.core)
    }

    /// 构造字符设备类型的 `stat` 结果。
    fn stat_impl() -> Stat {
        Stat {
            dev: TTY_FS_DEV_ID,
            ino: TTY_INO,
            mode: StatMode::CHAR | StatMode::from_bits_truncate(0o666),
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: TTY_RDEV,
            pad0: 0,
            size: 0,
            blksize: crate::config::PAGE_SIZE as u32,
            pad1: 0,
            blocks: 0,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
            unused: [0; 2],
        }
    }
}

impl File for TtyFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn read_at(&self, offset: usize, user_buf: UserBuffer) -> usize {
        self.read_at_result(offset, user_buf).unwrap_or(0)
    }

    fn read_at_result(&self, _offset: usize, user_buf: UserBuffer) -> Result<usize, ERRNO> {
        // 首次读取时按 Linux 语义为该终端确立控制会话 / 前台进程组。
        self.core.adopt_controlling_if_unset();
        let mut n = 0usize;
        for user_ptr in user_buf.into_iter() {
            match self.core.read_blocking() {
                Ok(Some(ch)) => {
                    unsafe {
                        core::ptr::write_volatile(user_ptr, ch);
                    }
                    n += 1;
                    if !self.core.has_ready_input() {
                        break;
                    }
                }
                Ok(None) => break, // EOF
                Err(err) => {
                    // 已读到部分数据时按短读返回，保留已拷贝的字节。
                    if n > 0 {
                        break;
                    }
                    return Err(err);
                }
            }
        }
        Ok(n)
    }

    fn read_bytes_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize, ERRNO> {
        self.core.adopt_controlling_if_unset();
        let mut n = 0usize;
        for byte in buf.iter_mut() {
            match self.core.read_blocking() {
                Ok(Some(ch)) => {
                    *byte = ch;
                    n += 1;
                    if !self.core.has_ready_input() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    if n > 0 {
                        break;
                    }
                    return Err(err);
                }
            }
        }
        Ok(n)
    }

    fn write_at(&self, _offset: usize, buf: UserBuffer) -> usize {
        // 以单次 `write` 为粒度串行化终端输出，尽量贴近 Linux tty 的整块写语义。
        let _tx_guard = self.core.tx_lock.lock();
        let mut n = 0usize;
        for slice in buf.buffers.iter() {
            for &ch in slice.iter() {
                // 逐字节透传到底层驱动，先保持与旧 stdio 行为一致。
                self.core.write_byte(ch);
                n += 1;
            }
        }
        n
    }

    fn write_bytes_at(&self, _offset: usize, buf: &[u8]) -> Result<usize, ERRNO> {
        let _tx_guard = self.core.tx_lock.lock();
        for &ch in buf {
            self.core.write_byte(ch);
        }
        Ok(buf.len())
    }

    fn poll(&self, events: u16) -> u16 {
        const POLLIN_BIT: u16 = 0x001;
        const POLLOUT_BIT: u16 = 0x004;

        let mut ready = 0u16;
        if self.readable && (events & POLLIN_BIT) != 0 && self.core.poll_read_ready() {
            ready |= POLLIN_BIT;
        }
        if self.writable && (events & POLLOUT_BIT) != 0 {
            ready |= POLLOUT_BIT;
        }
        ready
    }

    fn poll_source_id(&self) -> usize {
        self.core.poll_source_id()
    }

    fn ioctl(&self, req: usize, arg: usize) -> Result<isize, ERRNO> {
        // 任何控制终端交互（isatty/tcgetattr/tcgetpgrp 等）都可能是该会话第一次
        // 接触终端：此时按 Linux 语义确立控制会话与前台进程组。这样 shell 的
        // 作业控制初始化 `while (tcgetpgrp(fd) != getpgrp()) killpg(SIGTTIN)`
        // 能立刻成立并退出，而不会因前台进程组为 0 而空转。
        self.core.adopt_controlling_if_unset();
        let token = current_user_token();
        match req {
            TCGETS => {
                write_pod_to_user(arg as *mut Termios, &self.core.termios())?;
                Ok(0)
            }
            TCSETS | TCSETSW | TCSETSF => {
                let termios = *translated_ref(token, arg as *const Termios).ok_or(ERRNO::EFAULT)?;
                // TODO: 目前将 TCSETS/TCSETSW/TCSETSF 统一处理，尚未区分 drain/flush 语义。
                self.core.set_termios(termios);
                Ok(0)
            }
            TCFLSH => {
                match arg {
                    TCIFLUSH | TCIOFLUSH => self.core.flush_input(),
                    TCOFLUSH => {} // 输出无内核侧缓冲，无需处理。
                    _ => return Err(ERRNO::EINVAL),
                }
                Ok(0)
            }
            TIOCGWINSZ => {
                write_pod_to_user(arg as *mut WinSize, &self.core.winsize())?;
                Ok(0)
            }
            TIOCSWINSZ => {
                let winsize = *translated_ref(token, arg as *const WinSize).ok_or(ERRNO::EFAULT)?;
                // TODO: 更新窗口大小后，后续需要补发 SIGWINCH。
                self.core.set_winsize(winsize);
                Ok(0)
            }
            TIOCGPGRP => {
                let slot = translated_refmut(token, arg as *mut i32).ok_or(ERRNO::EFAULT)?;
                *slot = self.core.foreground_pgrp() as i32;
                Ok(0)
            }
            TIOCSPGRP => {
                let pgrp = *translated_ref(token, arg as *const i32).ok_or(ERRNO::EFAULT)?;
                if pgrp < 0 {
                    return Err(ERRNO::EINVAL);
                }
                self.core.set_foreground_pgrp(pgrp as u32);
                Ok(0)
            }
            TIOCGSID => {
                let slot = translated_refmut(token, arg as *mut i32).ok_or(ERRNO::EFAULT)?;
                *slot = self.core.session() as i32;
                Ok(0)
            }
            TIOCSCTTY => {
                // 将该终端设为调用进程会话的控制终端，前台进程组取其进程组。
                let process = current_process();
                self.core
                    .set_controlling(process.getsid(), process.getpgid());
                Ok(0)
            }
            TIOCNOTTY => {
                self.core.drop_controlling();
                Ok(0)
            }
            _ => Err(ERRNO::ENOTTY),
        }
    }

    fn stat(&self) -> Stat {
        Self::stat_impl()
    }
}


/// tty device node flavor exported under `/dev`.
#[derive(Clone, Copy, Debug)]
pub enum TtyDeviceKind {
    /// `/dev/console` bound to the global console tty.
    Console,
    /// `/dev/tty` bound to the current controlling console tty.
    Tty,
}

/// VFS node exposing the global console tty through a device path in `/dev`.
#[derive(Debug)]
pub struct TtyDeviceNode {
    kind: TtyDeviceKind,
    rdev: u64,
}

impl TtyDeviceNode {
    /// Create a tty device node with the given visible kind and device number.
    pub fn new(kind: TtyDeviceKind, rdev: u64) -> Self {
        Self { kind, rdev }
    }

    fn core(&self) -> Arc<TtyCore> {
        console_tty()
    }

    /// Handle tty-related ioctls through the shared console backend.
    pub fn ioctl(&self, req: usize, arg: usize) -> Result<isize, ERRNO> {
        let file = TtyFile::new(self.core(), true, true);
        file.ioctl(req, arg)
    }
}

impl VfsNode for TtyDeviceNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn ls(&self) -> alloc::vec::Vec<(alloc::string::String, VfsFileType)> {
        alloc::vec::Vec::new()
    }

    fn find(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn create(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn mkdir(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Char
    }

    fn clear(&self) {}

    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> usize {
        let core = self.core();
        core.adopt_controlling_if_unset();
        let mut n = 0usize;
        for byte in buf.iter_mut() {
            match core.read_blocking() {
                Ok(Some(ch)) => {
                    *byte = ch;
                    n += 1;
                    if !core.has_ready_input() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }
        n
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> usize {
        let core = self.core();
        let _tx_guard = core.tx_lock.lock();
        for &ch in buf {
            core.write_byte(ch);
        }
        buf.len()
    }

    fn rdev(&self) -> u64 {
        self.rdev
    }

    fn mode(&self) -> Option<u32> {
        Some((StatMode::CHAR | StatMode::from_bits_truncate(0o666)).bits())
    }

    fn uid(&self) -> Option<u32> {
        Some(0)
    }

    fn gid(&self) -> Option<u32> {
        Some(0)
    }

    fn size(&self) -> usize {
        0
    }
}
