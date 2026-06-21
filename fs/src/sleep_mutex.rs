//! Sleepable mutex shim used by filesystem code that can cross block I/O.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "kernel_sleep_mutex")]
extern "C" {
    fn fs_sleep_mutex_wait(key: usize, locked: *const AtomicBool);
    fn fs_sleep_mutex_wake(key: usize);
}

fn wait_for_unlock(key: usize, locked: &AtomicBool) {
    #[cfg(feature = "kernel_sleep_mutex")]
    unsafe {
        fs_sleep_mutex_wait(key, locked);
    }

    #[cfg(not(feature = "kernel_sleep_mutex"))]
    {
        let _ = key;
        while locked.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
    }
}

fn wake_waiters(key: usize) {
    #[cfg(feature = "kernel_sleep_mutex")]
    unsafe {
        fs_sleep_mutex_wake(key);
    }

    #[cfg(not(feature = "kernel_sleep_mutex"))]
    {
        let _ = key;
    }
}

/// A mutex that sleeps through OS-provided hooks when contended.
pub struct SleepMutex<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for SleepMutex<T> {}
unsafe impl<T: Send> Sync for SleepMutex<T> {}

impl<T> SleepMutex<T> {
    /// Create a new unlocked mutex.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        }
    }

    /// Lock the mutex, sleeping through the OS hook if it is contended.
    pub fn lock(&self) -> SleepMutexGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            wait_for_unlock(self as *const Self as usize, &self.locked);
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
        wake_waiters(self.lock as *const SleepMutex<T> as usize);
    }
}
