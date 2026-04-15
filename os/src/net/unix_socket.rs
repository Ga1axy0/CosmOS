use core::any::Any;

use alloc::sync::Arc;

use crate::{fs::{File, Pipe, Stat, StatMode}, mm::UserBuffer};

const POLLIN: u16 = 0x001;
const POLLOUT: u16 = 0x004;
const POLLHUP: u16 = 0x010;

/// 使用两条单向 pipe 交叉组合为一个全双工端点。
pub struct UnixSocketPairEnd {
    rx: Arc<Pipe>,
    tx: Arc<Pipe>,
}

impl UnixSocketPairEnd {
    /// 创建一对 UNIX socket
    pub fn new(rx: Arc<Pipe>, tx: Arc<Pipe>) -> Self {
        Self { rx, tx }
    }
}

impl File for UnixSocketPairEnd {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read_at(&self, offset: usize, buf: UserBuffer) -> usize {
        self.rx.read_at(offset, buf)
    }

    fn write_at(&self, offset: usize, buf: UserBuffer) -> usize {
        self.tx.write_at(offset, buf)
    }

    fn poll(&self, events: u16) -> u16 {
        let mut ready = 0u16;
        if (events & (POLLIN | POLLHUP)) != 0 {
            ready |= self.rx.poll(events & (POLLIN | POLLHUP));
        }
        if (events & POLLOUT) != 0 {
            ready |= self.tx.poll(events & POLLOUT);
        }
        ready
    }

    fn stat(&self) -> Stat {
        Stat {
            dev: 0,
            ino: self as *const _ as u64,
            mode: StatMode::SOCK,
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