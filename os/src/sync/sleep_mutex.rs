//! Sleepable mutex for kernel data that may be held across blocking paths.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::task::{
    check_fatal_signals_of_current, current_task, exit_current_and_run_next, ExitReason, WaitQueue,
    WaitReason,
};

/// A small sleepable mutex.
///
/// Unlike [`crate::sync::SpinNoIrqLock`], holding this lock does not disable
/// local interrupts, so it may protect state across filesystem or block I/O.
pub struct SleepMutex<T> {
    locked: AtomicBool,
    wait_queue: WaitQueue,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for SleepMutex<T> {}
unsafe impl<T: Send> Sync for SleepMutex<T> {}

impl<T> SleepMutex<T> {
    /// Create a new unlocked mutex.
    pub fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            wait_queue: WaitQueue::new(),
            data: UnsafeCell::new(value),
        }
    }

    /// Acquire the mutex, blocking the current task while contended.
    pub fn lock(&self) -> SleepMutexGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            assert!(
                current_task().is_some(),
                "SleepMutex::lock attempted to sleep without a current task"
            );
            self.wait_queue
                .wait_with_reason_or_skip(WaitReason::Mutex, || {
                    !self.locked.load(Ordering::Acquire)
                });
            if self.locked.load(Ordering::Acquire) {
                if let Some((signum, _)) = check_fatal_signals_of_current() {
                    exit_current_and_run_next(ExitReason::Signal(signum as u32));
                }
            }
        }
        SleepMutexGuard { lock: self }
    }
}

/// Guard returned by [`SleepMutex::lock`].
pub struct SleepMutexGuard<'a, T> {
    lock: &'a SleepMutex<T>,
}

impl<T> Deref for SleepMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SleepMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SleepMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        self.lock.wait_queue.wake_one();
    }
}
