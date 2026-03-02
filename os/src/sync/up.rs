//! Safe Cell for uniprocessor（single cpu core）
//!
//! UPSafeCell is used to wrap a static data structure which can access safely.
//!
//! NOTICE: We should only use it in environment with uniprocessor（single cpu core）, and the kernel can not support task preempting in kernel mode （or trap in kernel mode）.

use core::cell::{RefCell, RefMut, UnsafeCell};

use riscv::register::sstatus;

/// Wrap a static data structure inside it so that we are
/// able to access it without any `unsafe`.
///
/// We should only use it in uniprocessor.
///
/// In order to get mutable reference of inner data, call
/// `exclusive_access`.
pub struct UPSafeCell<T> {
    /// inner data
    inner: RefCell<T>,
}

unsafe impl<T> Sync for UPSafeCell<T> {}

impl<T> UPSafeCell<T> {
    /// User is responsible to guarantee that inner struct is only used in
    /// uniprocessor.
    pub unsafe fn new(value: T) -> Self {
        Self {
            inner: RefCell::new(value),
        }
    }
    /// Panic if the data has been borrowed.
    pub fn exclusive_access(&self) -> RefMut<'_, T> {
        self.inner.borrow_mut()
    }
}


/// A uniprocessor cell that provides exclusive access by temporarily disabling
/// supervisor interrupts.
///
/// This is useful for sharing data between normal context and interrupt/trap
/// handlers without introducing a blocking lock.
#[derive(Debug)]
pub struct UPIntrFreeCell<T> {
    inner: UnsafeCell<T>,
}

impl<T> UPIntrFreeCell<T> {
    /// # Safety
    /// The caller must ensure the contained value is only accessed via
    /// `exclusive_access`.
    pub unsafe fn new(value: T) -> Self {
        Self {
            inner: UnsafeCell::new(value),
        }
    }

    pub fn exclusive_access(&self) -> UPIntrFreeCellRefMut<'_, T> {
        UPIntrFreeCellRefMut {
            cell: self,
            _guard: IntrGuard::new(),
        }
    }
}

unsafe impl<T> Sync for UPIntrFreeCell<T> {}

pub struct UPIntrFreeCellRefMut<'a, T> {
    cell: &'a UPIntrFreeCell<T>,
    _guard: IntrGuard,
}

impl<'a, T> core::ops::Deref for UPIntrFreeCellRefMut<'a, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.cell.inner.get() }
    }
}

impl<'a, T> core::ops::DerefMut for UPIntrFreeCellRefMut<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.cell.inner.get() }
    }
}

struct IntrGuard {
    sie_was_enabled: bool,
}

impl IntrGuard {
    fn new() -> Self {
        let sie_was_enabled = sstatus::read().sie();
        unsafe { sstatus::clear_sie() };
        Self { sie_was_enabled }
    }
}

impl Drop for IntrGuard {
    fn drop(&mut self) {
        if self.sie_was_enabled {
            unsafe { sstatus::set_sie() };
        }
    }
}