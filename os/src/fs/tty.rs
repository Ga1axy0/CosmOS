use super::{File, Stat, StatMode};
use crate::drivers::chardev::{CharDevice, UART};
use crate::mm::{translated_ref, UserBuffer};
use crate::syscall::errno::ERRNO;
use crate::syscall::{write_pod_to_user, Pod};
use crate::task::current_user_token;
use crate::sync::SpinNoIrqLock;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::any::Any;

/// `ioctl(TCGETS)`：读取当前终端配置。
const TCGETS: usize = 0x5401;
/// `ioctl(TCSETS)`：立即更新终端配置。
const TCSETS: usize = 0x5402;
/// `ioctl(TCSETSW)`：等待输出完成后更新终端配置。
const TCSETSW: usize = 0x5403;
/// `ioctl(TCSETSF)`：等待输出完成并刷新输入后更新终端配置。
const TCSETSF: usize = 0x5404;
/// `ioctl(TIOCGWINSZ)`：读取窗口大小。
const TIOCGWINSZ: usize = 0x5413;
/// `ioctl(TIOCSWINSZ)`：设置窗口大小。
const TIOCSWINSZ: usize = 0x5414;

/// termios input flag: CR -> NL conversion.
const IFLAG_ICRNL: u32 = 0x0000_0100;
/// termios local flag: canonical mode (line buffering).
const LFLAG_ICANON: u32 = 0x0000_0002;
/// termios local flag: echo input characters.
const LFLAG_ECHO: u32 = 0x0000_0008;
/// termios local flag: echo erase (backspace).
const LFLAG_ECHOE: u32 = 0x0000_0010;

/// tty 终端配置的最小占位结构。
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
    pub cc: [u8; 19],
}

impl Default for Termios {
    /// 返回一份最小可用的终端配置占位值。
    fn default() -> Self {
        Self {
            iflag: IFLAG_ICRNL,
            oflag: 0,
            cflag: 0,
            lflag: LFLAG_ICANON | LFLAG_ECHO | LFLAG_ECHOE,
            line: 0,
            cc: [0; 19],
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
    input_buf: VecDeque<u8>,
    line_buf: VecDeque<u8>,
}

/// 共享 tty 核心，统一管理一个控制台终端的底层设备与状态。
pub struct TtyCore {
    driver: Arc<dyn CharDevice>,
    state: SpinNoIrqLock<TtyState>,
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
            state: {
                // TODO: 后续接入真正的 termios 初始化策略，而不是固定默认值。
                SpinNoIrqLock::new(TtyState {
                    termios: Termios::default(),
                    winsize: WinSize::default(),
                    input_buf: VecDeque::new(),
                    line_buf: VecDeque::new(),
                })
            },
            tx_lock: SpinNoIrqLock::new(()),
        }
    }

    /// 创建基于全局 UART 的默认控制台 tty。
    pub fn new_console() -> Self {
        let driver: Arc<dyn CharDevice> = UART.clone();
        Self::new(driver)
    }

    /// 从底层终端读取一个字节。
    pub fn read_byte(&self) -> u8 {
        // 原始读取：直接从底层驱动取字节（阻塞）。
        self.driver.read()
    }

    /// 是否已有可直接返回给用户的输入字节。
    pub fn has_ready_input(&self) -> bool {
        !self.state.lock().input_buf.is_empty()
    }

    /// 轮询是否具备可读输入：canonical 下必须已有完整行。
    pub fn poll_read_ready(&self) -> bool {
        let (has_ready, canonical) = {
            let state = self.state.lock();
            (
                !state.input_buf.is_empty(),
                (state.termios.lflag & LFLAG_ICANON) != 0,
            )
        };
        if has_ready {
            return true;
        }
        if canonical {
            return false;
        }
        self.driver.has_data()
    }

    /// 读取一个经过 tty 行规程处理后的字节。
    pub fn read_processed_byte(&self) -> u8 {
        loop {
            if let Some(ch) = self.state.lock().input_buf.pop_front() {
                return ch;
            }
            let raw = self.driver.read();
            self.ingest_input_byte(raw);
        }
    }

    fn ingest_input_byte(&self, raw: u8) {
        let mut echo_buf = [0u8; 3];
        let mut echo_len = 0usize;

        let mut state = self.state.lock();
        let termios = state.termios;
        let mut ch = raw;

        if (termios.iflag & IFLAG_ICRNL) != 0 && ch == b'\r' {
            ch = b'\n';
        }

        let canonical = (termios.lflag & LFLAG_ICANON) != 0;
        let echo_enabled = (termios.lflag & LFLAG_ECHO) != 0;
        let echo_erase = (termios.lflag & LFLAG_ECHOE) != 0;

        if canonical {
            match ch {
                b'\n' => {
                    state.line_buf.push_back(b'\n');
                    while let Some(b) = state.line_buf.pop_front() {
                        state.input_buf.push_back(b);
                    }
                    if echo_enabled {
                        echo_buf[0] = b'\r';
                        echo_buf[1] = b'\n';
                        echo_len = 2;
                    }
                }
                0x08 | 0x7f => {
                    if state.line_buf.pop_back().is_some() && echo_enabled && echo_erase {
                        echo_buf[0] = 0x08;
                        echo_buf[1] = b' ';
                        echo_buf[2] = 0x08;
                        echo_len = 3;
                    }
                }
                _ => {
                    state.line_buf.push_back(ch);
                    if echo_enabled {
                        echo_buf[0] = ch;
                        echo_len = 1;
                    }
                }
            }
        } else {
            state.input_buf.push_back(ch);
            if echo_enabled {
                if ch == b'\n' {
                    echo_buf[0] = b'\r';
                    echo_buf[1] = b'\n';
                    echo_len = 2;
                } else {
                    echo_buf[0] = ch;
                    echo_len = 1;
                }
            }
        }

        drop(state);
        if echo_enabled && echo_len > 0 {
            self.echo_bytes(&echo_buf[..echo_len]);
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
        // TODO: 后续在这里接入输出后处理，例如换行转换等。
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
            dev: 0,
            ino: 0,
            mode: StatMode::CHAR,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            pad0: 0,
            size: 0,
            blksize: 0,
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

    fn read_at(&self, _offset: usize, user_buf: UserBuffer) -> usize {
        // TODO: 后续接入非阻塞语义以及信号中断。
        let mut n = 0usize;
        for user_ptr in user_buf.into_iter() {
            let ch = self.core.read_processed_byte();
            unsafe {
                core::ptr::write_volatile(user_ptr, ch);
            }
            n += 1;

            if !self.core.has_ready_input() {
                break;
            }
        }
        n
    }

    fn read_bytes_at(&self, _offset: usize, buf: &mut [u8]) -> Result<usize, ERRNO> {
        let mut n = 0usize;
        for byte in buf.iter_mut() {
            *byte = self.core.read_processed_byte();
            n += 1;
            if !self.core.has_ready_input() {
                break;
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
        const POLLIN: u16 = 0x001;
        const POLLOUT: u16 = 0x004;

        let mut ready = 0u16;
        if self.readable && (events & POLLIN) != 0 && self.core.poll_read_ready() {
            ready |= POLLIN;
        }
        if self.writable && (events & POLLOUT) != 0 {
            ready |= POLLOUT;
        }
        ready
    }

    fn poll_source_id(&self) -> usize {
        self.core.poll_source_id()
    }

    fn ioctl(&self, req: usize, arg: usize) -> Result<isize, ERRNO> {
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
            _ => Err(ERRNO::ENOTTY),
        }
    }

    fn stat(&self) -> Stat {
        Self::stat_impl()
    }
}
