/// Keyed wait queue supporting wakeup by selected key.
use super::{current_task, wakeup_task, TaskControlBlock, TaskStatus, WaitReason};
use crate::sched::block_current_and_run_next;
use crate::sync::SpinNoIrqLock;
use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use hashbrown::HashMap;

/// Type-erased handle used by signal delivery to properly wake a task
/// from whichever WaitQueue (or WaitQueueKeyed<T>) it's sleeping in.
///
/// The handle stores a raw pointer to the wait queue and a function
/// pointer that knows how to remove-and-wake a specific task from it.
/// This avoids needing trait objects or generics in the task struct.
#[derive(Clone)]
pub struct WaitQueueHandle {
    ptr: *const (),
    wake_fn: fn(ptr: *const (), task: &Arc<TaskControlBlock>),
    remove_fn: fn(ptr: *const (), task: &Arc<TaskControlBlock>),
}

// Safety: ptr always points to a Sync+Send object with static effective lifetime.
unsafe impl Send for WaitQueueHandle {}
unsafe impl Sync for WaitQueueHandle {}

impl WaitQueueHandle {
    /// Wake a specific task by properly removing it from the wait queue
    /// and then calling `wakeup_task`.
    pub fn wake_waiter(&self, task: &Arc<TaskControlBlock>) {
        (self.wake_fn)(self.ptr, task);
    }

    /// Remove a task from this wait queue without making it runnable.
    pub fn remove_waiter(&self, task: &Arc<TaskControlBlock>) {
        (self.remove_fn)(self.ptr, task);
    }

    fn points_to(&self, ptr: *const ()) -> bool {
        self.ptr == ptr
    }
}

pub trait NextKey: Default + Eq + core::hash::Hash + Copy {
    fn next(&self) -> Self;
}

impl NextKey for u16 {
    fn next(&self) -> Self {
        self.wrapping_add(1)
    }
}

impl NextKey for () {
    fn next(&self) -> Self {
        *self
    }
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
        let task = self.prepare_to_wait(reason);

        // Re-check condition after enqueueing ourselves. If already ready,
        // cancel the sleep transition and keep running on this hart.
        if should_skip() {
            self.cancel_prepared_wait(&task);
            return;
        }

