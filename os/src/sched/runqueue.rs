//! Per-hart runqueue management for RT and CFS scheduling classes.

use super::{current_task, processor::processor_for_hart};
use crate::config::MAX_HARTS;
use crate::hart::hartid;
use crate::mm::online_mask as online_hart_mask;
use crate::sbi::send_ipi_mask;
use crate::sched::{request_current_task_resched, CFS_WAKEUP_GRANULARITY_NS};
use crate::sync::SpinNoIrqLock;
use crate::task::{
    ProcessControlBlock, ReschedReason, SchedPolicy, TaskControlBlock, TaskControlBlockInner,
    TaskStatus, SCHED_RT_PRIO_MAX,
};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::array;
use lazy_static::*;

const RT_QUEUE_LEVELS: usize = SCHED_RT_PRIO_MAX as usize + 1;
type CfsKey = (u64, usize);

#[derive(Copy, Clone)]
struct EnqueuedTaskInfo {
    policy: SchedPolicy,
    rt_priority: u8,
    vruntime_ns: u64,
}

/// Local runnable queues owned by one hart.
struct RunQueue {
    rt_queues: [VecDeque<Arc<TaskControlBlock>>; RT_QUEUE_LEVELS],
    highest_rt_prio: Option<u8>,
    rt_nr_running: usize,
    cfs_tasks: BTreeMap<CfsKey, Arc<TaskControlBlock>>,
    cfs_nr_running: usize,
    cfs_load: u64,
    min_vruntime_ns: u64,
    /// Keep a reference to the last exiting task so its kernel stack
    /// is not freed while this hart is still running on it.
    stop_task: Option<Arc<TaskControlBlock>>,
}

impl RunQueue {
    fn new() -> Self {
        Self {
            rt_queues: array::from_fn(|_| VecDeque::new()),
            highest_rt_prio: None,
            rt_nr_running: 0,
            cfs_tasks: BTreeMap::new(),
            cfs_nr_running: 0,
            cfs_load: 0,
            min_vruntime_ns: 0,
            stop_task: None,
        }
    }

    fn enqueue_locked(
        &mut self,
        task: Arc<TaskControlBlock>,
        task_inner: &mut TaskControlBlockInner,
    ) -> EnqueuedTaskInfo {
        match task_inner.sched.policy {
            SchedPolicy::Fifo | SchedPolicy::Rr => {
                let prio = task_inner.sched.rt_priority;
                if task_inner.sched.rt_enqueue_head {
                    self.rt_queues[prio as usize].push_front(task);
                } else {
                    self.rt_queues[prio as usize].push_back(task);
                }
                task_inner.sched.rt_enqueue_head = false;
                self.rt_nr_running += 1;
                self.highest_rt_prio =
                    Some(self.highest_rt_prio.map_or(prio, |curr| curr.max(prio)));
                EnqueuedTaskInfo {
                    policy: task_inner.sched.policy,
                    rt_priority: prio,
                    vruntime_ns: 0,
                }
            }
            SchedPolicy::Other => {
                let (placed_vruntime, initialized) = self.place_cfs_entity(
                    task_inner.sched.vruntime_ns,
                    task_inner.sched.cfs_initialized,
                );
                task_inner.sched.vruntime_ns = placed_vruntime;
                task_inner.sched.cfs_initialized = initialized;
                let vruntime = task_inner.sched.vruntime_ns;
                let weight = task_inner.sched.weight;
                let key = (vruntime, Arc::as_ptr(&task) as usize);
                task_inner.sched.cfs_rq_key = Some(key);
                self.cfs_tasks.insert(key, task);
                self.cfs_nr_running += 1;
                self.cfs_load = self.cfs_load.saturating_add(weight);
                self.refresh_min_vruntime(None);
                EnqueuedTaskInfo {
                    policy: SchedPolicy::Other,
                    rt_priority: 0,
                    vruntime_ns: vruntime,
                }
            }
            SchedPolicy::Idle => unreachable!("idle tasks are not enqueued"),
        }
    }

