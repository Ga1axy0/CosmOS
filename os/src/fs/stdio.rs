use super::{console_tty, AccessMode, FileDescription, FileStatusFlags, TtyFile};
use crate::task::FdEntry;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

/// 基于同一个控制台 tty 构造默认的 stdin/stdout/stderr 三元组。
pub fn new_stdio_files() -> Vec<Option<FdEntry>> {
    // 复用全局控制台 tty 单例，使 UART 中断路径与 stdio 共享同一行规程状态
    // （前台进程组、规范行缓冲等）。
    let core = console_tty();
    vec![
        // fd 0: 可读，不可写。
        Some(FdEntry::new(Arc::new(FileDescription::new(
            Arc::new(TtyFile::new(Arc::clone(&core), true, false)),
            AccessMode::ReadOnly,
            FileStatusFlags::empty(),
            0,
        )))),
        // fd 1: 不可读，可写。
        Some(FdEntry::new(Arc::new(FileDescription::new(
            Arc::new(TtyFile::new(Arc::clone(&core), false, true)),
            AccessMode::WriteOnly,
            FileStatusFlags::empty(),
            0,
        )))),
        // fd 2: 不可读，可写。
        Some(FdEntry::new(Arc::new(FileDescription::new(
            Arc::new(TtyFile::new(core, false, true)),
            AccessMode::WriteOnly,
            FileStatusFlags::empty(),
            0,
        )))),
    ]
}
