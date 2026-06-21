//! Wait/wake hooks for the filesystem crate's sleepable mutex shim.

use crate::sync::SpinNoIrqLock;
use crate::task::{WaitQueue, WaitReason};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use lazy_static::lazy_static;

lazy_static! {
    static ref FS_MUTEX_WAIT_QUEUES: SpinNoIrqLock<BTreeMap<usize, Arc<WaitQueue>>> =
        SpinNoIrqLock::new(BTreeMap::new());
}

fn wait_queue_for(key: usize) -> Arc<WaitQueue> {
    if let Some(queue) = FS_MUTEX_WAIT_QUEUES.lock().get(&key).cloned() {
        return queue;
    }
    let queue = Arc::new(WaitQueue::new());
    let mut queues = FS_MUTEX_WAIT_QUEUES.lock();
    queues
        .entry(key)
        .or_insert_with(|| Arc::clone(&queue))
        .clone()
}

/// Sleep until a filesystem mutex becomes unlocked.
#[no_mangle]
pub extern "C" fn fs_sleep_mutex_wait(key: usize, locked: *const AtomicBool) {
    crate::trap::assert_can_sleep("fs_sleep_mutex_wait");
    let is_unlocked = || unsafe { !(*locked).load(Ordering::Acquire) };
    if is_unlocked() {
        return;
    }
    wait_queue_for(key).wait_with_reason_or_skip(WaitReason::Mutex, is_unlocked);
}

/// Wake one waiter for a filesystem mutex.
#[no_mangle]
pub extern "C" fn fs_sleep_mutex_wake(key: usize) {
    if let Some(queue) = FS_MUTEX_WAIT_QUEUES.lock().get(&key).cloned() {
        queue.wake_one();
    }
}
