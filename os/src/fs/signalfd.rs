//! In-kernel signalfd implementation.
//!
//! A signalfd is a read-only, non-seekable file whose readable state is the
//! intersection of the fd's signal mask, the current task's blocked mask, and
//! the task/process pending-signal queues.

use super::{File, Stat, StatMode};
use crate::mm::UserBuffer;
use crate::poll::{notify_poll_source, POLLIN};
use crate::signal::{SigInfo, SignalBit};
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{WaitQueue, WaitReason};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::mem::size_of;
use lazy_static::lazy_static;

/// Linux's fixed-size signalfd read record.
#[repr(C)]
#[derive(Clone, Copy)]
struct SignalFdSigInfo {
    ssi_signo: u32,
    ssi_errno: i32,
    ssi_code: i32,
    ssi_pid: u32,
    ssi_uid: u32,
    ssi_fd: i32,
    ssi_tid: u32,
    ssi_band: u32,
    ssi_overrun: u32,
    ssi_trapno: u32,
    ssi_status: i32,
    ssi_int: i32,
    ssi_ptr: u64,
    ssi_utime: u64,
    ssi_stime: u64,
    ssi_addr: u64,
    ssi_addr_lsb: u16,
    _pad: [u8; 46],
}

const _: [(); 128] = [(); size_of::<SignalFdSigInfo>()];

lazy_static! {
    /// Weak registry used only to wake blocking reads and poll/epoll waiters.
    /// The pending signal itself remains in the task/process queues, so a
    /// spurious wake is harmless and also makes fd inheritance work correctly.
    static ref SIGNAL_FD_REGISTRY: SpinNoIrqLock<Vec<Weak<SignalFdFile>>> =
        SpinNoIrqLock::new(Vec::new());
}

/// File object backing one signalfd open file description.
pub(crate) struct SignalFdFile {
    signal_mask: SpinNoIrqLock<SignalBit>,
    read_wait: WaitQueue,
}

impl SignalFdFile {
    pub(crate) fn new(signal_mask: SignalBit) -> Arc<Self> {
        let file = Arc::new(Self {
            signal_mask: SpinNoIrqLock::new(signal_mask),
            read_wait: WaitQueue::new(),
        });
        SIGNAL_FD_REGISTRY.lock().push(Arc::downgrade(&file));
        file
    }

    fn source_id(&self) -> usize {
        self as *const Self as usize
    }

    fn mask(&self) -> SignalBit {
        *self.signal_mask.lock()
    }

    pub(crate) fn set_mask(&self, signal_mask: SignalBit) {
        *self.signal_mask.lock() = signal_mask;
        // A mask update can make an already-pending signal readable.
        self.read_wait.wake_all();
        notify_poll_source(self.source_id(), POLLIN);
    }

    fn has_pending(&self) -> bool {
        crate::signal::has_signalfd_pending_in_set(self.mask())
    }

    fn encode_siginfo(siginfo: &SigInfo) -> SignalFdSigInfo {
        SignalFdSigInfo {
            ssi_signo: siginfo.si_signo as u32,
            ssi_errno: siginfo.si_errno,
            ssi_code: siginfo.si_code,
            ssi_pid: siginfo.si_pid as u32,
            ssi_uid: siginfo.si_uid,
            ssi_fd: 0,
            ssi_tid: 0,
            ssi_band: 0,
            ssi_overrun: 0,
            ssi_trapno: 0,
            ssi_status: 0,
            ssi_int: 0,
            ssi_ptr: 0,
            ssi_utime: 0,
            ssi_stime: 0,
            ssi_addr: 0,
            ssi_addr_lsb: 0,
            _pad: [0; 46],
        }
    }

    fn copy_siginfo_to_user(
        output: &mut crate::mm::UserBufferIterator,
        siginfo: &SigInfo,
    ) -> Result<(), ERRNO> {
        let record = Self::encode_siginfo(siginfo);
        let bytes = unsafe {
            core::slice::from_raw_parts(
                (&record as *const SignalFdSigInfo).cast::<u8>(),
                size_of::<SignalFdSigInfo>(),
            )
        };
        for byte in bytes {
            let dst = output.next().ok_or(ERRNO::EFAULT)?;
            unsafe {
                *dst = *byte;
            }
        }
        Ok(())
    }

    /// Read one or more pending signals, honoring the current open-file
    /// description's `O_NONBLOCK` state.
    pub(crate) fn read_with_nonblock(
        &self,
        buf: UserBuffer,
        nonblock: bool,
    ) -> Result<usize, ERRNO> {
        let record_size = size_of::<SignalFdSigInfo>();
        if buf.len() < record_size {
            return Err(ERRNO::EINVAL);
        }

        let max_records = buf.len() / record_size;
        let mut output = buf.into_iter();
        let mut records = 0usize;

        loop {
            if records == max_records {
                return Ok(records * record_size);
            }

            if let Some((_signum, siginfo)) =
                crate::signal::take_signalfd_signal_in_set(self.mask())
            {
                Self::copy_siginfo_to_user(&mut output, &siginfo)?;
                records += 1;
                continue;
            }

            // A read that returned at least one record must not wait for the
            // caller's entire buffer to fill.
            if records != 0 {
                return Ok(records * record_size);
            }
            if nonblock {
                return Err(ERRNO::EAGAIN);
            }

            self.read_wait
                .wait_with_reason_or_skip(WaitReason::Poll, || {
                    self.has_pending() || crate::signal::has_unmasked_pending_signal()
                });
            if crate::signal::has_unmasked_pending_signal() {
                return Err(ERRNO::EINTR);
            }
        }
    }
}

impl File for SignalFdFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        false
    }

    fn read_at_result(&self, _offset: usize, _buf: UserBuffer) -> Result<usize, ERRNO> {
        Err(ERRNO::ESPIPE)
    }

    fn poll(&self, events: u16) -> u16 {
        if (events & POLLIN) != 0 && self.has_pending() {
            POLLIN
        } else {
            0
        }
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

impl Drop for SignalFdFile {
    fn drop(&mut self) {
        self.read_wait.wake_all();
        notify_poll_source(self.source_id(), POLLIN);
    }
}

/// Wake signalfd readers and poll/epoll subscribers after a signal is queued.
///
/// The registry deliberately does not filter by process.  A signalfd is an
/// open file description and may be inherited across fork; the subsequent
/// read checks the current task's queues and mask.  Unrelated fds can receive a
/// harmless spurious wake, while inherited fds cannot miss a notification.
pub(crate) fn notify_signal_fd(pending_bits: u64) {
    if pending_bits == 0 {
        return;
    }

    let files = {
        let mut registry = SIGNAL_FD_REGISTRY.lock();
        let mut files = Vec::new();
        registry.retain(|weak| {
            let Some(file) = weak.upgrade() else {
                return false;
            };
            if (file.mask().bits() & pending_bits) != 0 {
                files.push(file);
            }
            true
        });
        files
    };

    for file in files {
        file.read_wait.wake_all();
        notify_poll_source(file.source_id(), POLLIN);
    }
}
