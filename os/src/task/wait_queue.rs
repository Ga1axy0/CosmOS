/// Keyed wait queue supporting wakeup by selected key.
use super::{
    block_current_preblocked_and_run_next, current_task, wakeup_task, TaskControlBlock,
    TaskStatus, WaitReason,
};
use crate::sync::SpinNoIrqLock;
use alloc::{
    collections::{VecDeque},
    sync::Arc,
};
use hashbrown::HashMap;

pub trait NextKey: Default + Eq + core::hash::Hash + Copy {
    fn next(&self) -> Self;
}

impl NextKey for u16 {
    fn next(&self) -> Self { self.wrapping_add(1) }
}

impl NextKey for () {
    fn next(&self) -> Self { *self }
}


/// Simple FIFO wait queue without key-based targeting.
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

    /// Enqueue current task and block with a specific reason.
    pub fn wait_with_reason(&self, reason: WaitReason) {
        let task = current_task().unwrap();
        let mut queue = self.queue.lock();
        {
            let mut task_inner = task.inner_exclusive_access();
            debug_assert!(matches!(task_inner.task_status, TaskStatus::Running));
            task_inner.task_status = TaskStatus::PreBlocked;
            task_inner.wait_reason = Some(reason);
            task_inner.wake_pending = false;
        }
        queue.push_back(task);
        drop(queue);
        block_current_preblocked_and_run_next();
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


/// Keyed wait queue supporting wakeup by selected key.
pub struct WaitQueueKeyed<T>
where
    T: Default + Eq + core::hash::Hash + core::fmt::Display + Copy + NextKey,
{
    /// FIFO order of waiter keys.
    queue: SpinNoIrqLock<VecDeque<T>>,
    /// Mapping from waiter key to waiting task.
    waiters: SpinNoIrqLock<HashMap<T, Arc<TaskControlBlock>>>,
    /// Next auto-generated key used by non-selected waits.
    next_key: SpinNoIrqLock<T>,
}


impl<T> WaitQueueKeyed<T>
where
    T: Default + Eq + core::hash::Hash + core::fmt::Display + Copy + NextKey,
{
    /// Create an empty keyed wait queue.
    pub fn new() -> Self {
        Self {
            queue: SpinNoIrqLock::new(VecDeque::new()),
            waiters: SpinNoIrqLock::new(HashMap::new()),
            next_key: SpinNoIrqLock::new(T::default()),
        }
    }

    /// Enqueue current task and block with a specific reason.
    pub fn wait_with_reason(&self, reason: WaitReason) {
        let task = current_task().unwrap();
        {
            let mut task_inner = task.inner_exclusive_access();
            debug_assert!(matches!(task_inner.task_status, TaskStatus::Running));
            task_inner.task_status = TaskStatus::PreBlocked;
            task_inner.wait_reason = Some(reason);
            task_inner.wake_pending = false;
        }
        let key = {
            let mut next = self.next_key.lock();
            let key = *next;
            *next = (*next).next();
            key
        };
        self.waiters.lock().insert(key, task);
        self.queue.lock().push_back(key);
        block_current_preblocked_and_run_next();
    }

    /// Enqueue current task with a selected key and block with a specific reason.
    pub fn wait_selected_with_reason(&self, key: T, reason: WaitReason) {
        let task = current_task().unwrap();
        {
            let mut task_inner = task.inner_exclusive_access();
            debug_assert!(matches!(task_inner.task_status, TaskStatus::Running));
            task_inner.task_status = TaskStatus::PreBlocked;
            task_inner.wait_reason = Some(reason);
            task_inner.wake_pending = false;
        }
        let replaced = self.waiters.lock().insert(key, task);
        debug_assert!(replaced.is_none(), "duplicate wait key");
        self.queue.lock().push_back(key);
        block_current_preblocked_and_run_next();
    }

    fn pop_next_task_keyed(
        queue: &SpinNoIrqLock<VecDeque<T>>,
        waiters: &SpinNoIrqLock<HashMap<T, Arc<TaskControlBlock>>>,
    ) -> Option<Arc<TaskControlBlock>> {
        loop {
            let key = queue.lock().pop_front()?;
            if let Some(task) = waiters.lock().remove(&key) {
                return Some(task);
            }
        }
    }

    /// Wake one waiter (FIFO order).
    pub fn wake_one(&self) {
        if let Some(task) = Self::pop_next_task_keyed(&self.queue, &self.waiters) {
            wakeup_task(task);
        }
    }

    /// Wake selected waiter by key.
    /// Returns whether a waiter is found and woken.
    pub fn wake_selected(&self, key: T) -> bool {
        if let Some(task) = self.waiters.lock().remove(&key) {
            wakeup_task(task);
            true
        } else {
            false
        }
    }

    /// Wake all waiters.
    pub fn wake_all(&self) {
        while let Some(task) = Self::pop_next_task_keyed(&self.queue, &self.waiters) {
            wakeup_task(task);
        }
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Default for WaitQueueKeyed<T>
where
    T: Default + Eq + core::hash::Hash + core::fmt::Display + Copy + NextKey,
{
    fn default() -> Self {
        Self::new()
    }
}
