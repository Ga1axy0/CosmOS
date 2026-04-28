//! Mutex (spin-like and blocking(sleep))

use super::SpinNoIrqLock;
use crate::sched::suspend_current_and_run_next;
use crate::task::WaitQueue;
use crate::task::WaitReason;

/// Mutex trait
pub trait Mutex: Sync + Send {
    /// Lock the mutex
    fn lock(&self);
    /// Unlock the mutex
    fn unlock(&self);
}

/// Spinlock Mutex struct
pub struct MutexSpin {
    locked: SpinNoIrqLock<bool>,
}

impl MutexSpin {
    /// Create a new spinlock mutex
    pub fn new() -> Self {
        Self {
            locked: SpinNoIrqLock::new(false),
        }
    }
}

impl Mutex for MutexSpin {
    /// Lock the spinlock mutex
    fn lock(&self) {
        trace!("kernel: MutexSpin::lock");
        loop {
            let mut locked = self.locked.lock();
            if *locked {
                drop(locked);
                suspend_current_and_run_next();
                continue;
            } else {
                *locked = true;
                return;
            }
        }
    }

    fn unlock(&self) {
        trace!("kernel: MutexSpin::unlock");
        let mut locked = self.locked.lock();
        *locked = false;
    }
}

/// Blocking Mutex struct
pub struct MutexBlocking {
    inner: SpinNoIrqLock<MutexBlockingInner>,
    wait_queue: WaitQueue,
}

pub struct MutexBlockingInner {
    locked: bool,
}

impl MutexBlocking {
    /// Create a new blocking mutex
    pub fn new() -> Self {
        trace!("kernel: MutexBlocking::new");
        Self {
            inner: SpinNoIrqLock::new(MutexBlockingInner {
                locked: false,
            }),
            wait_queue: WaitQueue::new(),
        }
    }
}

impl Mutex for MutexBlocking {
    /// lock the blocking mutex
    fn lock(&self) {
        trace!("kernel: MutexBlocking::lock");
        loop {
            let mut mutex_inner = self.inner.lock();
            if !mutex_inner.locked {
                mutex_inner.locked = true;
                return;
            }
            drop(mutex_inner);
            self.wait_queue
                .wait_with_reason_or_skip(WaitReason::Mutex, || !self.inner.lock().locked);
        }
    }

    /// unlock the blocking mutex
    fn unlock(&self) {
        trace!("kernel: MutexBlocking::unlock");
        let mut mutex_inner = self.inner.lock();
        assert!(mutex_inner.locked);
        mutex_inner.locked = false;
        drop(mutex_inner);
        self.wait_queue.wake_one();
    }
}
