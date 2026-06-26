//! Sleepable mutex shim used by filesystem code that can cross block I/O.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::AtomicBool;
#[cfg(not(feature = "kernel_sleep_mutex"))]
use core::sync::atomic::Ordering;

#[cfg(feature = "kernel_sleep_mutex")]
extern "C" {
    fn fs_sleep_mutex_try_lock(key: usize, locked: *const AtomicBool) -> bool;
    fn fs_sleep_mutex_wait(key: usize, locked: *const AtomicBool) -> bool;
    fn fs_sleep_mutex_unlock(key: usize, locked: *const AtomicBool);
}

fn try_lock(key: usize, locked: &AtomicBool) -> bool {
    #[cfg(feature = "kernel_sleep_mutex")]
    unsafe {
        return fs_sleep_mutex_try_lock(key, locked);
    }

    #[cfg(not(feature = "kernel_sleep_mutex"))]
    {
        let _ = key;
        locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }
}

fn wait_for_lock(key: usize, locked: &AtomicBool) -> bool {
    #[cfg(feature = "kernel_sleep_mutex")]
    unsafe {
        return fs_sleep_mutex_wait(key, locked);
    }

    #[cfg(not(feature = "kernel_sleep_mutex"))]
    {
        let _ = key;
        while locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        true
    }
}

fn unlock(key: usize, locked: &AtomicBool) {
    #[cfg(feature = "kernel_sleep_mutex")]
    unsafe {
        fs_sleep_mutex_unlock(key, locked);
    }

    #[cfg(not(feature = "kernel_sleep_mutex"))]
    {
        let _ = key;
        locked.store(false, Ordering::Release);
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
        let key = self as *const Self as usize;
        while !try_lock(key, &self.locked) {
            if wait_for_lock(key, &self.locked) {
                break;
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
        let key = self.lock as *const SleepMutex<T> as usize;
        unlock(key, &self.lock.locked);
    }
}
