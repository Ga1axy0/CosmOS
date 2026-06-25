use crate::sync::SpinNoIrqLock;
use crate::{
    config::PAGE_SIZE,
    mm::VirtAddr,
    syscall::{errno::ERRNO, read_pod_from_user},
    task::{
        current_process, current_task, wakeup_task, ProcessControlBlock, TaskControlBlock,
        WaitQueue, WaitReason,
    },
    timer::{add_timer_with_futex_tag, get_time_ns},
};
use alloc::sync::Arc;
use alloc::vec::Vec;
use hashbrown::{HashMap, HashSet};
use lazy_static::lazy_static;

const MAX_FUTEX_WAITERS: usize = 1024;
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
    generation: u16,
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
    static ref FUTEX_QUEUES: SpinNoIrqLock<HashMap<FutexKey, Arc<WaitQueue>>> =
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
                slot_idx: idx as u16,
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
    slot_idx: u16,
    generation: u16,
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
    slot_idx: u16,
    generation: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FutexWakeState {
    Ready,
    TimedOut,
    Canceled,
}

fn futex_task_ids(task: &Arc<TaskControlBlock>) -> (usize, usize, usize) {
    let pid = task
        .process
        .upgrade()
        .map(|process| process.getpid())
        .unwrap_or(usize::MAX);
    let (tid, thread_id) = {
        let task_inner = task.inner_exclusive_access();
        task_inner
            .res
            .as_ref()
            .map(|res| (res.tid, res.thread_id()))
            .unwrap_or((usize::MAX, usize::MAX))
    };
    (pid, tid, thread_id)
}

fn futex_waiter_list(queue: &WaitQueue) -> Vec<(usize, usize, usize)> {
    queue
        .debug_waiters()
        .into_iter()
        .map(|task| futex_task_ids(&task))
        .collect()
}

fn futex_waiter_head(
    waiters: &[(usize, usize, usize)],
    limit: usize,
) -> Vec<(usize, usize, usize)> {
    waiters.iter().copied().take(limit).collect()
}

fn futex_waiter_tail(
    waiters: &[(usize, usize, usize)],
    limit: usize,
) -> Vec<(usize, usize, usize)> {
    if waiters.len() <= limit {
        return Vec::new();
    }
    waiters[waiters.len().saturating_sub(limit)..].to_vec()
}

fn futex_waiter_diff(
    before: &[(usize, usize, usize)],
    after: &[(usize, usize, usize)],
) -> Vec<(usize, usize, usize)> {
    let after_set: HashSet<_> = after.iter().copied().collect();
    before
        .iter()
        .copied()
        .filter(|waiter| !after_set.contains(waiter))
        .collect()
}