    fn place_cfs_entity(&self, vruntime_ns: u64, initialized: bool) -> (u64, bool) {
        if !initialized {
            return (self.min_vruntime_ns, true);
        }
        let sleeper_floor = self
            .min_vruntime_ns
            .saturating_sub(CFS_WAKEUP_GRANULARITY_NS);
        let placed_vruntime = vruntime_ns.max(sleeper_floor);
        (placed_vruntime, true)
    }

    fn dequeue_highest_rt(&mut self) -> Option<Arc<TaskControlBlock>> {
        let prio = self.highest_rt_prio?;
        let task = self.rt_queues[prio as usize].pop_front()?;
        self.rt_nr_running = self.rt_nr_running.saturating_sub(1);
        self.refresh_highest_rt_prio();
        Some(task)
    }

    fn dequeue_leftmost_cfs(&mut self) -> Option<Arc<TaskControlBlock>> {
        let key = *self.cfs_tasks.keys().next()?;
        self.remove_cfs_by_key(key)
    }

    fn remove_cfs_by_key(&mut self, key: CfsKey) -> Option<Arc<TaskControlBlock>> {
        let task = self.cfs_tasks.remove(&key)?;
        let accounted_vruntime = {
            let mut task_inner = task.inner_exclusive_access();
            task_inner.sched.cfs_rq_key = None;
            self.cfs_load = self.cfs_load.saturating_sub(task_inner.sched.weight);
            task_inner.sched.vruntime_ns
        };
        self.cfs_nr_running = self.cfs_nr_running.saturating_sub(1);
        self.refresh_min_vruntime(Some(accounted_vruntime));
        Some(task)
    }

    fn remove_task(&mut self, task: &Arc<TaskControlBlock>) -> bool {
        let (policy, prio, cfs_key) = {
            let task_inner = task.inner_exclusive_access();
            (
                task_inner.sched.policy,
                task_inner.sched.rt_priority,
                task_inner.sched.cfs_rq_key,
            )
        };
        match policy {
            SchedPolicy::Fifo | SchedPolicy::Rr => {
                if let Some((idx, _)) = self.rt_queues[prio as usize]
                    .iter()
                    .enumerate()
                    .find(|(_, t)| Arc::as_ptr(t) == Arc::as_ptr(task))
                {
                    self.rt_queues[prio as usize].remove(idx);
                    self.rt_nr_running = self.rt_nr_running.saturating_sub(1);
                    self.refresh_highest_rt_prio();
                    true
                } else {
                    false
                }
            }
            SchedPolicy::Other => {
                if let Some(key) = cfs_key {
                    return self.remove_cfs_by_key(key).is_some();
                }
                let key = self
                    .cfs_tasks
                    .iter()
                    .find(|(_, queued)| Arc::as_ptr(*queued) == Arc::as_ptr(task))
                    .map(|(key, _)| *key);
                key.and_then(|key| self.remove_cfs_by_key(key)).is_some()
            }
            SchedPolicy::Idle => false,
        }
    }

    fn highest_rt_prio(&self) -> Option<u8> {
        self.highest_rt_prio
    }

    fn has_same_or_higher_rt(&self, prio: u8) -> bool {
        self.highest_rt_prio.is_some_and(|highest| highest >= prio)
    }

    fn total_nr_running(&self) -> usize {
        self.rt_nr_running + self.cfs_nr_running
    }

    fn cfs_load_score(&self) -> (u64, usize) {
        (self.cfs_load, self.cfs_nr_running)
    }

    fn leftmost_cfs_vruntime(&self) -> Option<u64> {
        self.cfs_tasks.keys().next().map(|key| key.0)
    }

    fn refresh_highest_rt_prio(&mut self) {
        self.highest_rt_prio = (1..RT_QUEUE_LEVELS)
            .rev()
            .find(|prio| !self.rt_queues[*prio].is_empty())
            .map(|prio| prio as u8);
    }

