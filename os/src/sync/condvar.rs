//! Conditian variable

use crate::sync::{Mutex, SpinNoIrqLock};
use crate::task::{block_current_and_run_next, current_task, wakeup_task, TaskControlBlock};
use alloc::{collections::VecDeque, sync::Arc};

/// Condition variable structure
pub struct Condvar {
    /// Condition variable inner
    pub inner: SpinNoIrqLock<CondvarInner>,
}

pub struct CondvarInner {
    pub wait_queue: VecDeque<Arc<TaskControlBlock>>,
}

impl Condvar {
    /// Create a new condition variable
    pub fn new() -> Self {
        trace!("kernel: Condvar::new");
        Self {
            inner: SpinNoIrqLock::new(CondvarInner {
                wait_queue: VecDeque::new(),
            }),
        }
    }

    /// Signal a task waiting on the condition variable
    pub fn signal(&self) {
        let mut inner = self.inner.lock();
        if let Some(task) = inner.wait_queue.pop_front() {
            wakeup_task(task);
        }
    }

    /// blocking current task, let it wait on the condition variable
    pub fn wait(&self, mutex: Arc<dyn Mutex>) {
        trace!("kernel: Condvar::wait_with_mutex");
        mutex.unlock();
        let mut inner = self.inner.lock();
        inner.wait_queue.push_back(current_task().unwrap());
        drop(inner);
        block_current_and_run_next();
        mutex.lock();
    }

    /// blocking current task, let it wait on the condition variable, without unlocking mutex
    pub fn wait_simple(&self) {
        let mut inner = self.inner.lock();
        inner.wait_queue.push_back(current_task().unwrap());
        drop(inner);
        block_current_and_run_next();
    }
}
