//! Semaphore

use crate::sync::SpinNoIrqLock;
use crate::task::{WaitQueue, WaitReason};

/// semaphore structure
pub struct Semaphore {
    /// semaphore inner
    pub inner: SpinNoIrqLock<SemaphoreInner>,
    wait_queue: WaitQueue,
}

pub struct SemaphoreInner {
    pub count: isize,
}

impl Semaphore {
    /// Create a new semaphore
    pub fn new(res_count: usize) -> Self {
        trace!("kernel: Semaphore::new");
        Self {
            inner: SpinNoIrqLock::new(SemaphoreInner {
                count: res_count as isize,
            }),
            wait_queue: WaitQueue::new(),
        }
    }

    /// up operation of semaphore
    pub fn up(&self) {
        trace!("kernel: Semaphore::up");
        let mut inner = self.inner.lock();
        inner.count += 1;
        drop(inner);
        self.wait_queue.wake_one();
    }

    /// down operation of semaphore
    pub fn down(&self) {
        trace!("kernel: Semaphore::down");
        loop {
            let mut inner = self.inner.lock();
            if inner.count > 0 {
                inner.count -= 1;
                return;
            }
            drop(inner);
            self.wait_queue
                .wait_with_reason_or_skip(WaitReason::Semaphore, || self.inner.lock().count > 0);
        }
    }
}
