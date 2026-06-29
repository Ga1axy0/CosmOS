//! Per-hart runqueue management for RT and CFS scheduling classes.

use super::{current_task, processor::processor_for_hart};
use crate::config::MAX_HARTS;
use crate::hal::hartid;
use crate::mm::online_mask as online_hart_mask;
use crate::sbi::send_ipi_mask;
use crate::sched::{request_current_task_resched, CFS_WAKEUP_GRANULARITY_NS};
use crate::sync::SpinNoIrqLock;
use crate::task::{
    ProcessControlBlock, ReschedReason, SchedPolicy, TaskControlBlock, TaskControlBlockInner,
    TaskStatus, SCHED_RT_PRIO_MAX,
};
use crate::timer::get_time_ns;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::array;
use core::sync::atomic::Ordering;
use lazy_static::*;

const RT_QUEUE_LEVELS: usize = SCHED_RT_PRIO_MAX as usize + 1;
const RT_SLEEP_CFS_BOOST_MIN_RUNNING: usize = 128;
const RT_SLEEP_CFS_BOOST_MAX_TASKS: usize = 8;
type CfsKey = (u64, usize);

#[derive(Copy, Clone)]
struct EnqueuedTaskInfo {
    policy: SchedPolicy,
    rt_priority: u8,
    vruntime_ns: u64,
}

