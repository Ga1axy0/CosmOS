use crate::sync::SpinNoIrqLock;
use crate::{
    config::PAGE_SIZE,
    mm::VirtAddr,
    syscall::{read_pod_from_user, errno::ERRNO},
    task::{
        current_process, current_task, wakeup_task, ProcessControlBlock, TaskControlBlock,
        WaitQueue, WaitReason,
    },
    timer::{add_timer_with_futex_tag, get_time_ns},
};
use alloc::sync::{Arc, Weak};
use hashbrown::HashMap;
use lazy_static::lazy_static;

const MAX_FUTEX_WAITERS: usize = 256;
const MAX_CACHED_FUTEX_KEYS: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct FutexKey {
    address: usize,
    private_mm: Option<usize>,
}

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
    static ref FUTEX_QUEUES: SpinNoIrqLock<HashMap<FutexKey, Weak<WaitQueue>>> =
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

fn futex_key(
    process: &Arc<ProcessControlBlock>,
    uaddr: usize,
    private: bool,
) -> Result<FutexKey, ERRNO> {
    let process_inner = process.inner_exclusive_access();
    if private {
        return Ok(FutexKey {
            address: uaddr,
            private_mm: Some(process_inner.memory_set.token()),
        });
    }

    let va = VirtAddr(uaddr);
    let pte = process_inner
        .memory_set
        .translate(va.floor())
        .ok_or(ERRNO::EFAULT)?;
    Ok(FutexKey {
        address: pte.ppn().0 * PAGE_SIZE + va.page_offset(),
        private_mm: None,
    })
}

fn futex_queue_by_key(key: FutexKey) -> Arc<WaitQueue> {
    let mut queues = FUTEX_QUEUES.lock();
    if let Some(queue) = queues.get(&key).and_then(Weak::upgrade) {
        return queue;
    }
    if queues.len() >= MAX_CACHED_FUTEX_KEYS {
        queues.retain(|_, queue| queue.strong_count() != 0);
    }
    let queue = Arc::new(WaitQueue::new());
    queues.insert(key, Arc::downgrade(&queue));
    queue
}

/// Get or create the wait queue associated with a futex in the current process.
pub fn futex_queue(uaddr: usize, private: bool) -> Result<Arc<WaitQueue>, ERRNO> {
    let key = futex_key(&current_process(), uaddr, private)?;
    Ok(futex_queue_by_key(key))
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

fn futex_wake_key(key: FutexKey, uaddr: usize, max_count: usize) -> isize {
    let queue = {
        let queues = FUTEX_QUEUES.lock();
        queues.get(&key).and_then(Weak::upgrade)
    };
    let woke = queue
        .map(|q| {
            q.wake_up_to_with(max_count, |task| {

                futex_wait_mark_ready(task);
            }) as isize
        })
        .unwrap_or(0);
    woke
}

/// Wake tasks waiting on a futex in the current process.
pub fn futex_wake_addr(
    uaddr: usize,
    max_count: usize,
    private: bool,
) -> Result<isize, ERRNO> {
    futex_wake_addr_in_process(&current_process(), uaddr, max_count, private)
}

/// Wake tasks using an explicitly supplied process address space.
pub fn futex_wake_addr_in_process(
    process: &Arc<ProcessControlBlock>,
    uaddr: usize,
    max_count: usize,
    private: bool,
) -> Result<isize, ERRNO> {
    let key = futex_key(process, uaddr, private)?;
    Ok(futex_wake_key(key, uaddr, max_count))
}

/// Wake waiters on one futex and move more waiters to another futex queue.
pub fn futex_requeue_addr(
    uaddr: usize,
    uaddr2: usize,
    wake_count: usize,
    requeue_count: usize,
    private: bool,
) -> Result<isize, ERRNO> {
    let process = current_process();
    let src_key = futex_key(&process, uaddr, private)?;
    let dst_key = futex_key(&process, uaddr2, private)?;
    let src = futex_queue_by_key(src_key);
    let dst = futex_queue_by_key(dst_key);
    Ok(src.wake_and_requeue_with(
        &dst,
        wake_count,
        requeue_count,
        futex_wait_mark_ready,
    ) as isize)
}

/// Wait on the futex at `uaddr` while its value still equals `expected`.
///
/// `deadline_ns`, when present, is an **absolute CLOCK_MONOTONIC** deadline in
/// nanoseconds (already normalized by the caller — see
/// `futex_deadline_mono_ns`), because the kernel timer queue only fires against
/// the monotonic clock. `None` means wait indefinitely.
pub fn futex_wait_addr(
    uaddr: *const i32,
    expected: i32,
    deadline_ns: Option<u64>,
    private: bool,
) -> Result<isize, ERRNO> {
    let task = current_task().unwrap();
    let current = read_pod_from_user(uaddr)?;
    if current != expected {
        return Err(ERRNO::EAGAIN);
    }

    // 截止时刻已过：立即超时，不进入等待（也避免后续错过唤醒）。
    if let Some(deadline) = deadline_ns {
        if deadline <= get_time_ns() {
            return Err(ERRNO::ETIMEDOUT);
        }
    }

    let queue = futex_queue(uaddr as usize, private)?;
    let handle = deadline_ns
        .map(|_| register_futex_wait(&task).ok_or(ERRNO::EAGAIN))
        .transpose()?;
    if let (Some(deadline_ns), Some(handle)) = (deadline_ns, handle) {
        add_timer_with_futex_tag(deadline_ns, Arc::clone(&task), Some(handle.timer_tag()));
    }
    queue.wait_with_reason_or_skip(WaitReason::Futex, || {
        let current_after_enqueue = read_pod_from_user(uaddr);
        let value_changed = current_after_enqueue
            .as_ref()
            .map(|current| *current != expected)
            .unwrap_or(true);
        let handle_ready = handle.is_some_and(futex_wait_should_skip);
        value_changed || handle_ready
    });
    if let Some(handle) = handle {
        let wake_state = futex_wait_state(handle);
        cleanup_futex_wait(handle);
        match wake_state {
            FutexWakeState::Ready => return Ok(0),
            FutexWakeState::TimedOut => return Err(ERRNO::ETIMEDOUT),
            FutexWakeState::Canceled => {}
        }
    }
    if crate::signal::has_unmasked_pending_signal() {
        return Err(ERRNO::EINTR);
    }
    Ok(0)
}
