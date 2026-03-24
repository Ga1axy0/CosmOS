use super::{TtyCore, TtyFile};
use crate::task::FdEntry;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

/// 基于同一个控制台 tty 构造默认的 stdin/stdout/stderr 三元组。
pub fn new_stdio_files() -> Vec<Option<FdEntry>> {
    let core = Arc::new(TtyCore::new_console());
    vec![
        // fd 0: 可读，不可写。
        Some(FdEntry::new(Arc::new(TtyFile::new(
            Arc::clone(&core),
            true,
            false,
        )))),
        // fd 1: 不可读，可写。
        Some(FdEntry::new(Arc::new(TtyFile::new(
            Arc::clone(&core),
            false,
            true,
        )))),
        // fd 2: 不可读，可写。
        Some(FdEntry::new(Arc::new(TtyFile::new(core, false, true)))),
    ]
}
