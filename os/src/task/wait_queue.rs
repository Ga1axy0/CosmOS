/// Keyed wait queue supporting wakeup by selected key.
use super::{
    block_current_and_run_next, current_task, wakeup_task, TaskControlBlock, TaskStatus,
    WaitReason,
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
        self.wait_with_reason_or_skip(reason, || false);
    }

    /// Enqueue current task and block, unless `should_skip` reports
    /// the awaited condition is already satisfied after enqueue.
    pub fn wait_with_reason_or_skip<F>(&self, reason: WaitReason, should_skip: F)
    where
        F: FnOnce() -> bool,
    {
        let task = current_task().unwrap();
        let mut queue = self.queue.lock();
        {
            let mut task_inner = task.inner_exclusive_access();
            debug_assert!(matches!(task_inner.task_status, TaskStatus::Running));
            task_inner.task_status = TaskStatus::Interruptible;
            task_inner.wait_reason = Some(reason);
        }
        queue.push_back(task.clone());
        drop(queue);

        // Re-check condition after enqueueing ourselves. If already ready,
        // cancel the sleep transition and keep running on this hart.
        if should_skip() {
            self.queue
                .lock()
                .retain(|queued| Arc::as_ptr(queued) != Arc::as_ptr(&task));
            if let Some(task) = current_task() {
                let mut task_inner = task.inner_exclusive_access();
                if matches!(task_inner.task_status, TaskStatus::Interruptible) {
                    task_inner.task_status = TaskStatus::Running;
                    task_inner.wait_reason = None;
                }
            }
            return;
        }

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
            task_inner.task_status = TaskStatus::Interruptible;
            task_inner.wait_reason = Some(reason);
        }
        let key = {
            let mut next = self.next_key.lock();
            let key = *next;
            *next = (*next).next();
            key
        };
        self.waiters.lock().insert(key, task);
        self.queue.lock().push_back(key);
        block_current_and_run_next(reason);
    }

    /// Enqueue current task with a selected key and block with a specific reason.
    pub fn wait_selected_with_reason(&self, key: T, reason: WaitReason) {
        self.wait_selected_with_reason_or_skip(key, reason, || false);
    }

    /// Enqueue current task with a selected key and block, unless `should_skip`
    /// reports the awaited condition is already satisfied after enqueue.
    ///
    /// This closes the common lost-wakeup window:
    ///
    /// 1) waiter checks condition (not ready)
    /// 2) waker signals before waiter is fully asleep
    /// 3) waiter sleeps forever
    pub fn wait_selected_with_reason_or_skip<F>(&self, key: T, reason: WaitReason, should_skip: F)
    where
        F: FnOnce() -> bool,
    {
        let task = current_task().unwrap();
        {
            let mut task_inner = task.inner_exclusive_access();
            debug_assert!(matches!(task_inner.task_status, TaskStatus::Running));
            task_inner.task_status = TaskStatus::Interruptible;
            task_inner.wait_reason = Some(reason);
        }
        let replaced = self.waiters.lock().insert(key, task);
        assert!(replaced.is_none(), "duplicate wait key");
        self.queue.lock().push_back(key);

        // Re-check condition after enqueueing ourselves. If already ready,
        // cancel the sleep transition and keep running on this hart.
        if should_skip() {
            self.waiters.lock().remove(&key);
            if let Some(task) = current_task() {
                let mut task_inner = task.inner_exclusive_access();
                if matches!(task_inner.task_status, TaskStatus::Interruptible) {
                    task_inner.task_status = TaskStatus::Running;
                    task_inner.wait_reason = None;
                }
            }
            return;
        }

        block_current_and_run_next(reason);
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