    fn refresh_min_vruntime(&mut self, accounted_vruntime: Option<u64>) {
        if let Some(vruntime) = accounted_vruntime {
            self.min_vruntime_ns = self.min_vruntime_ns.max(vruntime);
        }
        if let Some(leftmost) = self.leftmost_cfs_vruntime() {
            self.min_vruntime_ns = self.min_vruntime_ns.max(leftmost);
        }
    }
}

lazy_static! {
    /// Per-hart local run queues, indexed by hart id.
    static ref RUN_QUEUES: [SpinNoIrqLock<RunQueue>; MAX_HARTS] =
        array::from_fn(|_| SpinNoIrqLock::new(RunQueue::new()));

    /// PID2PCB instance (map of pid to pcb)
    pub static ref PID2PCB: SpinNoIrqLock<BTreeMap<usize, Arc<ProcessControlBlock>>> =
        SpinNoIrqLock::new(BTreeMap::new());
}

fn normalize_hart(hart: usize) -> usize {
    hart.min(MAX_HARTS.saturating_sub(1))
}

fn effective_affinity_mask(affinity_mask: usize) -> usize {
    let online = online_hart_mask();
    let online = if online != 0 {
        online
    } else {
        1usize << normalize_hart(hartid())
    };
    let effective = if affinity_mask == 0 {
        online
    } else {
        affinity_mask & online
    };
    if effective == 0 {
        online
    } else {
        effective
    }
}

fn select_target_hart(preferred_hart: usize, affinity_mask: usize, policy: SchedPolicy) -> usize {
    let affinity_mask = effective_affinity_mask(affinity_mask);
    let preferred_hart = normalize_hart(preferred_hart);
    if policy.is_rt() {
        if affinity_mask & (1usize << preferred_hart) != 0 {
            return preferred_hart;
        }
        return affinity_mask.trailing_zeros() as usize;
    }

    if affinity_mask & (1usize << preferred_hart) != 0
        && RUN_QUEUES[preferred_hart].lock().total_nr_running() == 0
    {
        return preferred_hart;
    }

    let mut best_hart = affinity_mask.trailing_zeros() as usize;
    let mut best_score = RUN_QUEUES[best_hart].lock().cfs_load_score();
    for hart in 0..MAX_HARTS {
        if affinity_mask & (1usize << hart) == 0 {
            continue;
        }
        let score = RUN_QUEUES[hart].lock().cfs_load_score();
        if score < best_score || (score == best_score && hart == preferred_hart) {
            best_hart = hart;
            best_score = score;
        }
    }
    best_hart
}

/// Request a reschedule on the selected hart, using an IPI when needed.
pub fn resched_hart(hart: usize) {
    let target_hart = normalize_hart(hart);
    if target_hart == hartid() {
        request_current_task_resched(ReschedReason::Migration);
        return;
    }
    send_ipi_mask(1usize << target_hart);
}

fn preempt_reason_for_current(
    current_policy: SchedPolicy,
    current_rt_priority: u8,
    current_vruntime_ns: u64,
    incoming: EnqueuedTaskInfo,
) -> Option<ReschedReason> {
    match (current_policy, incoming.policy) {
        (current, incoming_policy) if current.is_rt() && incoming_policy.is_rt() => {
            (incoming.rt_priority > current_rt_priority).then_some(ReschedReason::HigherRtPriority)
        }
        (SchedPolicy::Other, incoming) if incoming.is_rt() => Some(ReschedReason::HigherRtPriority),
        (SchedPolicy::Other, SchedPolicy::Other) => (incoming
            .vruntime_ns
            .saturating_add(CFS_WAKEUP_GRANULARITY_NS)
            < current_vruntime_ns)
            .then_some(ReschedReason::CfsPreempt),
        _ => None,
    }
}

fn maybe_preempt_current_on_this_hart(incoming: EnqueuedTaskInfo) {
    let Some(task) = current_task() else {
        return;
    };
    let task_inner = task.inner_exclusive_access();
    if !task_inner.sched.on_cpu || !matches!(task_inner.task_status, TaskStatus::Running) {
        return;
    }
    let reason = preempt_reason_for_current(
        task_inner.sched.policy,
        task_inner.sched.rt_priority,
        task_inner.sched.vruntime_ns,
        incoming,
    );
    drop(task_inner);
    if let Some(reason) = reason {
        request_current_task_resched(reason);
    }
}

