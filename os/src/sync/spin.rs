//! Kernel spin locks with optional interrupt masking.
//!
//! Two flavours are provided:
//!
//! * [`SpinLock<T>`] – plain test-and-set spinlock.  Does **not** touch the
//!   interrupt-enable bits; the caller is responsible for managing them if
//!   needed.
//!
//! * [`SpinNoIrqLock<T>`] – like `SpinLock`, but automatically **disables
//!   supervisor interrupts** (`sstatus.SIE`) while the lock is held and
//!   restores the previous state on unlock.  This is the equivalent of
//!   Linux's `spin_lock_irqsave` / `spin_unlock_irqrestore`.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// SpinLock – plain spinlock (no interrupt masking)
// ---------------------------------------------------------------------------

/// A mutual-exclusion primitive based on busy-waiting (spinning).
pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for SpinLock<T> {}
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Create a new, unlocked `SpinLock`.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        }
    }

    /// Acquire the lock, spinning until it is available.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Hint to the processor that we are in a spin loop.
            core::hint::spin_loop();
        }
        SpinLockGuard { lock: self }
    }

    /// Get mutable access without locking.
    ///
    /// # Safety
    /// Caller must guarantee exclusive access.
    #[allow(dead_code)]
    pub unsafe fn get_mut(&self) -> &mut T {
        &mut *self.data.get()
    }
}

/// RAII guard for [`SpinLock`].
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// SpinNoIrqLock – spinlock with interrupt save/restore
// ---------------------------------------------------------------------------

/// A spinlock that **disables supervisor interrupts** while held.
///
/// This prevents the classic deadlock scenario where:
/// 1. Hart 0 acquires the lock.
/// 2. A timer interrupt fires on the same hart.
/// 3. The interrupt handler tries to acquire the same lock → deadlock.
///
/// Semantically equivalent to Linux's `spin_lock_irqsave`.
pub struct SpinNoIrqLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for SpinNoIrqLock<T> {}
unsafe impl<T: Send> Sync for SpinNoIrqLock<T> {}

impl<T> SpinNoIrqLock<T> {
    /// Create a new, unlocked `SpinNoIrqLock`.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        }
    }

    /// Acquire the lock with interrupts disabled.
    pub fn lock(&self) -> SpinNoIrqLockGuard<'_, T> {
        // Save the current SIE bit state and then disable.
        let sie_was_enabled = crate::hal::local_irqs_enabled();
        unsafe { crate::hal::disable_local_irqs() };

        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Keep interrupts disabled while spinning to preserve irqsave
            // semantics and avoid deadlocks with interrupt handlers that
            // might take the same or nested locks.
            core::hint::spin_loop();
        }
        crate::trap::enter_noirq_lock();

        SpinNoIrqLockGuard {
            lock: self,
            sie_was_enabled,
        }
    }

    /// Get mutable access without locking.
    ///
    /// # Safety
    /// Caller must guarantee exclusive access.
    #[allow(dead_code)]
    pub unsafe fn get_mut(&self) -> &mut T {
        &mut *self.data.get()
    }
}

/// RAII guard for [`SpinNoIrqLock`].  Restores the saved `sstatus.SIE`
/// state when dropped.
pub struct SpinNoIrqLockGuard<'a, T> {
    lock: &'a SpinNoIrqLock<T>,
    sie_was_enabled: bool,
}

impl<T> Deref for SpinNoIrqLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinNoIrqLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinNoIrqLockGuard<'_, T> {
    fn drop(&mut self) {
        crate::trap::exit_noirq_lock();
        self.lock.locked.store(false, Ordering::Release);
        if self.sie_was_enabled {
            unsafe { crate::hal::enable_local_irqs() };
        }
    }
}