fn futex_waiter_intersection(
    waiters: &[(usize, usize, usize)],
    selected: &[(usize, usize, usize)],
) -> Vec<(usize, usize, usize)> {
    let selected_set: HashSet<_> = selected.iter().copied().collect();
    waiters
        .iter()
        .copied()
        .filter(|waiter| selected_set.contains(waiter))
        .collect()
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
    if let Some(queue) = queues.get(&key) {
        return Arc::clone(queue);
    }
    if queues.len() >= MAX_CACHED_FUTEX_KEYS {
        // `WaitQueueHandle` stores only a raw pointer, so the futex queue map
        // must keep a strong reference alive while waiters may still sleep on
        // that queue (including after requeue onto a destination futex).
        queues.retain(|_, queue| queue.debug_waiter_count() != 0);
    }
    let queue = Arc::new(WaitQueue::new());
    queues.insert(key, Arc::clone(&queue));
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
    let (pid, tid, thread_id) = futex_task_ids(task);
    for (idx, slot) in registry.slots.iter_mut().enumerate() {
        if slot.task_ptr == task_ptr && matches!(slot.state, FutexWaitState::Active) {
            task.note_first_futex_wake(get_time_ns());
            debug!(
                "[futex-warn] mark_ready pid={} tid={} thread_id={} slot={} gen={} state={:?}->{:?}",
                pid,
                tid,
                thread_id,
                idx,
                slot.generation,
                slot.state,
                FutexWaitState::Ready
            );
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
    let (pid, tid, thread_id) = futex_task_ids(task);
    {
        let mut registry = FUTEX_WAIT_REGISTRY.lock();
        if !registry.key_valid(handle) {
            debug!(
                "[futex-warn] timeout_skip_invalid pid={} tid={} thread_id={} slot={} gen={}",
                pid, tid, thread_id, handle.slot_idx, handle.generation
            );
            return true;
        }
        let slot = &mut registry.slots[handle.slot_idx as usize];
        if slot.task_ptr != Arc::as_ptr(task) as usize {
            debug!(
                "[futex-warn] timeout_skip_mismatch pid={} tid={} thread_id={} slot={} gen={} slot_task_ptr={:#x} timer_task_ptr={:#x}",
                pid,
                tid,
                thread_id,
                handle.slot_idx,
                handle.generation,
                slot.task_ptr,
                Arc::as_ptr(task) as usize
            );
            return true;
        }
        if matches!(slot.state, FutexWaitState::Ready) {
            debug!(
                "[futex-warn] timeout_skip_ready pid={} tid={} thread_id={} slot={} gen={}",
                pid, tid, thread_id, handle.slot_idx, handle.generation
            );
            return true;
        }
        if !matches!(slot.state, FutexWaitState::Active) {
            debug!(
                "[futex-warn] timeout_skip_state pid={} tid={} thread_id={} slot={} gen={} state={:?}",
                pid,
                tid,
                thread_id,
                handle.slot_idx,
                handle.generation,
                slot.state
            );
            return true;
        }
        debug!(
            "[futex-warn] timeout_fire pid={} tid={} thread_id={} slot={} gen={} state={:?}->{:?}",
            pid,
            tid,
            thread_id,
            handle.slot_idx,
            handle.generation,
            slot.state,
            FutexWaitState::TimedOut
        );
        slot.state = FutexWaitState::TimedOut;
    }
    task.note_first_futex_wake(get_time_ns());
    wake_task_via_wait_handle(task);
    true
}

fn futex_wake_key(key: FutexKey, uaddr: usize, max_count: usize) -> isize {
    let queue = {
        let queues = FUTEX_QUEUES.lock();
        queues.get(&key).cloned()
    };
    let woke = queue
        .map(|q| {
            let before_waiters = futex_waiter_list(&q);
            let before = before_waiters.len();
            let woke = q.wake_up_to_with(max_count, |task| {
                futex_wait_mark_ready(task);
            }) as isize;
            let after_waiters = futex_waiter_list(&q);
            let after = after_waiters.len();
            let removed_waiters = futex_waiter_diff(&before_waiters, &after_waiters);
            debug!(
                "[futex-queue] op=wake uaddr={:#x} max_count={} before={} after={} woke={} before_head={:?} before_tail={:?} after_head={:?} after_tail={:?} removed_count={} removed_head={:?} removed_tail={:?}",
                uaddr,
                max_count,
                before,
                after,
                woke,
                futex_waiter_head(&before_waiters, 8),
                futex_waiter_tail(&before_waiters, 8),
                futex_waiter_head(&after_waiters, 8),
                futex_waiter_tail(&after_waiters, 8),
                removed_waiters.len(),
                futex_waiter_head(&removed_waiters, 8),
                futex_waiter_tail(&removed_waiters, 8),
            );
            woke
        })
        .unwrap_or_else(|| {
            debug!(
                "[futex-queue] op=wake uaddr={:#x} max_count={} before=0 after=0 woke=0 before_head=[] before_tail=[] after_head=[] after_tail=[] removed_count=0 removed_head=[] removed_tail=[] queue_missing=true",
                uaddr,
                max_count,
            );
            0
        });
    woke
}

/// Wake tasks waiting on a futex in the current process.
pub fn futex_wake_addr(uaddr: usize, max_count: usize, private: bool) -> Result<isize, ERRNO> {
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
    let src_before_waiters = futex_waiter_list(&src);
    let dst_before_waiters = futex_waiter_list(&dst);
    let src_before = src_before_waiters.len();
    let dst_before = dst_before_waiters.len();
    let mut woke_count = 0usize;
    let moved = src.wake_and_requeue_with(&dst, wake_count, requeue_count, |task| {
        woke_count += 1;
        futex_wait_mark_ready(task);
    });
    let src_after_waiters = futex_waiter_list(&src);
    let dst_after_waiters = futex_waiter_list(&dst);
    let src_after = src_after_waiters.len();
    let dst_after = dst_after_waiters.len();
    let src_removed_waiters = futex_waiter_diff(&src_before_waiters, &src_after_waiters);
    let dst_added_waiters = futex_waiter_diff(&dst_after_waiters, &dst_before_waiters);
    let requeued_waiters = futex_waiter_intersection(&src_removed_waiters, &dst_added_waiters);
    let requeued_set: HashSet<_> = requeued_waiters.iter().copied().collect();
    let woke_waiters: Vec<_> = src_removed_waiters
        .iter()
        .copied()
        .filter(|waiter| !requeued_set.contains(waiter))
        .collect();
    let requeued = moved.saturating_sub(woke_count);
    debug!(
        "[futex-queue] op=requeue uaddr={:#x} uaddr2={:#x} wake_req={} requeue_req={} src_before={} src_after={} dst_before={} dst_after={} moved={} woke={} requeued={} src_before_head={:?} src_before_tail={:?} src_after_head={:?} src_after_tail={:?} dst_before_head={:?} dst_before_tail={:?} dst_after_head={:?} dst_after_tail={:?} src_removed_count={} src_removed_head={:?} src_removed_tail={:?} dst_added_count={} dst_added_head={:?} dst_added_tail={:?} woke_waiters_count={} woke_waiters_head={:?} woke_waiters_tail={:?} requeued_waiters_count={} requeued_waiters_head={:?} requeued_waiters_tail={:?}",
        uaddr,
        uaddr2,
        wake_count,
        requeue_count,
        src_before,
        src_after,
        dst_before,
        dst_after,
        moved,
        woke_count,
        requeued,
        futex_waiter_head(&src_before_waiters, 8),
        futex_waiter_tail(&src_before_waiters, 8),
        futex_waiter_head(&src_after_waiters, 8),
        futex_waiter_tail(&src_after_waiters, 8),
        futex_waiter_head(&dst_before_waiters, 8),
        futex_waiter_tail(&dst_before_waiters, 8),
        futex_waiter_head(&dst_after_waiters, 8),
        futex_waiter_tail(&dst_after_waiters, 8),
        src_removed_waiters.len(),
        futex_waiter_head(&src_removed_waiters, 8),
        futex_waiter_tail(&src_removed_waiters, 8),
        dst_added_waiters.len(),
        futex_waiter_head(&dst_added_waiters, 8),
        futex_waiter_tail(&dst_added_waiters, 8),
        woke_waiters.len(),
        futex_waiter_head(&woke_waiters, 8),
        futex_waiter_tail(&woke_waiters, 8),
        requeued_waiters.len(),
        futex_waiter_head(&requeued_waiters, 8),
        futex_waiter_tail(&requeued_waiters, 8),
    );
    Ok(moved as isize)
}

/// Compare the futex word at `uaddr`, then wake/requeue waiters if it matches.
pub fn futex_cmp_requeue_addr(
    uaddr: *const i32,
    uaddr2: usize,
    wake_count: usize,
    requeue_count: usize,
    expected: i32,
    private: bool,
) -> Result<isize, ERRNO> {
    let current = read_pod_from_user(uaddr)?;
    if current != expected {
        return Err(ERRNO::EAGAIN);
    }
    futex_requeue_addr(uaddr as usize, uaddr2, wake_count, requeue_count, private)
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
    let wait_entry_ns = get_time_ns();
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
    if let Some(timing) = task.note_first_futex_wait(wait_entry_ns) {
        let (pid, tid, thread_id) = futex_task_ids(&task);
        debug!(
            "[clone-chain] pid={} tid={} thread_id={} expected={} clone_ready_ns={} first_run_ns={} first_user_return_ns={} first_futex_wait_ns={} clone_to_run_ns={} run_to_user_ns={} user_to_futex_ns={} clone_to_futex_ns={}",
            pid,
            tid,
            thread_id,
            expected,
            timing.clone_ready_ns,
            timing.first_run_ns,
            timing.first_user_return_ns,
            timing.first_futex_wait_ns,
            timing.first_run_ns.saturating_sub(timing.clone_ready_ns),
            timing.first_user_return_ns.saturating_sub(timing.first_run_ns),
            timing.first_futex_wait_ns
                .saturating_sub(timing.first_user_return_ns),
            timing.first_futex_wait_ns
                .saturating_sub(timing.clone_ready_ns),
        );
    }
    let handle = deadline_ns
        .map(|_| register_futex_wait(&task).ok_or(ERRNO::EAGAIN))
        .transpose()?;
    if let (Some(deadline_ns), Some(handle)) = (deadline_ns, handle) {
        let (pid, tid, thread_id) = futex_task_ids(&task);
        debug!(
            "[futex-warn] arm_timer pid={} tid={} thread_id={} slot={} gen={} uaddr={:#x} expected={} deadline_ns={} now_ns={}",
            pid,
            tid,
            thread_id,
            handle.slot_idx,
            handle.generation,
            uaddr as usize,
            expected,
            deadline_ns,
            get_time_ns()
        );
        add_timer_with_futex_tag(deadline_ns, Arc::clone(&task), Some(handle.timer_tag()));
    }
    queue.wait_with_reason_or_skip(WaitReason::Futex, || {
        let current_after_enqueue = read_pod_from_user(uaddr);
        let value_changed = current_after_enqueue
            .as_ref()
            .map(|current| *current != expected)
            .unwrap_or(true);
        let handle_ready = handle.is_some_and(futex_wait_should_skip);
        value_changed || handle_ready || crate::signal::has_unmasked_pending_signal()
    });
    if let Some(handle) = handle {
        let wake_state = futex_wait_state(handle);
        let wait_done_ns = get_time_ns();
        let (pid, tid, thread_id) = futex_task_ids(&task);
        debug!(
            "[futex-warn] wait_done pid={} tid={} thread_id={} slot={} gen={} uaddr={:#x} expected={} wake_state={:?}",
            pid,
            tid,
            thread_id,
            handle.slot_idx,
            handle.generation,
            uaddr as usize,
            expected,
            wake_state
        );
        if let Some(timing) = task.note_first_futex_wait_done(wait_done_ns) {
            debug!(
                "[futex-chain] pid={} tid={} thread_id={} expected={} wake_state={:?} clone_ready_ns={} first_run_ns={} first_user_return_ns={} first_futex_wait_ns={} first_futex_wake_ns={} first_post_futex_run_ns={} first_futex_wait_done_ns={} wait_to_wake_ns={} wake_to_run_ns={} run_to_wait_done_ns={} wake_to_wait_done_ns={} total_wait_roundtrip_ns={}",
                pid,
                tid,
                thread_id,
                expected,
                wake_state,
                timing.clone_ready_ns,
                timing.first_run_ns,
                timing.first_user_return_ns,
                timing.first_futex_wait_ns,
                timing.first_futex_wake_ns,
                timing.first_post_futex_run_ns,
                timing.first_futex_wait_done_ns,
                timing.first_futex_wake_ns
                    .saturating_sub(timing.first_futex_wait_ns),
                timing.first_post_futex_run_ns
                    .saturating_sub(timing.first_futex_wake_ns),
                timing.first_futex_wait_done_ns
                    .saturating_sub(timing.first_post_futex_run_ns),
                timing.first_futex_wait_done_ns
                    .saturating_sub(timing.first_futex_wake_ns),
                timing.first_futex_wait_done_ns
                    .saturating_sub(timing.first_futex_wait_ns),
            );
        }
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