fn notify_enqueued_task(target_hart: usize, incoming: EnqueuedTaskInfo) {
    if target_hart == hartid() {
        maybe_preempt_current_on_this_hart(incoming);
    } else {
        resched_hart(target_hart);
    }
}

/// Returns whether this hart already has runnable RT work at or above `prio`.
pub fn has_runnable_task_at_or_above(hart: usize, prio: u8) -> bool {
    RUN_QUEUES[normalize_hart(hart)]
        .lock()
        .has_same_or_higher_rt(prio)
}

/// Returns the highest runnable RT priority on the selected hart.
pub fn highest_runnable_prio(hart: usize) -> Option<u8> {
    RUN_QUEUES[normalize_hart(hart)].lock().highest_rt_prio()
}

/// Return whether CFS should preempt the current task on `hart`.
pub fn cfs_should_preempt(
    hart: usize,
    current_vruntime_ns: u64,
    current_weight: u64,
    current_slice_exec_ns: u64,
) -> bool {
    let rq = RUN_QUEUES[normalize_hart(hart)].lock();
    if rq.highest_rt_prio().is_some() {
        return true;
    }
    let Some(leftmost_vruntime) = rq.leftmost_cfs_vruntime() else {
        return false;
    };
    if current_vruntime_ns <= leftmost_vruntime.saturating_add(CFS_WAKEUP_GRANULARITY_NS) {
        return false;
    }
    let runnable = rq.cfs_nr_running.saturating_add(1);
    let period = if (runnable as u64) * crate::sched::CFS_MIN_GRANULARITY_NS
        > crate::sched::CFS_TARGET_LATENCY_NS
    {
        (runnable as u64) * crate::sched::CFS_MIN_GRANULARITY_NS
    } else {
        crate::sched::CFS_TARGET_LATENCY_NS
    };
    let total_load = rq.cfs_load.saturating_add(current_weight).max(1);
    let ideal_runtime = (period as u128)
        .saturating_mul(current_weight as u128)
        .checked_div(total_load as u128)
        .unwrap_or(0) as u64;
    current_slice_exec_ns >= ideal_runtime.max(crate::sched::CFS_MIN_GRANULARITY_NS)
}

/// Add a task to the scheduler on the current hart.
pub(crate) fn add_task(task: Arc<TaskControlBlock>) {
    enqueue_task_on(task, hartid());
}

/// Add a task to a specific hart's runqueue.
pub fn enqueue_task_on(task: Arc<TaskControlBlock>, hart: usize) {
    let (affinity_mask, policy) = {
        let task_inner = task.inner_exclusive_access();
        if task_inner.sched.on_rq
            || task_inner.sched.on_cpu
            || matches!(task_inner.task_status, TaskStatus::Zombie)
        {
            return;
        }
        (task_inner.sched.cpu_affinity_mask, task_inner.sched.policy)
    };
    let target_hart = select_target_hart(hart, affinity_mask, policy);
    let incoming = {
        let mut rq = RUN_QUEUES[target_hart].lock();
        let mut task_inner = task.inner_exclusive_access();
        if task_inner.sched.on_rq
            || task_inner.sched.on_cpu
            || matches!(task_inner.task_status, TaskStatus::Zombie)
        {
            return;
        }
        task_inner.task_status = TaskStatus::Runnable;
        task_inner.wait_reason = None;
        task_inner.sched.last_cpu = target_hart;
        task_inner.sched.on_rq = true;
        rq.enqueue_locked(Arc::clone(&task), &mut task_inner)
    };
    notify_enqueued_task(target_hart, incoming);
}