        self.block_prepared(task, reason);
    }

    /// Prepare the current task for sleeping on this queue.
    pub(crate) fn prepare_to_wait(&self, reason: WaitReason) -> Arc<TaskControlBlock> {
        let task = current_task().unwrap();
        let mut queue = self.queue.lock();
        {
            let mut task_inner = task.inner_exclusive_access();
            debug_assert!(matches!(task_inner.task_status, TaskStatus::Running));
            task_inner.task_status = TaskStatus::Interruptible;
            task_inner.wait_reason = Some(reason);
            task_inner.current_wq_handle = Some(self.to_handle());
        }
        queue.push_back(task.clone());
        task
    }

    /// Cancel a prepared wait before the task actually switches out.
    pub(crate) fn cancel_prepared_wait(&self, task: &Arc<TaskControlBlock>) {
        self.remove_waiter_by_ptr(task);
        let mut task_inner = task.inner_exclusive_access();
        if matches!(task_inner.task_status, TaskStatus::Interruptible) {
            task_inner.task_status = TaskStatus::Running;
            task_inner.wait_reason = None;
            task_inner.current_wq_handle = None;
            task_inner.sched.on_cpu = true;
            task_inner.sched.on_rq = false;
        }
    }

    /// Block after [`prepare_to_wait`] has enqueued the task.
    pub(crate) fn block_prepared(&self, task: Arc<TaskControlBlock>, reason: WaitReason) {
        block_current_and_run_next(reason);
        self.finish_wait(&task);
    }

    fn finish_wait(&self, task: &Arc<TaskControlBlock>) {
        // Wakers normally remove the task before scheduling it. Keep this
        // cleanup idempotent so forced exits and competing wake paths cannot
        // leave a stale strong reference in the queue.
        self.remove_waiter_by_ptr(task);
        let mut task_inner = task.inner_exclusive_access();
        if task_inner
            .current_wq_handle
            .as_ref()
            .is_some_and(|handle| handle.points_to(self as *const Self as *const ()))
        {
            task_inner.current_wq_handle = None;
        }
    }

    /// Remove and return the next valid waiter without waking it.
    pub(crate) fn take_one_waiter(&self) -> Option<Arc<TaskControlBlock>> {
        loop {
            let task = self.queue.lock().pop_front()?;
            if self.is_current_waiter(&task) {
                return Some(task);
            }
        }
    }

    /// Wake one waiter (FIFO order).
    pub fn wake_one(&self) {
        self.wake_up_to(1);
    }

    /// Wake up to `limit` waiters and return the number actually woken.
    pub fn wake_up_to(&self, limit: usize) -> usize {
        self.wake_up_to_with(limit, |_| {})
    }

    /// Wake up to `limit` waiters and run a callback before each wakeup.
    pub fn wake_up_to_with<F>(&self, limit: usize, mut on_wake: F) -> usize
    where
        F: FnMut(&Arc<TaskControlBlock>),
    {
        let mut count = 0;
        while count < limit {
            let Some(task) = self.queue.lock().pop_front() else {
                break;
            };
            if !self.is_current_waiter(&task) {
                continue;
            }
            on_wake(&task);
            wakeup_task(task);
            count += 1;
        }
        count
    }

    /// Wake all waiters.
    pub fn wake_all(&self) -> usize {
        self.wake_up_to(usize::MAX)
    }

    pub(crate) fn debug_waiter_count(&self) -> usize {
        self.queue.lock().len()
    }

    pub(crate) fn debug_waiters(&self) -> Vec<Arc<TaskControlBlock>> {
        self.queue.lock().iter().cloned().collect()
    }

    /// Wake some waiters from this queue and requeue the rest onto `dst`.
    pub fn wake_and_requeue(
        &self,
        dst: &WaitQueue,
        wake_count: usize,
        requeue_count: usize,
    ) -> usize {
        self.wake_and_requeue_with(dst, wake_count, requeue_count, |_| {})
    }

    /// Wake some waiters from this queue, running a callback before wakeup,
    /// and requeue the rest onto `dst`.
    pub fn wake_and_requeue_with<F>(
        &self,
        dst: &WaitQueue,
        wake_count: usize,
        requeue_count: usize,
        mut on_wake: F,
    ) -> usize
    where
        F: FnMut(&Arc<TaskControlBlock>),
    {
        if core::ptr::eq(self, dst) {
            return self.wake_up_to_with(wake_count, on_wake);
        }

        let src_ptr = self as *const Self as usize;
        let dst_ptr = dst as *const Self as usize;
        let dst_handle = dst.to_handle();
        let mut wake_list = Vec::new();
        let mut requeue_list = Vec::new();

        if src_ptr < dst_ptr {
            let mut src_queue = self.queue.lock();
            let mut dst_queue = dst.queue.lock();
            Self::collect_wake_and_requeue(
                &mut src_queue,
                &mut dst_queue,
                self as *const Self as *const (),
                &dst_handle,
                wake_count,
                requeue_count,
                &mut wake_list,
                &mut requeue_list,
            );
        } else {
            let mut dst_queue = dst.queue.lock();
            let mut src_queue = self.queue.lock();
            Self::collect_wake_and_requeue(
                &mut src_queue,
                &mut dst_queue,
                self as *const Self as *const (),
                &dst_handle,
                wake_count,
                requeue_count,
                &mut wake_list,
                &mut requeue_list,
            );
        }

        let moved = wake_list.len() + requeue_list.len();

        for task in wake_list {
            on_wake(&task);
            wakeup_task(task);
        }

        moved
    }

    fn collect_wake_and_requeue(
        src_queue: &mut VecDeque<Arc<TaskControlBlock>>,
        dst_queue: &mut VecDeque<Arc<TaskControlBlock>>,
        src_ptr: *const (),
        dst_handle: &WaitQueueHandle,
        wake_count: usize,
        requeue_count: usize,
        wake_list: &mut Vec<Arc<TaskControlBlock>>,
        requeue_list: &mut Vec<Arc<TaskControlBlock>>,
    ) {
        while wake_list.len() < wake_count {
            let Some(task) = src_queue.pop_front() else {
                break;
            };
            if !Self::task_waits_on_ptr(&task, src_ptr) {
                continue;
            }
            wake_list.push(task);
        }

        while requeue_list.len() < requeue_count {
            let Some(task) = src_queue.pop_front() else {
                break;
            };
            if !Self::task_waits_on_ptr(&task, src_ptr) {
                continue;
            }
            task.inner_exclusive_access().current_wq_handle = Some(dst_handle.clone());
            dst_queue.push_back(task.clone());
            requeue_list.push(task);
        }
    }

    /// Remove this specific task from the queue and wake it.
    pub fn wake_waiter_by_ptr(&self, task: &Arc<TaskControlBlock>) {
        let current_handle = {
            let task_inner = task.inner_exclusive_access();
            task_inner.current_wq_handle.clone()
        };
        if let Some(handle) = current_handle {
            if !handle.points_to(self as *const Self as *const ()) {
                handle.wake_waiter(task);
                return;
            }
        }
        self.remove_waiter_by_ptr(task);
        wakeup_task(task.clone());
    }

    fn remove_waiter_by_ptr(&self, task: &Arc<TaskControlBlock>) {
        let task_ptr = Arc::as_ptr(task);
        self.queue
            .lock()
            .retain(|queued| Arc::as_ptr(queued) != task_ptr);
    }

    fn is_current_waiter(&self, task: &Arc<TaskControlBlock>) -> bool {
        Self::task_waits_on_ptr(task, self as *const Self as *const ())
    }

    fn task_waits_on_ptr(task: &Arc<TaskControlBlock>, wait_queue_ptr: *const ()) -> bool {
        let task_inner = task.inner_exclusive_access();
        matches!(
            task_inner.task_status,
            TaskStatus::Interruptible | TaskStatus::Uninterruptible
        ) && task_inner
            .current_wq_handle
            .as_ref()
            .is_some_and(|handle| handle.points_to(wait_queue_ptr))
    }

    /// Build a type-erased handle for signal delivery.
    pub fn to_handle(&self) -> WaitQueueHandle {
        fn wake_fn(ptr: *const (), task: &Arc<TaskControlBlock>) {
            let wq = unsafe { &*(ptr as *const WaitQueue) };
            wq.wake_waiter_by_ptr(task);
        }
        fn remove_fn(ptr: *const (), task: &Arc<TaskControlBlock>) {
            let wq = unsafe { &*(ptr as *const WaitQueue) };
            wq.remove_waiter_by_ptr(task);
        }
        WaitQueueHandle {
            ptr: self as *const Self as *const (),
            wake_fn,
            remove_fn,
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
            task_inner.current_wq_handle = Some(self.to_handle());
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

        self.waiters.lock().remove(&key);
        self.queue.lock().retain(|queued| *queued != key);
        let task = current_task().unwrap();
        let mut task_inner = task.inner_exclusive_access();
        if task_inner
            .current_wq_handle
            .as_ref()
            .is_some_and(|handle| handle.points_to(self as *const Self as *const ()))
        {
            task_inner.current_wq_handle = None;
        }
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
            task_inner.current_wq_handle = Some(self.to_handle());
        }
        let replaced = self.waiters.lock().insert(key, task);
        assert!(replaced.is_none(), "duplicate wait key");
        self.queue.lock().push_back(key);

        // Re-check condition after enqueueing ourselves. If already ready,
        // cancel the sleep transition and keep running on this hart.
        if should_skip() {
            self.waiters.lock().remove(&key);
            self.queue.lock().retain(|queued| *queued != key);
            if let Some(task) = current_task() {
                let mut task_inner = task.inner_exclusive_access();
                if matches!(task_inner.task_status, TaskStatus::Interruptible) {
                    task_inner.task_status = TaskStatus::Running;
                    task_inner.wait_reason = None;
                    task_inner.current_wq_handle = None;
                    task_inner.sched.on_cpu = true;
                    task_inner.sched.on_rq = false;
                }
            }
            return;
        }

        block_current_and_run_next(reason);

        self.waiters.lock().remove(&key);
        self.queue.lock().retain(|queued| *queued != key);
        let task = current_task().unwrap();
        let mut task_inner = task.inner_exclusive_access();
        if task_inner
            .current_wq_handle
            .as_ref()
            .is_some_and(|handle| handle.points_to(self as *const Self as *const ()))
        {
            task_inner.current_wq_handle = None;
        }
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
            self.queue.lock().retain(|queued| *queued != key);
            // debug!("Poll: waking with key {}", key);
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

    /// Remove this specific task from the waiters and wake it.
    pub fn wake_waiter_by_ptr(&self, task: &Arc<TaskControlBlock>) {
        self.remove_waiter_by_ptr(task);
        wakeup_task(task.clone());
    }

    fn remove_waiter_by_ptr(&self, task: &Arc<TaskControlBlock>) {
        let task_ptr = Arc::as_ptr(task);
        let key = {
            let waiters = self.waiters.lock();
            waiters
                .iter()
                .find(|(_, t)| Arc::as_ptr(t) == task_ptr)
                .map(|(k, _)| *k)
        };
        if let Some(key) = key {
            self.waiters.lock().remove(&key);
            self.queue.lock().retain(|queued| *queued != key);
        }
    }

    /// Build a type-erased handle for signal delivery.
    pub fn to_handle(&self) -> WaitQueueHandle {
        fn wake_fn<T>(ptr: *const (), task: &Arc<TaskControlBlock>)
        where
            T: Default + Eq + core::hash::Hash + core::fmt::Display + Copy + NextKey,
        {
            let wq = unsafe { &*(ptr as *const WaitQueueKeyed<T>) };
            wq.wake_waiter_by_ptr(task);
        }
        fn remove_fn<T>(ptr: *const (), task: &Arc<TaskControlBlock>)
        where
            T: Default + Eq + core::hash::Hash + core::fmt::Display + Copy + NextKey,
        {
            let wq = unsafe { &*(ptr as *const WaitQueueKeyed<T>) };
            wq.remove_waiter_by_ptr(task);
        }
        WaitQueueHandle {
            ptr: self as *const Self as *const (),
            wake_fn: wake_fn::<T>,
            remove_fn: remove_fn::<T>,
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
