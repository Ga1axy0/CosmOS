//! Kernel log buffer and `sys_syslog` helpers.
//!
//! This module keeps a small in-memory ring buffer for kernel log lines so
//! `SYSLOG_ACTION_READ` can return recent kernel messages to userspace.

use crate::drivers::chardev::uart_ready;
/// Close the log.  Currently a NOP.
pub const SYSLOG_ACTION_CLOSE: usize = 0;
/// Open the log.  Currently a NOP.
pub const SYSLOG_ACTION_OPEN: usize = 1;
/// Read from the log.
pub const SYSLOG_ACTION_READ: usize = 2;
/// Read all messages remaining in the ring buffer,
/// placing them in the buffer pointed to by bufp.
pub const SYSLOG_ACTION_READ_ALL: usize = 3;
/// Read and clear all messages remaining in the ring buffer.
pub const SYSLOG_ACTION_READ_CLEAR: usize = 4;
/// The call executes just the "clear ring buffer" command.
/// The bufp and size arguments are ignored.
pub const SYSLOG_ACTION_CLEAR: usize = 5;
/// The command saves the current value of console_loglevel and
/// then sets console_loglevel to minimum_console_loglevel, so
/// that no messages are printed to the console.
pub const SYSLOG_ACTION_CONSOLE_OFF: usize = 6;
/// If a previous SYSLOG_ACTION_CONSOLE_OFF command has been
/// performed, this command restores console_loglevel to the
/// value that was saved by that command.
pub const SYSLOG_ACTION_CONSOLE_ON: usize = 7;
/// The call sets console_loglevel to the value given in size,
/// which must be an integer between 1 and 8 (inclusive). 
pub const SYSLOG_ACTION_CONSOLE_LEVEL: usize = 8;
/// The call returns the number of bytes currently available to
/// be read from the kernel log buffer via command 2
/// (SYSLOG_ACTION_READ).
pub const SYSLOG_ACTION_SIZE_UNREAD: usize = 9;
///  This command returns the total size of the kernel log buffer.
pub const SYSLOG_ACTION_SIZE_BUFFER: usize = 10;

use alloc::collections::VecDeque;
use alloc::format;
use core::error;
use core::fmt;
use lazy_static::lazy_static;
use log::{Level, LevelFilter, Log, Metadata, Record};

use crate::mm::translated_byte_buffer;
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::syscall_body;
use crate::task::current_task;
use crate::task::current_user_token;

const KLOG_BUFFER_CAPACITY: usize = 16 * 1024;

/// a simple logger
struct SimpleLoggerPrinter;

impl Log for SimpleLoggerPrinter {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        append_log_record(record.level(), record.args());
        if !uart_ready() {
            return;
        }
        let color = match record.level() {
            Level::Error => 31, // Red
            Level::Warn => 93,  // BrightYellow
            Level::Info => 34,  // Blue
            Level::Debug => 32, // Green
            Level::Trace => 90, // BrightBlack
        };
        println!(
            "\u{1B}[{}m[{:>5}] {}\u{1B}[0m",
            color,
            record.level(),
            record.args(),
        );
    }

    fn flush(&self) {}
}

struct KernelLogBuffer {
    bytes: VecDeque<u8>,
    unread: usize,
    since_clear: usize,
}

impl KernelLogBuffer {
    const fn new() -> Self {
        Self {
            bytes: VecDeque::new(),
            unread: 0,
            since_clear: 0,
        }
    }

    fn push_bytes(&mut self, data: &[u8]) {
        if data.len() >= KLOG_BUFFER_CAPACITY {
            self.bytes.clear();
            self.bytes.extend(
                data[data.len() - KLOG_BUFFER_CAPACITY..]
                    .iter()
                    .copied(),
            );
            self.unread = KLOG_BUFFER_CAPACITY;
            self.since_clear = KLOG_BUFFER_CAPACITY;
            return;
        }
        while self.bytes.len() + data.len() > KLOG_BUFFER_CAPACITY {
            let _ = self.bytes.pop_front();
            if self.unread == self.bytes.len() + 1 {
                self.unread -= 1;
            }
            if self.since_clear == self.bytes.len() + 1 {
                self.since_clear -= 1;
            }
        }
        self.bytes.extend(data.iter().copied());
        self.unread = (self.unread + data.len()).min(KLOG_BUFFER_CAPACITY);
        self.since_clear = (self.since_clear + data.len()).min(KLOG_BUFFER_CAPACITY);
    }

    fn read_unread_into(&mut self, dst: &mut [u8]) -> usize {
        let available = self.unread.min(self.bytes.len());
        let to_read = dst.len().min(available);
        let start = self.bytes.len() - available;
        for (slot, byte) in dst.iter_mut().zip(self.bytes.iter().skip(start).take(to_read)) {
            *slot = *byte;
        }
        self.unread -= to_read;
        to_read
    }

    fn copy_suffix_into(&self, available: usize, skip: usize, dst: &mut [u8]) -> usize {
        let available = available.min(self.bytes.len());
        if skip >= available {
            return 0;
        }
        let to_read = dst.len().min(available - skip);
        let start = self.bytes.len() - available + skip;
        for (slot, byte) in dst.iter_mut().zip(self.bytes.iter().skip(start).take(to_read)) {
            *slot = *byte;
        }
        to_read
    }

    fn clear_since_marker(&mut self) {
        self.since_clear = 0;
    }

    fn unread_len(&self) -> usize {
        self.unread.min(self.bytes.len())
    }

    fn since_clear_len(&self) -> usize {
        self.since_clear.min(self.bytes.len())
    }
}

