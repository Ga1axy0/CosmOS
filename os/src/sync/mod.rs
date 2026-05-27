//! Synchronization and interior mutability primitives

mod condvar;
mod mutex;
mod semaphore;
mod spin;
mod up;
mod deadlock;
mod futex;

pub use condvar::Condvar;
pub use deadlock::DeadlockDetector;
pub use mutex::{Mutex, MutexBlocking, MutexSpin};
pub use semaphore::Semaphore;
pub use spin::{SpinLock, SpinLockGuard, SpinNoIrqLock, SpinNoIrqLockGuard};
pub use up::{UPSafeCell, UPSafeCellGuard, UPIntrFreeCell};
pub use futex::{
    cleanup_futex_wait_for_task, futex_wake_addr, handle_futex_wait_timeout, FutexTimerTag, futex_queue, futex_wait_mark_ready, futex_wait_addr
};