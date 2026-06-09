//! Kernel console output helpers.
use crate::drivers::chardev::{CharDevice, UART};
use core::fmt::{self, Write};
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, Ordering};

/// 串行化所有 hart 的控制台输出，避免多个 hart 同时逐字符写 UART 时互相穿插。
static CONSOLE_LOCK: AtomicBool = AtomicBool::new(false);

struct Stdout;

impl Write for Stdout {
    /// write str to console
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            UART.write(c as u8);
            // console_putchar(c as usize);
        }
        Ok(())
    }
}

struct EarlyStdout;

impl Write for EarlyStdout {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::platform::early_console_write(s);
        Ok(())
    }
}

/// 控制台输出期间的临界区守卫。
///
/// 它同时负责两件事：
/// 1. 通过全局自旋锁让多核输出按 `write_fmt` 为单位串行化；
/// 2. 临时关闭当前 hart 的 supervisor interrupt，避免同一 hart 在输出过程中
///    被中断后递归打印，造成自锁或进一步的字符交错。
struct ConsoleGuard {
    sie_was_enabled: bool,
}

impl ConsoleGuard {
    /// 获取控制台输出锁。
    fn lock() -> Self {
        let sie_was_enabled = crate::hal::local_irqs_enabled();
        unsafe { crate::hal::disable_local_irqs() };
        while CONSOLE_LOCK
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }
        Self { sie_was_enabled }
    }
}

impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        CONSOLE_LOCK.store(false, Ordering::Release);
        if self.sie_was_enabled {
            unsafe { crate::hal::enable_local_irqs() };
        }
    }
}
/// print to the host console using the format string and arguments.
pub fn print(args: fmt::Arguments) {
    let _guard = ConsoleGuard::lock();
    if crate::platform::use_early_console() {
        EarlyStdout.write_fmt(args).unwrap();
        return;
    }
    Stdout.write_fmt(args).unwrap();
}

/// Print! macro to the host console using the format string and arguments.
#[macro_export]
macro_rules! print {
    ($fmt: literal $(, $($arg: tt)+)?) => {
        $crate::console::print(format_args!($fmt $(, $($arg)+)?))
    }
}

/// Println! macro to the host console using the format string and arguments.
#[macro_export]
macro_rules! println {
    ($fmt: literal $(, $($arg: tt)+)?) => {
        $crate::console::print(format_args!(concat!($fmt, "\n") $(, $($arg)+)?))
    }
}
