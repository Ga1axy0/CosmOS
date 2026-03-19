//! Unified blocking/wakeup wait queue for kernel wait events.

use super::{
    block_current_and_run_next, current_task, wakeup_task,
    TaskControlBlock, WaitReason,
};
use crate::sync::SpinNoIrqLock;
use alloc::{collections::VecDeque, sync::Arc};

/// Generic wait queue used by kernel subsystems (process wait, device wait, etc.).
pub struct WaitQueue {
    queue: SpinNoIrqLock<VecDeque<Arc<TaskControlBlock>>>,
}

impl WaitQueue {
    /// Create an empty wait queue.
    pub fn new() -> Self {
        Self {
            queue: SpinNoIrqLock::new(VecDeque::new()),
        }
    }

    /// Enqueue current task and block until wakeup.
    pub fn wait(&self) {
        self.wait_with_reason(WaitReason::Unknown);
    }

    /// Enqueue current task and block with a specific reason.
    pub fn wait_with_reason(&self, reason: WaitReason) {
        let mut queue = self.queue.lock();
        queue.push_back(current_task().unwrap());
        drop(queue);
        block_current_and_run_next(reason);
    }

    /// Wake one waiter (FIFO order).
    pub fn wake_one(&self) {
        let mut queue = self.queue.lock();
        if let Some(task) = queue.pop_front() {
            wakeup_task(task);
        }
    }

    /// Wake all waiters.
    pub fn wake_all(&self) {
        let mut queue = self.queue.lock();
        while let Some(task) = queue.pop_front() {
            wakeup_task(task);
        }
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}