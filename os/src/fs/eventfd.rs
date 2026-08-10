//! In-kernel eventfd implementation.
//!
//! An eventfd is a non-seekable counter-backed file.  It is readable only
//! while its counter is non-zero and writable while adding the requested
//! value would not overflow the maximum eventfd counter value.

use super::{File, Stat, StatMode};
use crate::mm::UserBuffer;
use crate::poll::{notify_poll_source, POLLIN, POLLOUT};
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{WaitQueue, WaitReason};
use core::any::Any;

/// Linux reserves the all-ones value as the overflow marker, so userspace can
/// never make the counter larger than `u64::MAX - 1`.
const EVENTFD_COUNTER_MAX: u64 = u64::MAX - 1;

#[derive(Clone, Copy)]
struct EventFdState {
    counter: u64,
    semaphore: bool,
}

/// File object backing one eventfd open file description.
pub(crate) struct EventFdFile {
    state: SpinNoIrqLock<EventFdState>,
    read_wait: WaitQueue,
    write_wait: WaitQueue,
    nonblock: bool,
}

impl EventFdFile {
    pub(crate) fn new(initval: u32, semaphore: bool, nonblock: bool) -> Self {
        Self {
            state: SpinNoIrqLock::new(EventFdState {
                counter: initval as u64,
                semaphore,
            }),
            read_wait: WaitQueue::new(),
            write_wait: WaitQueue::new(),
            nonblock,
        }
    }

    fn source_id(&self) -> usize {
        self as *const Self as usize
    }

    fn copy_value_from_user(buf: UserBuffer) -> Result<u64, ERRNO> {
        // Linux accepts eventfd reads and writes with any count of at least
        // eight bytes, while consuming exactly one u64 value.
        if buf.len() < core::mem::size_of::<u64>() {
            return Err(ERRNO::EINVAL);
        }
        let mut bytes = [0u8; core::mem::size_of::<u64>()];
        for (dst, src) in bytes.iter_mut().zip(buf.into_iter()) {
            *dst = unsafe { *src };
        }
        Ok(u64::from_ne_bytes(bytes))
    }

    fn copy_value_to_user(buf: UserBuffer, value: u64) -> Result<usize, ERRNO> {
        if buf.len() < core::mem::size_of::<u64>() {
            return Err(ERRNO::EINVAL);
        }
        let bytes = value.to_ne_bytes();
        for (src, dst) in bytes.iter().zip(buf.into_iter()) {
            unsafe {
                *dst = *src;
            }
        }
        Ok(bytes.len())
    }

    /// Read one eventfd value, honoring the current open-file-description
    /// `O_NONBLOCK` state supplied by `FileDescription`.
    pub(crate) fn read_with_nonblock(
        &self,
        buf: UserBuffer,
        nonblock: bool,
    ) -> Result<usize, ERRNO> {
        if buf.len() < core::mem::size_of::<u64>() {
            return Err(ERRNO::EINVAL);
        }

        let value = loop {
            let mut state = self.state.lock();
            if state.counter != 0 {
                let value = if state.semaphore { 1 } else { state.counter };
                state.counter -= value;
                let became_empty = state.counter == 0;
                let was_full = state.counter.saturating_add(value) == EVENTFD_COUNTER_MAX;
                drop(state);

                // A read from a full counter can make blocked writers
                // runnable.  POLLOUT was not ready only in that case.
                self.write_wait.wake_all();
                if was_full {
                    notify_poll_source(self.source_id(), POLLOUT);
                }
                // Notify edge-triggered epoll subscribers that the readable
                // state has fallen back to zero.  The epoll layer uses this
                // notification to arm the next 0 -> 1 transition.
                if became_empty {
                    notify_poll_source(self.source_id(), POLLIN);
                }
                break value;
            }
            if nonblock {
                return Err(ERRNO::EAGAIN);
            }
            drop(state);
            self.read_wait
                .wait_with_reason_or_skip(WaitReason::EventFdReadable, || {
                    self.state.lock().counter != 0 || crate::signal::has_unmasked_pending_signal()
                });
            if crate::signal::has_unmasked_pending_signal() {
                return Err(ERRNO::EINTR);
            }
        };

        Self::copy_value_to_user(buf, value)
    }

    /// Add one userspace value to the counter.
    pub(crate) fn write_with_nonblock(
        &self,
        buf: UserBuffer,
        nonblock: bool,
    ) -> Result<usize, ERRNO> {
        let value = Self::copy_value_from_user(buf)?;
        if value == u64::MAX {
            return Err(ERRNO::EINVAL);
        }

        loop {
            let mut state = self.state.lock();
            if state.counter <= EVENTFD_COUNTER_MAX - value {
                let was_empty = state.counter == 0;
                state.counter += value;
                let became_readable = was_empty && state.counter != 0;
                drop(state);

                if became_readable {
                    self.read_wait.wake_all();
                    notify_poll_source(self.source_id(), POLLIN);
                }
                return Ok(core::mem::size_of::<u64>());
            }
            if nonblock {
                return Err(ERRNO::EAGAIN);
            }
            drop(state);
            self.write_wait
                .wait_with_reason_or_skip(WaitReason::EventFdWritable, || {
                    self.state.lock().counter <= EVENTFD_COUNTER_MAX - value
                        || crate::signal::has_unmasked_pending_signal()
                });
            if crate::signal::has_unmasked_pending_signal() {
                return Err(ERRNO::EINTR);
            }
        }
    }
}

impl File for EventFdFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        // This method describes operation permission.  Current readiness is
        // reported by `poll`, otherwise `get_readable_file` would reject an
        // ordinary blocking read while the counter is temporarily empty.
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read_at_result(&self, _offset: usize, buf: UserBuffer) -> Result<usize, ERRNO> {
        self.read_with_nonblock(buf, self.nonblock)
    }

    fn write_at_result(&self, _offset: usize, buf: UserBuffer) -> Result<usize, ERRNO> {
        self.write_with_nonblock(buf, self.nonblock)
    }

    fn poll(&self, events: u16) -> u16 {
        let state = self.state.lock();
        let mut ready = 0;
        if state.counter != 0 && (events & POLLIN) != 0 {
            ready |= POLLIN;
        }
        if state.counter < EVENTFD_COUNTER_MAX && (events & POLLOUT) != 0 {
            ready |= POLLOUT;
        }
        ready
    }

    fn poll_source_id(&self) -> usize {
        self.source_id()
    }

    fn stat(&self) -> Stat {
        Stat {
            dev: 0,
            ino: self.source_id() as u64,
            mode: StatMode::FILE,
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

impl Drop for EventFdFile {
    fn drop(&mut self) {
        self.read_wait.wake_all();
        self.write_wait.wake_all();
        notify_poll_source(self.source_id(), POLLIN | POLLOUT);
    }
}