lazy_static! {
    static ref KLOG_BUFFER: SpinNoIrqLock<KernelLogBuffer> =
        SpinNoIrqLock::new(KernelLogBuffer::new());
}

/// initiate logger
pub fn init() {
    static LOGGER: SimpleLoggerPrinter = SimpleLoggerPrinter;
    log::set_logger(&LOGGER).unwrap();
    log::set_max_level(match option_env!("LOG") {
        Some("ERROR") => LevelFilter::Error,
        Some("WARN") => LevelFilter::Warn,
        Some("INFO") => LevelFilter::Info,
        Some("DEBUG") => LevelFilter::Debug,
        Some("TRACE") => LevelFilter::Trace,
        _ => LevelFilter::Off,
    });
}

/// Append one formatted kernel log line into the in-memory ring buffer.
pub fn append_log_record(level: log::Level, args: &fmt::Arguments<'_>) {
    let line = format!("[{:>5}] {}\n", level, args);
    KLOG_BUFFER.lock().push_bytes(line.as_bytes());
}

/// read and/or clear kernel message ring buffer;
/// set console_loglevel
pub fn sys_syslog(action: usize, bufp: *mut u8, size: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_syslog, action = {}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        action
    );
    match action {
        SYSLOG_ACTION_CLOSE => syslog_action_close(),
        SYSLOG_ACTION_OPEN => syslog_action_open(),
        SYSLOG_ACTION_READ => syslog_action_read(bufp, size),
        SYSLOG_ACTION_READ_ALL => syslog_action_read_all(bufp, size),
        SYSLOG_ACTION_READ_CLEAR => syslog_action_read_clear(bufp, size),
        SYSLOG_ACTION_CLEAR => syslog_action_clear(),
        SYSLOG_ACTION_CONSOLE_OFF => syslog_action_console_off(),
        SYSLOG_ACTION_CONSOLE_ON => syslog_action_console_on(),
        SYSLOG_ACTION_CONSOLE_LEVEL => syslog_action_console_level(size),
        SYSLOG_ACTION_SIZE_UNREAD => syslog_action_size_unread(),
        SYSLOG_ACTION_SIZE_BUFFER => syslog_action_size_buffer(),
        _ => bad_type(action),
    }
}

fn bad_type(action: usize) -> isize {
    syscall_body!({
        error!("unknown syslog action: id = {}", action);
        Err(ERRNO::ENOSYS)
    })
}

fn syslog_action_close() -> isize {
    // NOP
    0
}

fn syslog_action_open() -> isize {
    // NOP
    0
}

fn syslog_action_read(bufp: *mut u8, size: usize) -> isize {
    let token = current_user_token();
    syscall_body!({
        if size == 0 {
            return Ok(0);
        }
        let user_bufs = translated_byte_buffer(token, bufp as *const u8, size)
            .ok_or(ERRNO::EINVAL)?;
        let mut klog = KLOG_BUFFER.lock();
        let mut copied = 0usize;
        for slice in user_bufs {
            let read = klog.read_unread_into(slice);
            copied += read;
            if read < slice.len() {
                break;
            }
        }
        Ok(copied as isize)
    })
}

fn syslog_action_read_all(bufp: *mut u8, size: usize) -> isize {
    let token = current_user_token();
    syscall_body!({
        if size == 0 {
            return Ok(0);
        }
        let user_bufs = translated_byte_buffer(token, bufp as *const u8, size)
            .ok_or({
                error!("invalid user buffer for syslog read_all: bufp = {:x}, size = {}", bufp as usize, size);
                ERRNO::EINVAL
            })?;
        let klog = KLOG_BUFFER.lock();
        let available = klog.since_clear_len();
        let target_len = size.min(available);
        let mut copied = 0usize;
        for slice in user_bufs {
            if copied >= target_len {
                break;
            }
            let chunk_len = (target_len - copied).min(slice.len());
            copied +=
                klog.copy_suffix_into(available, available - target_len + copied, &mut slice[..chunk_len]);
        }
        Ok(copied as isize)
    })
}

fn syslog_action_read_clear(bufp: *mut u8, size: usize) -> isize {
    let token = current_user_token();
    syscall_body!({
        if size == 0 {
            KLOG_BUFFER.lock().clear_since_marker();
            return Ok(0);
        }
        let user_bufs = translated_byte_buffer(token, bufp as *const u8, size)
            .ok_or(ERRNO::EINVAL)?;
        let mut klog = KLOG_BUFFER.lock();
        let available = klog.since_clear_len();
        let target_len = size.min(available);
        let mut copied = 0usize;
        for slice in user_bufs {
            if copied >= target_len {
                break;
            }
            let chunk_len = (target_len - copied).min(slice.len());
            copied +=
                klog.copy_suffix_into(available, available - target_len + copied, &mut slice[..chunk_len]);
        }
        klog.clear_since_marker();
        Ok(copied as isize)
    })
}

fn syslog_action_clear() -> isize {
    KLOG_BUFFER.lock().clear_since_marker();
    0
}

// TODO：这里noxaiom也是简便实现跳过了下面的几个功能，因此暂且不管。
fn syslog_action_console_off() -> isize {
    -(ERRNO::ENOSYS as isize)
}

fn syslog_action_console_on() -> isize {
    -(ERRNO::ENOSYS as isize)
}

fn syslog_action_console_level(_size: usize) -> isize {
    -(ERRNO::ENOSYS as isize)
}

fn syslog_action_size_unread() -> isize {
    KLOG_BUFFER.lock().unread_len() as isize
}

fn syslog_action_size_buffer() -> isize {
    debug!("kernel log buffer capacity queried");
    KLOG_BUFFER_CAPACITY as isize
}