fn enqueue_wakeup_task(task: Arc<TaskControlBlock>, target_hart: usize) -> bool {
    let incoming = {
        let mut rq = RUN_QUEUES[target_hart].lock();
        let mut task_inner = task.inner_exclusive_access();
        let pid = task
            .process
            .upgrade()
            .map(|process| process.getpid())
            .unwrap_or(usize::MAX);
        match task_inner.task_status {
            TaskStatus::Interruptible | TaskStatus::Uninterruptible => {}
            TaskStatus::Running | TaskStatus::Runnable | TaskStatus::Zombie => return true,
        }
        if task_inner.sched.on_rq || task_inner.sched.on_cpu {
            debug!(
                "enqueue_wakeup_task: pid={} target_hart={} already on_cpu={} on_rq={}, mark runnable only",
                pid,
                target_hart,
                task_inner.sched.on_cpu,
                task_inner.sched.on_rq
            );
            task_inner.task_status = TaskStatus::Runnable;
            task_inner.wait_reason = None;
            task_inner.current_wq_handle = None;
            return true;
        }
        debug!(
            "enqueue_wakeup_task: pid={} target_hart={} enqueue from status={:?} last_cpu={} wait_reason={:?}",
            pid,
            target_hart,
            task_inner.task_status,
            task_inner.sched.last_cpu,
            task_inner.wait_reason
        );
        task_inner.task_status = TaskStatus::Runnable;
        task_inner.wait_reason = None;
        task_inner.current_wq_handle = None;
        if matches!(task_inner.sched.policy, SchedPolicy::Rr) {
            task_inner.reset_time_slice();
        }
        task_inner.sched.on_rq = true;
        task_inner.sched.last_cpu = target_hart;
        rq.enqueue_locked(Arc::clone(&task), &mut task_inner)
    };
    notify_enqueued_task(target_hart, incoming);
    true
}

fn wake_running_or_queued_task(task: &Arc<TaskControlBlock>) -> bool {
    {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.task_status = TaskStatus::Runnable;
        task_inner.wait_reason = None;
        task_inner.current_wq_handle = None;
        if matches!(task_inner.sched.policy, SchedPolicy::Rr) {
            task_inner.reset_time_slice();
        }
    }
    true
}

/// Pop one runnable task from the selected hart's runqueue.
fn dequeue_task(hart: usize) -> Option<Arc<TaskControlBlock>> {
    let hart = normalize_hart(hart);
    loop {
        let task = {
            let mut rq = RUN_QUEUES[hart].lock();
            rq.dequeue_highest_rt()
                .or_else(|| rq.dequeue_leftmost_cfs())
        }?;
        let mut task_inner = task.inner_exclusive_access();
        task_inner.sched.on_rq = false;
        if matches!(task_inner.task_status, TaskStatus::Runnable) {
            task_inner.sched.on_cpu = true;
            task_inner.sched.last_cpu = hart;
            drop(task_inner);
            return Some(task);
        }
    }
}

fn steal_cfs_task(target_hart: usize) -> Option<Arc<TaskControlBlock>> {
    let target_bit = 1usize << normalize_hart(target_hart);
    for source_hart in 0..MAX_HARTS {
        if source_hart == target_hart {
            continue;
        }
        let maybe_task = {
            let mut source_rq = RUN_QUEUES[source_hart].lock();
            let key = source_rq
                .cfs_tasks
                .iter()
                .find(|(_, task)| {
                    task.inner_exclusive_access().sched.cpu_affinity_mask & target_bit != 0
                })
                .map(|(key, _)| *key);
            key.and_then(|key| source_rq.remove_cfs_by_key(key))
        };
        if let Some(task) = maybe_task {
            let target_min = RUN_QUEUES[normalize_hart(target_hart)]
                .lock()
                .min_vruntime_ns;
            let mut task_inner = task.inner_exclusive_access();
            task_inner.sched.on_rq = false;
            if !matches!(task_inner.task_status, TaskStatus::Runnable) {
                continue;
            }
            task_inner.sched.on_cpu = true;
            task_inner.sched.last_cpu = target_hart;
            task_inner.sched.vruntime_ns = task_inner.sched.vruntime_ns.max(target_min);
            drop(task_inner);
            return Some(task);
        }
    }
    None
}

