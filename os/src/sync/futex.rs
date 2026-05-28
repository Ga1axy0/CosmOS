use crate::sync::SpinNoIrqLock;
use crate::{
    syscall::{read_pod_from_user, errno::ERRNO, Timespec},
    task::{current_task, wakeup_task, TaskControlBlock, WaitQueue, WaitReason},
    timer::{add_timer_with_futex_tag, get_time_ms},
};
use alloc::sync::Arc;
use hashbrown::HashMap;
use lazy_static::lazy_static;

const MAX_FUTEX_WAITERS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FutexWaitState {
    Free,
    Active,
    Ready,
    TimedOut,
}

#[derive(Clone, Copy, Debug)]
struct FutexWaitSlot {
    generation: u8,
    state: FutexWaitState,
    task_ptr: usize,
}

impl FutexWaitSlot {
    const EMPTY: Self = Self {
        generation: 0,
        state: FutexWaitState::Free,
        task_ptr: 0,
    };
}

impl Default for FutexWaitSlot {
    fn default() -> Self {
        Self::EMPTY
    }
}

lazy_static! {
    static ref FUTEX_QUEUES: SpinNoIrqLock<HashMap<usize, Arc<WaitQueue>>> =
        SpinNoIrqLock::new(HashMap::new());
    static ref FUTEX_WAIT_REGISTRY: SpinNoIrqLock<FutexWaitRegistry> =
        SpinNoIrqLock::new(FutexWaitRegistry::new());
}

#[derive(Debug)]
struct FutexWaitRegistry {
    slots: [FutexWaitSlot; MAX_FUTEX_WAITERS],
    next_slot: usize,
}

impl FutexWaitRegistry {
    const fn new() -> Self {
        Self {
            slots: [FutexWaitSlot::EMPTY; MAX_FUTEX_WAITERS],
            next_slot: 0,
        }
    }

    fn alloc(&mut self, task: &Arc<TaskControlBlock>) -> Option<FutexWaitHandle> {
        for off in 0..MAX_FUTEX_WAITERS {
            let idx = (self.next_slot + off) % MAX_FUTEX_WAITERS;
            if !matches!(self.slots[idx].state, FutexWaitState::Free) {
                continue;
            }
            let slot = &mut self.slots[idx];
            slot.generation = slot.generation.wrapping_add(1);
            slot.state = FutexWaitState::Active;
            slot.task_ptr = Arc::as_ptr(task) as usize;
            self.next_slot = (idx + 1) % MAX_FUTEX_WAITERS;
            return Some(FutexWaitHandle {
                slot_idx: idx as u8,
                generation: slot.generation,
            });
        }
        None
    }

    fn key_valid(&self, handle: FutexWaitHandle) -> bool {
        let idx = handle.slot_idx as usize;
        idx < MAX_FUTEX_WAITERS
            && !matches!(self.slots[idx].state, FutexWaitState::Free)
            && self.slots[idx].generation == handle.generation
    }