fn running_cfs_vruntime_snapshot(hart: usize) -> Option<u64> {
    let task = processor_for_hart(normalize_hart(hart)).lock().current()?;
    let mut task_inner = task.inner_exclusive_access();
    if !task.on_cpu.load(Ordering::Relaxed)
        || !matches!(task_inner.task_status, TaskStatus::Running)
        || !matches!(task_inner.sched.policy, SchedPolicy::Other)
    {
        return None;
    }
    task_inner.account_cfs_runtime(get_time_ns());
    Some(task_inner.sched.vruntime_ns)
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

    /// Raw pointer identities of every runnable task in this runqueue
    /// (all RT levels + the CFS tree). `stop_task` is intentionally excluded:
    /// it is a dying-task reference held for kernel-stack safety, not a
    /// runnable entry. Debug invariant checker only.
    #[cfg(feature = "sched_invariant_checks")]
    pub(super) fn runnable_ptrs(&self) -> Vec<usize> {
        let mut v = Vec::new();
        for q in self.rt_queues.iter() {
            for t in q.iter() {
                v.push(Arc::as_ptr(t) as usize);
            }
        }
        for t in self.cfs_tasks.values() {
            v.push(Arc::as_ptr(t) as usize);
        }
        v
    }

    fn enqueue_locked(
        &mut self,
        task: Arc<TaskControlBlock>,
        task_inner: &mut TaskControlBlockInner,
        current_vruntime_hint: Option<u64>,
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
                    current_vruntime_hint,
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

    fn place_cfs_entity(
        &self,
        vruntime_ns: u64,
        initialized: bool,
        current_vruntime_hint: Option<u64>,
    ) -> (u64, bool) {
        let effective_min_vruntime = match current_vruntime_hint {
            Some(current_vruntime) if self.cfs_nr_running == 0 => current_vruntime,
            Some(current_vruntime) => self.min_vruntime_ns.min(current_vruntime),
            None => self.min_vruntime_ns,
        };
        if !initialized {
            return (effective_min_vruntime, true);
        }
        let sleeper_floor = effective_min_vruntime.saturating_sub(CFS_WAKEUP_GRANULARITY_NS);
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

    fn boost_process_cfs_tasks(&mut self, tasks: &[Arc<TaskControlBlock>]) -> usize {
        if self.cfs_nr_running < RT_SLEEP_CFS_BOOST_MIN_RUNNING {
            return 0;
        }
        let boosted_vruntime = self
            .min_vruntime_ns
            .saturating_sub(CFS_WAKEUP_GRANULARITY_NS);
        let mut boosted = 0usize;
        for task in tasks {
            if boosted >= RT_SLEEP_CFS_BOOST_MAX_TASKS {
                break;
            }
            let key = {
                let task_inner = task.inner_exclusive_access();
                if !matches!(task_inner.sched.policy, SchedPolicy::Other)
                    || !task_inner.sched.on_rq
                    || !matches!(task_inner.task_status, TaskStatus::Runnable)
                {
                    continue;
                }
                let Some(key) = task_inner.sched.cfs_rq_key else {
                    continue;
                };
                key
            };
            let Some(queued_task) = self.cfs_tasks.remove(&key) else {
                continue;
            };
            if !Arc::ptr_eq(&queued_task, task) {
                self.cfs_tasks.insert(key, queued_task);
                continue;
            }
            let new_key = {
                let mut task_inner = queued_task.inner_exclusive_access();
                task_inner.sched.vruntime_ns = boosted_vruntime;
                let new_key = (boosted_vruntime, Arc::as_ptr(&queued_task) as usize);
                task_inner.sched.cfs_rq_key = Some(new_key);
                new_key
            };
            self.cfs_tasks.insert(new_key, queued_task);
            boosted += 1;
        }
        if boosted != 0 {
            self.refresh_min_vruntime(None);
        }
        boosted
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
    let mut task_inner = task.inner_exclusive_access();
    if !task.on_cpu.load(Ordering::Relaxed)
        || !matches!(task_inner.task_status, TaskStatus::Running)
    {
        return;
    }
    let current_policy = task_inner.sched.policy;
    let current_rt_priority = task_inner.sched.rt_priority;
    // Wakeup preemption should compare against the current task's vruntime as
    // of "now", not the last tick or context-switch accounting point.
    if matches!(current_policy, SchedPolicy::Other) {
        task_inner.account_cfs_runtime(get_time_ns());
    }
    let current_vruntime_after = task_inner.sched.vruntime_ns;
    let reason = preempt_reason_for_current(
        current_policy,
        current_rt_priority,
        current_vruntime_after,
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

pub(crate) fn boost_process_cfs_tasks(hart: usize, tasks: &[Arc<TaskControlBlock>]) -> usize {
    RUN_QUEUES[normalize_hart(hart)]
        .lock()
        .boost_process_cfs_tasks(tasks)
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
            || task.on_cpu.load(Ordering::Relaxed)
            || matches!(task_inner.task_status, TaskStatus::Zombie)
        {
            return;
        }
        (task_inner.sched.cpu_affinity_mask, task_inner.sched.policy)
    };
    let target_hart = select_target_hart(hart, affinity_mask, policy);
    let current_vruntime_hint = running_cfs_vruntime_snapshot(target_hart);
    let incoming = {
        let mut rq = RUN_QUEUES[target_hart].lock();
        let mut task_inner = task.inner_exclusive_access();
        if task_inner.sched.on_rq
            || task.on_cpu.load(Ordering::Relaxed)
            || matches!(task_inner.task_status, TaskStatus::Zombie)
        {
            return;
        }
        task_inner.task_status = TaskStatus::Runnable;
        task_inner.wait_reason = None;
        task_inner.sched.last_cpu = target_hart;
        task_inner.sched.on_rq = true;
        rq.enqueue_locked(Arc::clone(&task), &mut task_inner, current_vruntime_hint)
    };
    notify_enqueued_task(target_hart, incoming);
}

fn enqueue_wakeup_task(task: Arc<TaskControlBlock>, target_hart: usize) -> bool {
    let current_vruntime_hint = running_cfs_vruntime_snapshot(target_hart);
    let incoming = {
        let mut rq = RUN_QUEUES[target_hart].lock();
        let mut task_inner = task.inner_exclusive_access();
        match task_inner.task_status {
            TaskStatus::Interruptible | TaskStatus::Uninterruptible => {}
            TaskStatus::Running | TaskStatus::Runnable | TaskStatus::Zombie => return true,
        }
        if task_inner.sched.on_rq || task.on_cpu.load(Ordering::Relaxed) {
            task_inner.task_status = TaskStatus::Runnable;
            task_inner.wait_reason = None;
            task_inner.current_wq_handle = None;
            return true;
        }
        task_inner.task_status = TaskStatus::Runnable;
        task_inner.wait_reason = None;
        task_inner.current_wq_handle = None;
        if matches!(task_inner.sched.policy, SchedPolicy::Rr) {
            task_inner.reset_time_slice();
        }
        task_inner.sched.on_rq = true;
        task_inner.sched.last_cpu = target_hart;
        rq.enqueue_locked(Arc::clone(&task), &mut task_inner, current_vruntime_hint)
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
            task.on_cpu.store(true, Ordering::Relaxed);
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
            task.on_cpu.store(true, Ordering::Relaxed);
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
                if task.on_cpu.load(Ordering::Relaxed) {
                    let last_cpu = normalize_hart(task_inner.sched.last_cpu);
                    let is_still_current = processor_for_hart(last_cpu)
                        .lock()
                        .current()
                        .is_some_and(|current| Arc::ptr_eq(&current, &task));
                    if is_still_current {
                        // Task is still the running task on `last_cpu`: just mark
                        // it Runnable without enqueueing. The running hart keeps it
                        // on-CPU (and if it is about to block, its
                        // `block_current_and_run_next` observes Runnable and skips
                        // the switch). Enqueueing a running task would corrupt it.
                        drop(task_inner);
                        return wake_running_or_queued_task(&task);
                    }
                    // Transition window: `take_current_task()` already cleared
                    // `processor.current` on `last_cpu`, but the context switch is
                    // still in flight, so `on_cpu` is still true. It is cleared
                    // post-switch by `finish_pending_task_release`, *after* the
                    // task's registers are safely saved. Enqueueing now would let
                    // another hart `__switch` into a half-saved context — the
                    // confirmed SMP wake/block race. Snapshot the target fields
                    // (stable across the transition), drop the lock, and spin
                    // until the owning hart finishes the switch, then enqueue. The
                    // wait is bounded: `finish_pending_task_release` runs as the
                    // very next step after that hart returns to its idle loop.
                    let affinity_mask = task_inner.sched.cpu_affinity_mask;
                    let policy = task_inner.sched.policy;
                    drop(task_inner);
                    // Defensive tripwire. The deferred release of `on_cpu` is
                    // owned by `last_cpu`; if that is THIS hart, the only thing
                    // that can clear `on_cpu` (`finish_pending_task_release`,
                    // run when this hart next reaches its idle loop) cannot make
                    // progress while we execute here — so the spin below would
                    // never terminate. That is exactly the cyclictest
                    // self-deadlock: a timer hardirq on the owning hart woke the
                    // half-blocked task and spun on its still-set `on_cpu`. The
                    // block/suspend transition is now kept IRQ-atomic, which
                    // makes this state unreachable; panic loudly if it ever
                    // recurs so it is debuggable instead of a silent 100%-CPU
                    // hang.
                    if last_cpu == normalize_hart(hartid()) {
                        panic!(
                            "[sched] wakeup_task: task {:#x} is mid-block (on_cpu set) with \
                             last_cpu={} == this hart {} — the deferred `on_cpu` release cannot \
                             complete while we run here; this should be unreachable now that the \
                             block/suspend transition is IRQ-atomic",
                            Arc::as_ptr(&task) as usize,
                            last_cpu,
                            hartid(),
                        );
                    }
                    // Lock-free spin: pair with the `Release` store in
                    // `finish_pending_task_release`. Seeing on_cpu==false means
                    // the owning hart has finished saving this task's context, so
                    // it is safe for us to enqueue it (and for another hart to
                    // later switch into it).
                    while task.on_cpu.load(Ordering::Acquire) {
                        core::hint::spin_loop();
                    }
                    // `enqueue_wakeup_task` re-validates on_rq/on_cpu under the
                    // runqueue+task locks, so a task that was re-picked (on_cpu
                    // flipped back to true) or already woken by a rival (on_rq)
                    // during the spin is handled safely.
                    Some((last_cpu, affinity_mask, policy))
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

/// Debug-only scheduler invariant checker.
///
/// Snapshots, under all-at-once locking (every `PROCESSORS` then every
/// `RUN_QUEUES`, in hart order), the identity of each hart's current task and
/// every runnable task in every runqueue, then verifies three invariants whose
/// violation is the signature of the SMP wake/block race in
/// `block_current_and_run_next` / `wakeup_task`:
///
/// 0. A task is `current` on at most one hart (no double-run).
/// 1. No `current` task is simultaneously present in any runqueue
///    (running + on-rq).
/// 2. No task is enqueued twice across runqueues (double-enqueue / leaked
///    node after a raced block).
///
/// `SpinNoIrqLock` keeps SIE disabled on the calling hart for the whole
/// snapshot, so no task can move between containers while we observe, and the
/// caller cannot be preempted mid-scan. No task-inner locks are taken, so this
/// cannot deadlock against paths that take processor/runqueue locks.
#[cfg(feature = "sched_invariant_checks")]
pub(crate) fn check_sched_invariants() {
    use super::processor::PROCESSORS;

    let proc_guards: Vec<_> = (0..MAX_HARTS).map(|h| PROCESSORS[h].lock()).collect();
    let currents: Vec<Option<usize>> = (0..MAX_HARTS)
        .map(|h| proc_guards[h].current_ptr())
        .collect();

    let rq_guards: Vec<_> = (0..MAX_HARTS).map(|h| RUN_QUEUES[h].lock()).collect();
    // (owning_hart, task_ptr) for every runnable entry across all runqueues.
    let mut all_runnable: Vec<(usize, usize)> = Vec::new();
    for h in 0..MAX_HARTS {
        for ptr in rq_guards[h].runnable_ptrs() {
            all_runnable.push((h, ptr));
        }
    }
    drop(proc_guards);
    drop(rq_guards);

    // Invariant 0: a task is current on at most one hart.
    for a in 0..MAX_HARTS {
        let Some(pa) = currents[a] else {
            continue;
        };
        for b in (a + 1)..MAX_HARTS {
            if currents[b] == Some(pa) {
                panic!(
                    "[sched-inv] task {:#x} is current on BOTH hart {} and hart {} (double-run)",
                    pa, a, b
                );
            }
        }
    }

    // Invariant 1: no current task is present in any runqueue.
    for h in 0..MAX_HARTS {
        let Some(p) = currents[h] else {
            continue;
        };
        for (rh, ptr) in all_runnable.iter() {
            if *ptr == p {
                panic!(
                    "[sched-inv] task {:#x} is current on hart {} AND enqueued on runqueue {} \
                     (running + on-rq: a wakeup enqueued a task that was still current)",
                    p, h, rh
                );
            }
        }
    }

    // Invariant 2: no task is enqueued twice across runqueues.
    let n = all_runnable.len();
    for i in 0..n {
        for j in (i + 1)..n {
            if all_runnable[i].1 == all_runnable[j].1 {
                panic!(
                    "[sched-inv] task {:#x} enqueued TWICE (runqueue {} and runqueue {}) \
                     (double-enqueue / leaked node after a raced block)",
                    all_runnable[i].1, all_runnable[i].0, all_runnable[j].0
                );
            }
        }
    }
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

/// Drop the stopped-task reference for the current hart.
/// Called by the idle loop after `__switch` returns, once the previous
/// task's kernel stack is guaranteed unused.
pub fn clear_stopping_task() {
    let hart = hartid();
    RUN_QUEUES[hart].lock().stop_task = None;
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
