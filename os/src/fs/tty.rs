use super::{File, Stat, StatMode};
use crate::drivers::chardev::{CharDevice, UART};
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use alloc::sync::Arc;

/// tty 终端配置的最小占位结构。
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
            iflag: 0,
            oflag: 0,
            cflag: 0,
            lflag: 0,
            line: 0,
            cc: [0; 19],
        }
    }
}

/// tty 窗口大小的最小占位结构。
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
}

/// 共享 tty 核心，统一管理一个控制台终端的底层设备与状态。
pub struct TtyCore {
    driver: Arc<dyn CharDevice>,
    state: UPSafeCell<TtyState>,
}

unsafe impl Send for TtyCore {}
unsafe impl Sync for TtyCore {}

impl TtyCore {
    /// 基于底层字符设备创建一个共享 tty 核心。
    pub fn new(driver: Arc<dyn CharDevice>) -> Self {
        Self {
            driver,
            state: unsafe {
                // TODO: 后续接入真正的 termios 初始化策略，而不是固定默认值。
                UPSafeCell::new(TtyState {
                    termios: Termios::default(),
                    winsize: WinSize::default(),
                })
            },
        }
    }

    /// 创建基于全局 UART 的默认控制台 tty。
    pub fn new_console() -> Self {
        let driver: Arc<dyn CharDevice> = UART.clone();
        Self::new(driver)
    }

    /// 从底层终端读取一个字节。
    pub fn read_byte(&self) -> u8 {
        // TODO: 后续在这里接入行规程、回显和信号语义。
        self.driver.read()
    }

    /// 向底层终端写入一个字节。
    pub fn write_byte(&self, ch: u8) {
        // TODO: 后续在这里接入输出后处理，例如换行转换等。
        self.driver.write(ch);
    }

    /// 读取当前 tty 配置快照。
    pub fn termios(&self) -> Termios {
        self.state.exclusive_access().termios
    }

    /// 读取当前窗口大小快照。
    pub fn winsize(&self) -> WinSize {
        self.state.exclusive_access().winsize
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
    fn readable(&self) -> bool {
        self.readable
    }

    fn writable(&self) -> bool {
        self.writable
    }

    fn read(&self, mut user_buf: UserBuffer) -> usize {
        // 当前 tty 仍沿用旧控制台模型，一次只读取一个字节。
        // TODO: 支持按行缓冲、非阻塞和更通用的多字节读取。
        assert_eq!(user_buf.len(), 1);
        let ch = self.core.read_byte();
        unsafe {
            user_buf.buffers[0].as_mut_ptr().write_volatile(ch);
        }
        1
    }

    fn write(&self, buf: UserBuffer) -> usize {
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

    fn stat(&self) -> Stat {
        Self::stat_impl()
    }
}