/// Pick the next task for the selected hart.
pub(crate) fn pick_next_task(hart: usize) -> Option<Arc<TaskControlBlock>> {
    dequeue_task(hart).or_else(|| steal_cfs_task(normalize_hart(hart)))
}

/// Wake up a sleeping task and place it on its target hart runqueue.
pub fn wakeup_task(task: Arc<TaskControlBlock>) -> bool {
    let wake_target = {
        let mut task_inner = task.inner_exclusive_access();
        let pid = task
            .process
            .upgrade()
            .map(|process| process.getpid())
            .unwrap_or(usize::MAX);
        debug!(
            "wakeup_task: pid={} status={:?} wait_reason={:?} on_cpu={} on_rq={} last_cpu={}",
            pid,
            task_inner.task_status,
            task_inner.wait_reason,
            task_inner.sched.on_cpu,
            task_inner.sched.on_rq,
            task_inner.sched.last_cpu
        );
        match task_inner.task_status {
            TaskStatus::Interruptible | TaskStatus::Uninterruptible => {
                if task_inner.sched.on_rq {
                    task_inner.task_status = TaskStatus::Runnable;
                    task_inner.wait_reason = None;
                    task_inner.current_wq_handle = None;
                    if matches!(task_inner.sched.policy, SchedPolicy::Rr) {
                        task_inner.reset_time_slice();
                    }
                    return true;
                }
                if task_inner.sched.on_cpu {
                    let last_cpu = normalize_hart(task_inner.sched.last_cpu);
                    let is_still_current = processor_for_hart(last_cpu)
                        .lock()
                        .current()
                        .is_some_and(|current| Arc::ptr_eq(&current, &task));
                    if is_still_current {
                        drop(task_inner);
                        return wake_running_or_queued_task(&task);
                    }
                    task_inner.sched.on_cpu = false;
                    Some((
                        last_cpu,
                        task_inner.sched.cpu_affinity_mask,
                        task_inner.sched.policy,
                    ))
                } else {
                    Some((
                        task_inner.sched.last_cpu,
                        task_inner.sched.cpu_affinity_mask,
                        task_inner.sched.policy,
                    ))
                }
            }
            TaskStatus::Running | TaskStatus::Runnable | TaskStatus::Zombie => return true,
        }
    };
    if let Some((preferred_hart, affinity_mask, policy)) = wake_target {
        let target_hart = select_target_hart(preferred_hart, affinity_mask, policy);
        return enqueue_wakeup_task(task, target_hart);
    }
    true
}

/// Remove a task from all local runqueues.
pub fn remove_task(task: Arc<TaskControlBlock>) {
    for rq in RUN_QUEUES.iter() {
        if rq.lock().remove_task(&task) {
            break;
        }
    }
    let mut task_inner = task.inner_exclusive_access();
    task_inner.sched.on_rq = false;
    task_inner.sched.cfs_rq_key = None;
}

/// Set a task to stop-wait status on the current hart, keeping its kernel
/// stack alive until the next context switch on this hart.
pub fn add_stopping_task(task: Arc<TaskControlBlock>) {
    let hart = hartid();
    RUN_QUEUES[hart].lock().stop_task = Some(task);
}

/// Get process by pid.
pub fn pid2process(pid: usize) -> Option<Arc<ProcessControlBlock>> {
    let map = (*PID2PCB).lock();
    map.get(&pid).map(Arc::clone)
}

/// List all current process IDs.
pub fn list_pids() -> Vec<usize> {
    let map = PID2PCB.lock();
    map.keys().copied().collect()
}

/// Insert item(pid, pcb) into PID2PCB map.
pub fn insert_into_pid2process(pid: usize, process: Arc<ProcessControlBlock>) {
    (*PID2PCB).lock().insert(pid, process);
}

/// Remove item(pid, _some_pcb) from PID2PCB map.
pub fn remove_from_pid2process(pid: usize) {
    let mut map = (*PID2PCB).lock();
    if map.remove(&pid).is_none() {
        panic!("cannot find pid {} in pid2task!", pid);
    }
}