    fn cleanup(&mut self, handle: FutexWaitHandle) {
        if !self.key_valid(handle) {
            return;
        }
        let slot = &mut self.slots[handle.slot_idx as usize];
        slot.state = FutexWaitState::Free;
        slot.task_ptr = 0;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FutexWaitHandle {
    slot_idx: u8,
    generation: u8,
}

impl FutexWaitHandle {
    fn timer_tag(self) -> FutexTimerTag {
        FutexTimerTag {
            slot_idx: self.slot_idx,
            generation: self.generation,
        }
    }
}

/// A tag used for identifying futex wait timers, containing the necessary information to correlate a timer expiration with a specific futex wait slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FutexTimerTag {
    slot_idx: u8,
    generation: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FutexWakeState {
    Ready,
    TimedOut,
    Canceled,
}

/// Get or create the wait queue associated with the given futex address.
pub fn futex_queue(uaddr: usize) -> Arc<WaitQueue> {
    let mut queues = FUTEX_QUEUES.lock();
    queues
        .entry(uaddr)
        .or_insert_with(|| Arc::new(WaitQueue::new()))
        .clone()
}

fn futex_timeout_ms(timeout: *const Timespec) -> Result<Option<usize>, ERRNO> {
    if timeout.is_null() {
        return Ok(None);
    }
    let timeout = read_pod_from_user(timeout)?;
    if timeout.tv_nsec >= 1_000_000_000 {
        return Err(ERRNO::EINVAL);
    }
    let sec_ms = timeout.tv_sec.checked_mul(1_000).ok_or(ERRNO::EINVAL)?;
    let nsec_ms = timeout.tv_nsec.div_ceil(1_000_000);
    let timeout_ms = sec_ms.checked_add(nsec_ms).ok_or(ERRNO::EINVAL)?;
    Ok(Some(timeout_ms))
}

fn register_futex_wait(task: &Arc<TaskControlBlock>) -> Option<FutexWaitHandle> {
    FUTEX_WAIT_REGISTRY.lock().alloc(task)
}

fn cleanup_futex_wait(handle: FutexWaitHandle) {
    FUTEX_WAIT_REGISTRY.lock().cleanup(handle);
}

/// Clean up any futex wait slots associated with the given task, marking them as free.
pub fn cleanup_futex_wait_for_task(task: &Arc<TaskControlBlock>) {
    let task_ptr = Arc::as_ptr(task) as usize;
    let mut registry = FUTEX_WAIT_REGISTRY.lock();
    for slot in registry.slots.iter_mut() {
        if slot.task_ptr == task_ptr && !matches!(slot.state, FutexWaitState::Free) {
            slot.state = FutexWaitState::Free;
            slot.task_ptr = 0;
        }
    }
}

fn futex_wait_state(handle: FutexWaitHandle) -> FutexWakeState {
    let registry = FUTEX_WAIT_REGISTRY.lock();
    if !registry.key_valid(handle) {
        return FutexWakeState::Canceled;
    }
    match registry.slots[handle.slot_idx as usize].state {
        FutexWaitState::Ready => FutexWakeState::Ready,
        FutexWaitState::TimedOut => FutexWakeState::TimedOut,
        FutexWaitState::Active | FutexWaitState::Free => FutexWakeState::Canceled,
    }
}

fn futex_wait_should_skip(handle: FutexWaitHandle) -> bool {
    let registry = FUTEX_WAIT_REGISTRY.lock();
    if !registry.key_valid(handle) {
        return true;
    }
    !matches!(
        registry.slots[handle.slot_idx as usize].state,
        FutexWaitState::Active
    )
}

/// Mark the futex wait slot associated with the given task as ready, so that the waiting task can be woken up by the futex wake logic.
pub fn futex_wait_mark_ready(task: &Arc<TaskControlBlock>) {
    let task_ptr = Arc::as_ptr(task) as usize;
    let mut registry = FUTEX_WAIT_REGISTRY.lock();
    for slot in registry.slots.iter_mut() {
        if slot.task_ptr == task_ptr && matches!(slot.state, FutexWaitState::Active) {
            slot.state = FutexWaitState::Ready;
        }
    }
}

fn wake_task_via_wait_handle(task: &Arc<TaskControlBlock>) {
    let handle = {
        let task_inner = task.inner_exclusive_access();
        task_inner.current_wq_handle.clone()
    };
    if let Some(handle) = handle {
        handle.wake_waiter(task);
        return;
    }
    wakeup_task(Arc::clone(task));
}

/// Handle a futex wait timeout by marking the wait slot as timed out and waking up the task.
pub fn handle_futex_wait_timeout(tag: FutexTimerTag, task: &Arc<TaskControlBlock>) -> bool {
    let handle = FutexWaitHandle {
        slot_idx: tag.slot_idx,
        generation: tag.generation,
    };
    {
        let mut registry = FUTEX_WAIT_REGISTRY.lock();
        if !registry.key_valid(handle) {
            return true;
        }
        let slot = &mut registry.slots[handle.slot_idx as usize];
        if slot.task_ptr != Arc::as_ptr(task) as usize {
            return true;
        }
        if matches!(slot.state, FutexWaitState::Ready) {
            return true;
        }
        if !matches!(slot.state, FutexWaitState::Active) {
            return true;
        }
        slot.state = FutexWaitState::TimedOut;
    }
    wake_task_via_wait_handle(task);
    true
}

/// Wake up tasks waiting on the given futex address, up to the specified maximum count.
pub fn futex_wake_addr(uaddr: usize, max_count: usize) -> isize {
    let queue = {
        let queues = FUTEX_QUEUES.lock();
        queues.get(&uaddr).cloned()
    };
    queue
        .map(|q| q.wake_up_to_with(max_count, futex_wait_mark_ready) as isize)
        .unwrap_or(0)
}

/// Wait on the futex at the given user-space address if its current value equals the expected value, with an optional timeout.
pub fn futex_wait_addr(
    uaddr: *const i32,
    expected: i32,
    timeout: Option<*const Timespec>,
) -> Result<isize, ERRNO> {
    let current = read_pod_from_user(uaddr)?;
    if current != expected {
        return Err(ERRNO::EAGAIN);
    }

    let timeout_ms = match timeout {
        Some(timeout) => futex_timeout_ms(timeout)?,
        None => None,
    };
    if matches!(timeout_ms, Some(0)) {
        return Err(ERRNO::ETIMEDOUT);
    }

    let queue = futex_queue(uaddr as usize);
    let task = current_task().unwrap();
    let handle = timeout_ms
        .map(|_| register_futex_wait(&task).ok_or(ERRNO::EAGAIN))
        .transpose()?;
    if let (Some(timeout_ms), Some(handle)) = (timeout_ms, handle) {
        let deadline = get_time_ms().checked_add(timeout_ms).ok_or(ERRNO::EINVAL)?;
        add_timer_with_futex_tag(deadline, Arc::clone(&task), Some(handle.timer_tag()));
    }
    queue.wait_with_reason_or_skip(WaitReason::Futex, || {
        read_pod_from_user(uaddr)
            .map(|current| current != expected)
            .unwrap_or(true)
            || handle.is_some_and(futex_wait_should_skip)
    });
    if let Some(handle) = handle {
        let wake_state = futex_wait_state(handle);
        cleanup_futex_wait(handle);
        if matches!(wake_state, FutexWakeState::TimedOut) {
            return Err(ERRNO::ETIMEDOUT);
        }
    }
    if crate::signal::has_unmasked_pending_signal() {
        return Err(ERRNO::EINTR);
    }
    Ok(0)
}
