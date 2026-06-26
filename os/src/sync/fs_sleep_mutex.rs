//! Wait/wake hooks for the filesystem crate's sleepable mutex shim.

use crate::sync::SpinNoIrqLock;
use crate::task::{
    check_fatal_signals_of_current, current_task, exit_current_and_run_next, ExitReason,
    wakeup_task, TaskControlBlock, WaitQueue, WaitReason,
};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use lazy_static::lazy_static;

struct FsSleepMutexState {
    wait_queue: WaitQueue,
    inner: SpinNoIrqLock<FsSleepMutexInner>,
}

struct FsSleepMutexInner {
    handoff_task: Option<usize>,
}

impl FsSleepMutexState {
    fn new() -> Self {
        Self {
            wait_queue: WaitQueue::new(),
            inner: SpinNoIrqLock::new(FsSleepMutexInner { handoff_task: None }),
        }
    }
}

lazy_static! {
    static ref FS_MUTEX_STATES: SpinNoIrqLock<BTreeMap<usize, Arc<FsSleepMutexState>>> =
        SpinNoIrqLock::new(BTreeMap::new());
}

fn state_for(key: usize) -> Arc<FsSleepMutexState> {
    if let Some(state) = FS_MUTEX_STATES.lock().get(&key).cloned() {
        return state;
    }
    let state = Arc::new(FsSleepMutexState::new());
    let mut states = FS_MUTEX_STATES.lock();
    states
        .entry(key)
        .or_insert_with(|| Arc::clone(&state))
        .clone()
}

fn state_if_present(key: usize) -> Option<Arc<FsSleepMutexState>> {
    FS_MUTEX_STATES.lock().get(&key).cloned()
}

fn task_handoff_token(task: &Arc<TaskControlBlock>) -> usize {
    Arc::as_ptr(task) as usize
}

fn current_handoff_token() -> Option<usize> {
    current_task().map(|task| task_handoff_token(&task))
}

fn try_lock_atomic(locked: &AtomicBool) -> bool {
    locked
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_ok()
}

fn try_lock_state_locked(
    inner: &mut FsSleepMutexInner,
    current: Option<usize>,
    locked: &AtomicBool,
) -> bool {
    if let Some(target) = inner.handoff_task {
        if Some(target) != current {
            return false;
        }
    }
    if try_lock_atomic(locked) {
        if inner.handoff_task.is_some() {
            inner.handoff_task = None;
        }
        return true;
    }
    false
}

fn try_lock_state(state: &FsSleepMutexState, locked: &AtomicBool) -> bool {
    let current = current_handoff_token();
    let mut inner = state.inner.lock();
    try_lock_state_locked(&mut inner, current, locked)
}

/// Try to acquire a filesystem mutex while respecting an active waiter handoff.
#[no_mangle]
pub extern "C" fn fs_sleep_mutex_try_lock(key: usize, locked: *const AtomicBool) -> bool {
    let locked = unsafe { &*locked };
    if let Some(state) = state_if_present(key) {
        try_lock_state(&state, locked)
    } else {
        try_lock_atomic(locked)
    }
}

/// Sleep until a filesystem mutex becomes available and acquire it when possible.
#[no_mangle]
pub extern "C" fn fs_sleep_mutex_wait(key: usize, locked: *const AtomicBool) -> bool {
    let state = state_for(key);
    let locked = unsafe { &*locked };
    loop {
        let task = {
            let current = current_handoff_token();
            let mut inner = state.inner.lock();
            if try_lock_state_locked(&mut inner, current, locked) {
                return true;
            }

            let task = state.wait_queue.prepare_to_wait(WaitReason::Mutex);
            if try_lock_state_locked(&mut inner, current, locked) {
                state.wait_queue.cancel_prepared_wait(&task);
                return true;
            }
            task
        };

        state.wait_queue.block_prepared(task, WaitReason::Mutex);
        if try_lock_state(&state, locked) {
            return true;
        }
        if locked.load(Ordering::Acquire) {
            if let Some((signum, _)) = check_fatal_signals_of_current() {
                exit_current_and_run_next(ExitReason::Signal(signum as u32));
            }
        }
        return false;
    }
}

/// Release a filesystem mutex and hand it to the next queued waiter when present.
#[no_mangle]
pub extern "C" fn fs_sleep_mutex_unlock(key: usize, locked: *const AtomicBool) {
    let locked = unsafe { &*locked };
    let Some(state) = state_if_present(key) else {
        locked.store(false, Ordering::Release);
        return;
    };

    let wake_task = {
        let mut inner = state.inner.lock();
        if let Some(task) = state.wait_queue.take_one_waiter() {
            inner.handoff_task = Some(task_handoff_token(&task));
            locked.store(false, Ordering::Release);
            Some(task)
        } else {
            inner.handoff_task = None;
            locked.store(false, Ordering::Release);
            None
        }
    };
    if let Some(task) = wake_task {
        wakeup_task(task);
    }
}
