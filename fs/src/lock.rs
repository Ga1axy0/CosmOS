use alloc::collections::BTreeMap;
use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use lazy_static::lazy_static;
use spin::Mutex as SpinMutex;

lazy_static! {
    /// lock_id -> address of `AtomicBool` lock word.
    static ref LOCK_STATE_REGISTRY: SpinMutex<BTreeMap<usize, usize>> =
        SpinMutex::new(BTreeMap::new());
}

#[inline]
fn register_lock_state(lock_id: usize, lock_word: *const AtomicBool) {
    LOCK_STATE_REGISTRY
        .lock()
        .insert(lock_id, lock_word as usize);
}

#[inline]
fn unregister_lock_state(lock_id: usize) {
    LOCK_STATE_REGISTRY.lock().remove(&lock_id);
}

/// Return whether the lock identified by `lock_id` is currently held.
///
/// Returns `false` when the lock is unknown.
pub fn is_lock_held(lock_id: usize) -> bool {
    let ptr = { LOCK_STATE_REGISTRY.lock().get(&lock_id).copied() };
    let Some(ptr) = ptr else {
        return false;
    };
    let lock_word = ptr as *const AtomicBool;
    unsafe { (*lock_word).load(Ordering::Acquire) }
}

/// Called when lock acquisition is contended.
///
/// The hook receives a stable `lock_id` (`usize`) identifying the lock object.
pub type LockWaitHook = fn(lock_id: usize);

/// Called after unlocking, so external waiters can be notified.
///
/// The hook receives the same `lock_id` as [`LockWaitHook`].
pub type LockWakeHook = fn(lock_id: usize);

/// Per-filesystem hook table.
///
/// Each filesystem backend (EasyFS/FAT32/Ext4) should own one table so OS can
/// maintain independent `lock_id -> WaitQueue` registries.
pub struct LockHookTable {
    wait_hook: AtomicUsize,
    wake_hook: AtomicUsize,
}

impl LockHookTable {
    pub const fn new() -> Self {
        Self {
            wait_hook: AtomicUsize::new(0),
            wake_hook: AtomicUsize::new(0),
        }
    }

    pub fn set_hooks(&self, wait: Option<LockWaitHook>, wake: Option<LockWakeHook>) {
        self.wait_hook
            .store(wait.map_or(0, |h| h as usize), Ordering::Release);
        self.wake_hook
            .store(wake.map_or(0, |h| h as usize), Ordering::Release);
    }

    #[inline]
    fn on_wait(&self, lock_id: usize) {
        let wait = self.wait_hook.load(Ordering::Acquire);
        if wait == 0 {
            spin_loop();
        } else {
            let callback: LockWaitHook = unsafe { core::mem::transmute(wait) };
            callback(lock_id);
        }
    }

    #[inline]
    fn on_wake(&self, lock_id: usize) {
        let wake = self.wake_hook.load(Ordering::Acquire);
        if wake == 0 {
            return;
        }
        let callback: LockWakeHook = unsafe { core::mem::transmute(wake) };
        callback(lock_id);
    }
}

/// Hookable mutex that supports external blocking wait/wakeup integration.
pub struct BlockingMutex<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
    hooks: &'static LockHookTable,
}

unsafe impl<T: Send> Send for BlockingMutex<T> {}
unsafe impl<T: Send> Sync for BlockingMutex<T> {}

impl<T> BlockingMutex<T> {
    pub const fn new_with_hooks(value: T, hooks: &'static LockHookTable) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
            hooks,
        }
    }

    #[inline]
    fn lock_id(&self) -> usize {
        self as *const Self as usize
    }

    #[inline]
    fn ensure_registered(&self) {
        register_lock_state(self.lock_id(), &self.locked as *const AtomicBool);
    }

    pub fn lock(&self) -> BlockingMutexGuard<'_, T> {
        self.ensure_registered();
        let lock_id = self.lock_id();
        loop {
            if self
                .locked
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return BlockingMutexGuard { mutex: self };
            }
            self.hooks.on_wait(lock_id);
        }
    }

    pub fn try_lock(&self) -> Option<BlockingMutexGuard<'_, T>> {
        self.ensure_registered();
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| BlockingMutexGuard { mutex: self })
    }
}

impl<T> Drop for BlockingMutex<T> {
    fn drop(&mut self) {
        unregister_lock_state(self.lock_id());
    }
}

pub struct BlockingMutexGuard<'a, T> {
    mutex: &'a BlockingMutex<T>,
}

impl<T> Deref for BlockingMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T> DerefMut for BlockingMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T> Drop for BlockingMutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.locked.store(false, Ordering::Release);
        self.mutex.hooks.on_wake(self.mutex.lock_id());
    }
}
