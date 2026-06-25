//! Safe Cell for uniprocessor（single cpu core）
//!
//! UPSafeCell is used to wrap a static data structure which can access safely.
//!
//! NOTICE: UPSafeCell now uses SpinNoIrqLock internally so it is
//! safe on multi-core as well.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// Wrap a static data structure inside a spinlock with interrupt masking.
///
/// `exclusive_access()` returns a guard that holds the lock and disables
/// supervisor interrupts.  This replaces the old `RefCell`-based
/// implementation and is safe on SMP.
pub struct UPSafeCell<T> {
    locked: AtomicBool,
    inner: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for UPSafeCell<T> {}
unsafe impl<T: Send> Sync for UPSafeCell<T> {}

impl<T> UPSafeCell<T> {
    /// Create a new `UPSafeCell`.
    ///
    /// # Safety
    /// In the new SMP-aware implementation this is actually safe, but we keep
    /// the `unsafe` signature for source compatibility with existing callers.
    pub unsafe fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            inner: UnsafeCell::new(value),
        }
    }

    /// Obtain exclusive (mutable) access to the inner data.
    ///
    /// Disables supervisor interrupts and spins until the lock is acquired.
    pub fn exclusive_access(&self) -> UPSafeCellGuard<'_, T> {
        let sie_was_enabled = crate::hal::local_irqs_enabled();
        unsafe { crate::hal::disable_local_irqs() };

        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            if sie_was_enabled {
                unsafe { crate::hal::enable_local_irqs() };
            }
            core::hint::spin_loop();
            unsafe { crate::hal::disable_local_irqs() };
        }

        UPSafeCellGuard {
            cell: self,
            sie_was_enabled,
        }
    }
}

/// RAII guard for [`UPSafeCell`].
pub struct UPSafeCellGuard<'a, T> {
    cell: &'a UPSafeCell<T>,
    sie_was_enabled: bool,
}

impl<T> Deref for UPSafeCellGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.cell.inner.get() }
    }
}

impl<T> DerefMut for UPSafeCellGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.cell.inner.get() }
    }
}

impl<T> Drop for UPSafeCellGuard<'_, T> {
    fn drop(&mut self) {
        self.cell.locked.store(false, Ordering::Release);
        if self.sie_was_enabled {
            unsafe { crate::hal::enable_local_irqs() };
        }
    }
}

/// A multicore-safe cell that provides exclusive access by disabling
/// supervisor interrupts and spinning.
///
/// Previously single-core only; now uses an atomic spinlock internally
/// so it is safe under SMP.
#[derive(Debug)]
pub struct UPIntrFreeCell<T> {
    locked: AtomicBool,
    inner: UnsafeCell<T>,
}

impl<T> UPIntrFreeCell<T> {
    /// # Safety
    /// Kept `unsafe` for source compatibility.
    pub unsafe fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            inner: UnsafeCell::new(value),
        }
    }

    /// Get exclusive access with interrupts disabled + spinlock.
    pub fn exclusive_access(&self) -> UPIntrFreeCellRefMut<'_, T> {
        let sie_was_enabled = crate::hal::local_irqs_enabled();
        unsafe { crate::hal::disable_local_irqs() };

        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            if sie_was_enabled {
                unsafe { crate::hal::enable_local_irqs() };
            }
            core::hint::spin_loop();
            unsafe { crate::hal::disable_local_irqs() };
        }

        UPIntrFreeCellRefMut {
            cell: self,
            sie_was_enabled,
        }
    }
}

unsafe impl<T: Send> Send for UPIntrFreeCell<T> {}
unsafe impl<T: Send> Sync for UPIntrFreeCell<T> {}

pub struct UPIntrFreeCellRefMut<'a, T> {
    cell: &'a UPIntrFreeCell<T>,
    sie_was_enabled: bool,
}

impl<T> Deref for UPIntrFreeCellRefMut<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.cell.inner.get() }
    }
}

impl<T> DerefMut for UPIntrFreeCellRefMut<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.cell.inner.get() }
    }
}

impl<T> Drop for UPIntrFreeCellRefMut<'_, T> {
    fn drop(&mut self) {
        self.cell.locked.store(false, Ordering::Release);
        if self.sie_was_enabled {
            unsafe { crate::hal::enable_local_irqs() };
        }
    }
}
