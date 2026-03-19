//! Conditian variable

use crate::sync::Mutex;
use crate::task::{WaitQueue, WaitReason};
use alloc::sync::Arc;

/// Condition variable structure
#[deprecated]
pub struct Condvar {
    wait_queue: WaitQueue,
}

impl Condvar {
    /// Create a new condition variable
    pub fn new() -> Self {
        trace!("kernel: Condvar::new");
        Self {
            wait_queue: WaitQueue::new(),
        }
    }

    /// Signal a task waiting on the condition variable
    pub fn signal(&self) {
        self.wait_queue.wake_one();
    }

    /// blocking current task, let it wait on the condition variable
    pub fn wait(&self, mutex: Arc<dyn Mutex>) {
        trace!("kernel: Condvar::wait_with_mutex");
        mutex.unlock();
        self.wait_queue.wait_with_reason(WaitReason::Condvar);
        mutex.lock();
    }

    /// blocking current task, let it wait on the condition variable, without unlocking mutex
    pub fn wait_simple(&self) {
        self.wait_queue.wait_with_reason(WaitReason::Condvar);
    }
}
