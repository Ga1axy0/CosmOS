//! Per-hart runqueue management for RT and the compiled-in fair scheduler.

use super::{current_task, processor::processor_for_hart};
use crate::config::MAX_HARTS;
use crate::hal::hartid;
use crate::mm::online_mask as online_hart_mask;
use crate::sbi::send_ipi_mask;
#[cfg(feature = "sched_eevdf")]
use crate::sched::{eevdf_virtual_deadline, EEVDF_DEFAULT_SLICE_NS, EEVDF_WAKEUP_GRANULARITY_NS};
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
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::*;

const RT_QUEUE_LEVELS: usize = SCHED_RT_PRIO_MAX as usize + 1;
const RT_SLEEP_CFS_BOOST_MIN_RUNNING: usize = 128;
const RT_SLEEP_CFS_BOOST_MAX_TASKS: usize = 8;
type CfsKey = (u64, usize);

#[derive(Copy, Clone)]
struct EnqueuedTaskInfo {
    policy: SchedPolicy,
    rt_priority: u8,
    #[cfg(not(feature = "sched_eevdf"))]
    vruntime_ns: u64,
    #[cfg(feature = "sched_eevdf")]
    virtual_deadline_ns: u64,
    #[cfg(feature = "sched_eevdf")]
    eligible: bool,
}

fn running_fair_entity_snapshot(hart: usize) -> Option<(u64, u64)> {
    let task = processor_for_hart(normalize_hart(hart)).lock().current()?;
    let mut task_inner = task.inner_exclusive_access();
    if !task.on_cpu.load(Ordering::Relaxed)
        || !matches!(task_inner.task_status, TaskStatus::Running)
        || !matches!(task_inner.sched.policy, SchedPolicy::Other)
    {
        return None;
    }
    task_inner.account_cfs_runtime(get_time_ns());
    Some((task_inner.sched.vruntime_ns, task_inner.sched.weight.max(1)))
}

/// Local runnable queues owned by one hart.
struct RunQueue {
    rt_queues: [VecDeque<Arc<TaskControlBlock>>; RT_QUEUE_LEVELS],
    highest_rt_prio: Option<u8>,
    rt_nr_running: usize,
    cfs_tasks: BTreeMap<CfsKey, Arc<TaskControlBlock>>,
    cfs_nr_running: usize,
    cfs_load: u64,
    /// Cached EEVDF numerator and denominator for the queued fair entities.
    ///
    /// The running entity is intentionally excluded because it is not in
    /// `cfs_tasks`; callers add it as a temporary argument when calculating
    /// eligibility. Keeping this summary under the runqueue lock avoids an
    /// O(n) scan and per-task lock acquisition on every scheduler decision.
    #[cfg(feature = "sched_eevdf")]
    eevdf_weighted_vruntime_ns: u128,
    #[cfg(feature = "sched_eevdf")]
    eevdf_total_weight: u128,
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
            #[cfg(feature = "sched_eevdf")]
            eevdf_weighted_vruntime_ns: 0,
            #[cfg(feature = "sched_eevdf")]
            eevdf_total_weight: 0,
            min_vruntime_ns: 0,
            stop_task: None,
        }
    }

    /// Raw pointer identities of every runnable task in this runqueue
    /// (all RT levels + the fair tree). `stop_task` is intentionally excluded:
    /// it is a dying-task reference held for kernel-stack safety, not a
    /// runnable entry. Used by scheduler diagnostics.
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
        current_entity_hint: Option<(u64, u64)>,
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
                    #[cfg(not(feature = "sched_eevdf"))]
                    vruntime_ns: 0,
                    #[cfg(feature = "sched_eevdf")]
                    virtual_deadline_ns: 0,
                    #[cfg(feature = "sched_eevdf")]
                    eligible: false,
                }
            }
            SchedPolicy::Other => {
                #[cfg(feature = "sched_eevdf")]
                let queue_was_empty = self.cfs_nr_running == 0;
                let (placed_vruntime, initialized) = self.place_cfs_entity(
                    task_inner.sched.vruntime_ns,
                    task_inner.sched.cfs_initialized,
                    current_entity_hint.map(|(vruntime, _)| vruntime),
                );
                task_inner.sched.vruntime_ns = placed_vruntime;
                task_inner.sched.cfs_initialized = initialized;
                let vruntime = task_inner.sched.vruntime_ns;
                let weight = task_inner.sched.weight;
                #[cfg(feature = "sched_eevdf")]
                let virtual_deadline =
                    eevdf_virtual_deadline(vruntime, weight, EEVDF_DEFAULT_SLICE_NS);
                #[cfg(not(feature = "sched_eevdf"))]
                let virtual_deadline = 0;
                #[cfg(feature = "sched_eevdf")]
                let key = (virtual_deadline, Arc::as_ptr(&task) as usize);
                #[cfg(not(feature = "sched_eevdf"))]
                let key = (vruntime, Arc::as_ptr(&task) as usize);
                #[cfg(feature = "sched_eevdf")]
                let eligible = task_inner.sched.vruntime_ns
                    <= self.eevdf_average_vruntime_with_entity(
                        current_entity_hint,
                        Some((vruntime, weight)),
                    );
                task_inner.sched.cfs_rq_key = Some(key);
                task_inner.sched.eevdf_deadline_ns = virtual_deadline;
                self.cfs_tasks.insert(key, task);
                self.cfs_nr_running += 1;
                self.cfs_load = self.cfs_load.saturating_add(weight);
                #[cfg(feature = "sched_eevdf")]
                {
                    self.eevdf_add_entity(vruntime, weight);
                    // A newly-created fair queue has no leftmost entry for
                    // refresh_min_vruntime() to inspect. Publish its first
                    // entity directly instead of scanning the queue.
                    if queue_was_empty {
                        self.min_vruntime_ns = self.min_vruntime_ns.max(vruntime);
                    }
                }
                EnqueuedTaskInfo {
                    policy: SchedPolicy::Other,
                    rt_priority: 0,
                    #[cfg(not(feature = "sched_eevdf"))]
                    vruntime_ns: vruntime,
                    #[cfg(feature = "sched_eevdf")]
                    virtual_deadline_ns: virtual_deadline,
                    #[cfg(feature = "sched_eevdf")]
                    eligible,
                }
            }
            SchedPolicy::Idle => unreachable!("idle tasks are not enqueued"),
        }
    }

    /// Add one queued fair entity to the cached EEVDF lag summary.
    #[cfg(feature = "sched_eevdf")]
    #[inline]
    fn eevdf_add_entity(&mut self, vruntime_ns: u64, weight: u64) {
        let weight = weight.max(1) as u128;
        self.eevdf_weighted_vruntime_ns = self
            .eevdf_weighted_vruntime_ns
            .saturating_add((vruntime_ns as u128).saturating_mul(weight));
        self.eevdf_total_weight = self.eevdf_total_weight.saturating_add(weight);
    }

    /// Remove one queued fair entity from the cached EEVDF lag summary.
    #[cfg(feature = "sched_eevdf")]
    #[inline]
    fn eevdf_remove_entity(&mut self, vruntime_ns: u64, weight: u64) {
        let weight = weight.max(1) as u128;
        self.eevdf_weighted_vruntime_ns = self
            .eevdf_weighted_vruntime_ns
            .saturating_sub((vruntime_ns as u128).saturating_mul(weight));
        self.eevdf_total_weight = self.eevdf_total_weight.saturating_sub(weight);
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

    /// Return the weighted average virtual runtime used by EEVDF's lag test.
    ///
    /// The currently running fair task is not in `cfs_tasks`, so callers pass
    /// its `(vruntime, weight)` separately when it should participate in the
    /// eligibility calculation. The queued portion comes from the cached
    /// summary above; this must stay O(1) because it is called from both the
    /// timer-tick and dequeue paths.
    #[cfg(feature = "sched_eevdf")]
    fn eevdf_average_vruntime(&self, current: Option<(u64, u64)>) -> u64 {
        self.eevdf_average_vruntime_with_entity(current, None)
    }

    /// Return the weighted average virtual runtime while also accounting for
    /// entities that are not part of the cached queue summary yet.
    #[cfg(feature = "sched_eevdf")]
    fn eevdf_average_vruntime_with_entity(
        &self,
        current: Option<(u64, u64)>,
        incoming: Option<(u64, u64)>,
    ) -> u64 {
        let mut weighted_vruntime = self.eevdf_weighted_vruntime_ns;
        let mut total_weight = self.eevdf_total_weight;
        if let Some((vruntime, weight)) = incoming {
            let weight = weight.max(1) as u128;
            weighted_vruntime =
                weighted_vruntime.saturating_add((vruntime as u128).saturating_mul(weight));
            total_weight = total_weight.saturating_add(weight);
        }
        if let Some((vruntime, weight)) = current {
            let weight = weight.max(1) as u128;
            weighted_vruntime =
                weighted_vruntime.saturating_add((vruntime as u128).saturating_mul(weight));
            total_weight = total_weight.saturating_add(weight);
        }
        if total_weight == 0 {
            return self.min_vruntime_ns;
        }
        let average = weighted_vruntime / total_weight;
        average.min(u64::MAX as u128) as u64
    }

    /// Return the earliest virtual deadline among currently eligible EEVDF
    /// tasks. The tree is deadline ordered, so the first eligible entry is
    /// the next candidate.
    #[cfg(feature = "sched_eevdf")]
    fn eevdf_earliest_eligible_deadline(&self, current: Option<(u64, u64)>) -> Option<u64> {
        let average_vruntime = self.eevdf_average_vruntime(current);
        self.cfs_tasks.iter().find_map(|(key, task)| {
            let task_inner = task.inner_exclusive_access();
            (task_inner.sched.vruntime_ns <= average_vruntime).then_some(key.0)
        })
    }

    fn dequeue_highest_rt(&mut self) -> Option<Arc<TaskControlBlock>> {
        let prio = self.highest_rt_prio?;
        let task = self.rt_queues[prio as usize].pop_front()?;
        self.rt_nr_running = self.rt_nr_running.saturating_sub(1);
        self.refresh_highest_rt_prio();
        Some(task)
    }

    fn dequeue_leftmost_cfs(&mut self) -> Option<Arc<TaskControlBlock>> {
        #[cfg(feature = "sched_eevdf")]
        {
            let average_vruntime = self.eevdf_average_vruntime(None);
            let key = self
                .cfs_tasks
                .iter()
                .find(|(_, task)| {
                    task.inner_exclusive_access().sched.vruntime_ns <= average_vruntime
                })
                .map(|(key, _)| *key)
                .or_else(|| self.cfs_tasks.keys().next().copied())?;
            return self.remove_cfs_by_key(key);
        }
        #[cfg(not(feature = "sched_eevdf"))]
        {
            let key = *self.cfs_tasks.keys().next()?;
            self.remove_cfs_by_key(key)
        }
    }

    fn remove_cfs_by_key(&mut self, key: CfsKey) -> Option<Arc<TaskControlBlock>> {
        let task = self.cfs_tasks.remove(&key)?;
        let accounted_vruntime = {
            let mut task_inner = task.inner_exclusive_access();
            task_inner.sched.cfs_rq_key = None;
            task_inner.sched.eevdf_deadline_ns = 0;
            let weight = task_inner.sched.weight;
            let vruntime = task_inner.sched.vruntime_ns;
            #[cfg(feature = "sched_eevdf")]
            self.eevdf_remove_entity(vruntime, weight);
            self.cfs_load = self.cfs_load.saturating_sub(weight);
            vruntime
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
            #[cfg(feature = "sched_eevdf")]
            let old_entity = {
                let task_inner = queued_task.inner_exclusive_access();
                (task_inner.sched.vruntime_ns, task_inner.sched.weight)
            };
            #[cfg(feature = "sched_eevdf")]
            self.eevdf_remove_entity(old_entity.0, old_entity.1);
            let new_key = {
                let mut task_inner = queued_task.inner_exclusive_access();
                task_inner.sched.vruntime_ns = boosted_vruntime;
                #[cfg(feature = "sched_eevdf")]
                let virtual_deadline = eevdf_virtual_deadline(
                    boosted_vruntime,
                    task_inner.sched.weight,
                    EEVDF_DEFAULT_SLICE_NS,
                );
                #[cfg(not(feature = "sched_eevdf"))]
                let virtual_deadline = 0;
                #[cfg(feature = "sched_eevdf")]
                let new_key = (virtual_deadline, Arc::as_ptr(&queued_task) as usize);
                #[cfg(not(feature = "sched_eevdf"))]
                let new_key = (boosted_vruntime, Arc::as_ptr(&queued_task) as usize);
                task_inner.sched.cfs_rq_key = Some(new_key);
                task_inner.sched.eevdf_deadline_ns = virtual_deadline;
                new_key
            };
            self.cfs_tasks.insert(new_key, queued_task);
            #[cfg(feature = "sched_eevdf")]
            self.eevdf_add_entity(boosted_vruntime, old_entity.1);
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

    #[cfg(not(feature = "sched_eevdf"))]
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
        #[cfg(not(feature = "sched_eevdf"))]
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

static LOST_RUNNABLE_SCAN_COUNT: AtomicUsize = AtomicUsize::new(0);
static LOST_RUNNABLE_ERROR_COUNT: AtomicUsize = AtomicUsize::new(0);
static LOST_RUNNABLE_REPAIR_COUNT: AtomicUsize = AtomicUsize::new(0);
static LOST_RUNNABLE_SELF_HEAL_COUNT: AtomicUsize = AtomicUsize::new(0);

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
    _current_weight: u64,
    incoming: EnqueuedTaskInfo,
) -> Option<ReschedReason> {
    match (current_policy, incoming.policy) {
        (current, incoming_policy) if current.is_rt() && incoming_policy.is_rt() => {
            (incoming.rt_priority > current_rt_priority).then_some(ReschedReason::HigherRtPriority)
        }
        (SchedPolicy::Other, incoming) if incoming.is_rt() => Some(ReschedReason::HigherRtPriority),
        (SchedPolicy::Other, SchedPolicy::Other) => {
            #[cfg(feature = "sched_eevdf")]
            {
                let current_deadline = eevdf_virtual_deadline(
                    current_vruntime_ns,
                    _current_weight,
                    EEVDF_DEFAULT_SLICE_NS,
                );
                (incoming.eligible
                    && incoming
                        .virtual_deadline_ns
                        .saturating_add(EEVDF_WAKEUP_GRANULARITY_NS)
                        < current_deadline)
                    .then_some(ReschedReason::CfsPreempt)
            }
            #[cfg(not(feature = "sched_eevdf"))]
            {
                (incoming
                    .vruntime_ns
                    .saturating_add(CFS_WAKEUP_GRANULARITY_NS)
                    < current_vruntime_ns)
                    .then_some(ReschedReason::CfsPreempt)
            }
        }
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
    let current_weight = task_inner.sched.weight;
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
        current_weight,
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

fn should_log_sched_sample(count: usize) -> bool {
    count <= 16 || count.is_power_of_two()
}

static ENQUEUE_SKIP_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

fn task_is_current_on_any_hart(task: &Arc<TaskControlBlock>) -> bool {
    (0..MAX_HARTS).any(|hart| {
        processor_for_hart(hart)
            .lock()
            .current()
            .is_some_and(|current| Arc::ptr_eq(&current, task))
    })
}

/// WARN diagnostics for tasks that are marked runnable but are not owned by
/// any scheduler container.
pub(crate) fn warn_lost_runnable_tasks(reason: &'static str) {
    if reason == "idle_no_task" {
        if normalize_hart(hartid()) != 0 {
            return;
        }
        let scan_count = LOST_RUNNABLE_SCAN_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if scan_count & 0x3f != 0 {
            return;
        }
    }

    // Snapshot each hart's processor/runqueue state in SHORT per-hart critical
    // sections rather than holding all locks at once. Each `SpinNoIrqLock`
    // disables local interrupts while held; locking every processor and runqueue
    // simultaneously keeps THIS hart's interrupts off for the entire scan, which
    // blocks any in-flight global TLB shootdown IPI from being serviced here —
    // observed to wedge the whole machine when this scan ran from the timer
    // tick. Lock/drop per hart keeps each IRQs-off window tiny, so a pending
    // shootdown IPI is handled between snapshots.
    let current_ptrs: Vec<usize> = (0..MAX_HARTS)
        .filter_map(|h| processor_for_hart(h).lock().current_ptr())
        .collect();

    let mut runnable_ptrs = Vec::new();
    let rq_snapshot: Vec<(usize, usize, usize, Option<u8>)> = (0..MAX_HARTS)
        .map(|h| {
            let rq = RUN_QUEUES[h].lock();
            runnable_ptrs.extend(rq.runnable_ptrs());
            (h, rq.rt_nr_running, rq.cfs_nr_running, rq.highest_rt_prio)
        })
        .collect();

    let processes: Vec<(usize, Arc<ProcessControlBlock>)> = PID2PCB
        .lock()
        .iter()
        .map(|(pid, process)| (*pid, Arc::clone(process)))
        .collect();

    // Collect lost-runnable orphans here, then re-enqueue them after the scan so
    // `wakeup_task` (which takes task/runqueue locks) is never called while we
    // hold process/task locks.
    let mut orphans: Vec<Arc<TaskControlBlock>> = Vec::new();

    for (pid, process) in processes {
        // Do not nest process-inner -> task-inner here.  exec rebuilds the
        // current task's user resources by briefly taking task-inner and then
        // process-inner; retaining the process lock while waiting for that task
        // produces an AB-BA deadlock.  Snapshot the stable Arc references and
        // process metadata, then inspect every task after dropping the process
        // lock.
        let (pgid, process_zombie, exec_path, tasks) = {
            let process_inner = process.inner_exclusive_access();
            let tasks: Vec<(usize, Arc<TaskControlBlock>)> = process_inner
                .tasks
                .iter()
                .enumerate()
                .filter_map(|(tid, task)| task.as_ref().map(|task| (tid, Arc::clone(task))))
                .collect();
            (
                process_inner.cred.pgid,
                process_inner.is_zombie,
                process_inner.exec_path.clone(),
                tasks,
            )
        };
        for (tid, task) in tasks {
            let task_ptr = Arc::as_ptr(&task) as usize;
            let task_inner = task.inner_exclusive_access();
            // Read `on_cpu` under the task-inner lock so it is consistent with
            // status/on_rq (every on_cpu writer holds this lock). A torn read
            // here would either miss a real orphan or cry wolf on a task that is
            // actually on-CPU, needlessly tripping the self-heal.
            let on_cpu = task.on_cpu.load(Ordering::Relaxed);
            if !matches!(task_inner.task_status, TaskStatus::Runnable)
                || on_cpu
                || task_inner.sched.on_rq
                || current_ptrs.contains(&task_ptr)
                || runnable_ptrs.contains(&task_ptr)
            {
                continue;
            }

            // Confirmed lost-runnable orphan: collect it for self-heal below.
            orphans.push(Arc::clone(&task));

            let thread_id = task_inner.res.as_ref().map(|res| res.thread_id);

            let count = LOST_RUNNABLE_ERROR_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if !should_log_sched_sample(count) {
                continue;
            }

            error!(
                "[sched-inv][lost-runnable] reason={} count={} scan_hart={} task={:#x} pid={} \
                 tid={} thread_id={:?} pgid={} exec={} process_zombie={} wait={:?} last_cpu={} \
                 policy={:?} on_cpu={} on_rq={} has_wq={} task_pending={:#x} \
                 mask={:#x} resched={:?} currents={:?} rq_snapshot={:?} \
                 last_sched_op={:?}",
                reason,
                count,
                hartid(),
                task_ptr,
                pid,
                tid,
                thread_id,
                pgid,
                exec_path,
                process_zombie,
                task_inner.wait_reason,
                task_inner.sched.last_cpu,
                task_inner.sched.policy,
                on_cpu,
                task_inner.sched.on_rq,
                task_inner.current_wq_handle.is_some(),
                task_inner.pending_signals.bits(),
                task_inner.signal_mask.bits(),
                task_inner.sched.resched_reason,
                current_ptrs,
                rq_snapshot,
                task_inner.last_sched_op,
            );
        }
    }

    // Self-heal: re-enqueue every lost-runnable orphan collected above. A
    // Runnable task that is on no runqueue and no CPU is unreachable by the
    // normal wake path; `wakeup_task` routes it through `enqueue_wakeup_task`'s
    // repair branch, which places it back on a runqueue. This is a safety net
    // that downgrades a permanent lost-runnable hang into a brief stall.
    for orphan in orphans {
        let count = LOST_RUNNABLE_SELF_HEAL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 16 || count.is_power_of_two() {
            error!(
                "[sched-inv][self-heal] count={} task={:#x} — re-enqueueing lost-runnable orphan",
                count,
                Arc::as_ptr(&orphan) as usize
            );
        }
        wakeup_task(orphan);
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

/// Return whether the compiled-in fair scheduler should preempt the current
/// task on `hart`.
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
    #[cfg(feature = "sched_eevdf")]
    {
        if rq.cfs_nr_running == 0 || current_slice_exec_ns < EEVDF_DEFAULT_SLICE_NS {
            return false;
        }
        let Some(incoming_deadline) =
            rq.eevdf_earliest_eligible_deadline(Some((current_vruntime_ns, current_weight)))
        else {
            return false;
        };
        let current_deadline =
            eevdf_virtual_deadline(current_vruntime_ns, current_weight, EEVDF_DEFAULT_SLICE_NS);
        return incoming_deadline.saturating_add(EEVDF_WAKEUP_GRANULARITY_NS) < current_deadline;
    }
    #[cfg(not(feature = "sched_eevdf"))]
    {
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
    let current_entity_hint = running_fair_entity_snapshot(target_hart);
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
        let incoming = rq.enqueue_locked(Arc::clone(&task), &mut task_inner, current_entity_hint);
        incoming
    };
    // Preserve the CFS min-vruntime refresh. EEVDF maintains its first-entry
    // floor and cached lag summary during enqueue, so it needs no extra queue
    // scan or lock acquisition here.
    #[cfg(not(feature = "sched_eevdf"))]
    RUN_QUEUES[target_hart].lock().refresh_min_vruntime(None);
    notify_enqueued_task(target_hart, incoming);
}

fn enqueue_wakeup_task(task: Arc<TaskControlBlock>, target_hart: usize) -> bool {
    let current_entity_hint = running_fair_entity_snapshot(target_hart);
    let (incoming, repair_info) = {
        let mut rq = RUN_QUEUES[target_hart].lock();
        let mut task_inner = task.inner_exclusive_access();
        let repair_lost_runnable = match task_inner.task_status {
            TaskStatus::Interruptible | TaskStatus::Uninterruptible => false,
            // A normal Runnable task should either be queued or on a CPU.
            // If both ownership markers are clear, preserve the wakeup by
            // routing it through the regular enqueue path and log the repair.
            TaskStatus::Runnable => {
                if task_inner.sched.on_rq || task.on_cpu.load(Ordering::Relaxed) {
                    return true;
                }
                true
            }
            TaskStatus::Running | TaskStatus::Zombie => return true,
        };
        if task_inner.sched.on_rq || task.on_cpu.load(Ordering::Relaxed) {
            task_inner.task_status = TaskStatus::Runnable;
            task_inner.wait_reason = None;
            task_inner.current_wq_handle = None;
            return true;
        }
        let repair_info = repair_lost_runnable.then_some((
            task_inner.sched.last_cpu,
            task_inner.sched.policy,
            task_inner.pending_signals.bits(),
            task_inner.signal_mask.bits(),
            task_inner.sched.resched_reason,
        ));
        task_inner.task_status = TaskStatus::Runnable;
        task_inner.wait_reason = None;
        task_inner.current_wq_handle = None;
        if matches!(task_inner.sched.policy, SchedPolicy::Rr) {
            task_inner.reset_time_slice();
        }
        task_inner.sched.last_cpu = target_hart;
        task_inner.sched.on_rq = true;
        let incoming = rq.enqueue_locked(Arc::clone(&task), &mut task_inner, current_entity_hint);
        (incoming, repair_info)
    };
    #[cfg(not(feature = "sched_eevdf"))]
    RUN_QUEUES[target_hart].lock().refresh_min_vruntime(None);
    if let Some((last_cpu, policy, pending, mask, resched)) = repair_info {
        let count = LOST_RUNNABLE_REPAIR_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if should_log_sched_sample(count) {
            warn!(
                "[sched][repair-lost-runnable] count={} task={:#x} target_hart={} \
                 last_cpu={} policy={:?} pending={:#x} mask={:#x} resched={:?}",
                count,
                Arc::as_ptr(&task) as usize,
                target_hart,
                last_cpu,
                policy,
                pending,
                mask,
                resched,
            );
        }
    }
    notify_enqueued_task(target_hart, incoming);
    true
}

fn mark_task_woken(task_inner: &mut TaskControlBlockInner) {
    task_inner.task_status = TaskStatus::Runnable;
    task_inner.wait_reason = None;
    task_inner.current_wq_handle = None;
    if matches!(task_inner.sched.policy, SchedPolicy::Rr) {
        task_inner.reset_time_slice();
    }
}

enum OnCpuWakeAction {
    Done,
    Enqueue {
        preferred_hart: usize,
        affinity_mask: usize,
        policy: SchedPolicy,
    },
    WaitForSwitch {
        last_cpu: usize,
        affinity_mask: usize,
        policy: SchedPolicy,
    },
}

/// Revalidate and wake a task that was observed with `on_cpu=true`.
///
/// The processor lock is deliberately acquired before the task lock, matching
/// `run_tasks`.  Holding both across the `processor.current` check and the
/// Runnable transition prevents the owning hart from committing a block in
/// between those two operations.
fn resolve_on_cpu_wake(task: &Arc<TaskControlBlock>, last_cpu: usize) -> OnCpuWakeAction {
    let processor = processor_for_hart(last_cpu).lock();
    let mut task_inner = task.inner_exclusive_access();

    if !matches!(
        task_inner.task_status,
        TaskStatus::Interruptible | TaskStatus::Uninterruptible
    ) {
        return OnCpuWakeAction::Done;
    }
    if task_inner.sched.on_rq {
        mark_task_woken(&mut task_inner);
        return OnCpuWakeAction::Done;
    }

    let is_still_current = processor
        .current()
        .is_some_and(|current| Arc::ptr_eq(&current, task));
    if is_still_current {
        mark_task_woken(&mut task_inner);
        let target_hart = normalize_hart(task_inner.sched.last_cpu);
        drop(task_inner);
        drop(processor);
        resched_hart(target_hart);
        return OnCpuWakeAction::Done;
    }

    let preferred_hart = task_inner.sched.last_cpu;
    let affinity_mask = task_inner.sched.cpu_affinity_mask;
    let policy = task_inner.sched.policy;
    if task.on_cpu.load(Ordering::Acquire) {
        OnCpuWakeAction::WaitForSwitch {
            last_cpu,
            affinity_mask,
            policy,
        }
    } else {
        OnCpuWakeAction::Enqueue {
            preferred_hart,
            affinity_mask,
            policy,
        }
    }
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
        if task_inner.exit_code.is_none()
            && matches!(task_inner.task_status, TaskStatus::Runnable)
        {
            task.on_cpu.store(true, Ordering::Relaxed);
            task_inner.sched.last_cpu = hart;
            drop(task_inner);
            return Some(task);
        }
        if task_inner.exit_code.is_some() {
            task_inner.task_status = TaskStatus::Zombie;
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
            if task_inner.exit_code.is_some()
                || !matches!(task_inner.task_status, TaskStatus::Runnable)
            {
                if task_inner.exit_code.is_some() {
                    task_inner.task_status = TaskStatus::Zombie;
                }
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
                if task.on_cpu.load(Ordering::Acquire) {
                    let last_cpu = normalize_hart(task_inner.sched.last_cpu);
                    drop(task_inner);
                    match resolve_on_cpu_wake(&task, last_cpu) {
                        OnCpuWakeAction::Done => return true,
                        OnCpuWakeAction::Enqueue {
                            preferred_hart,
                            affinity_mask,
                            policy,
                        } => Some((preferred_hart, affinity_mask, policy)),
                        OnCpuWakeAction::WaitForSwitch {
                            last_cpu,
                            affinity_mask,
                            policy,
                        } => {
                            // `take_current_task()` has already removed the
                            // task from its processor, but the owning hart has
                            // not finished saving its context.  Wait without
                            // holding scheduler locks, then let the enqueue path
                            // revalidate ownership under runqueue+task locks.
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
                            while task.on_cpu.load(Ordering::Acquire) {
                                core::hint::spin_loop();
                            }
                            Some((last_cpu, affinity_mask, policy))
                        }
                    }
                } else {
                    Some((
                        task_inner.sched.last_cpu,
                        task_inner.sched.cpu_affinity_mask,
                        task_inner.sched.policy,
                    ))
                }
            }
            TaskStatus::Runnable => {
                // Ordinary Runnable tasks are already owned by a runqueue or
                // a hart. Only repair the observed SMP invariant violation:
                // Runnable, not current anywhere, and not queued anywhere.
                if task_inner.sched.on_rq || task.on_cpu.load(Ordering::Relaxed) {
                    return true;
                }
                let target = (
                    task_inner.sched.last_cpu,
                    task_inner.sched.cpu_affinity_mask,
                    task_inner.sched.policy,
                );
                drop(task_inner);
                if task_is_current_on_any_hart(&task) {
                    return true;
                }
                Some(target)
            }
            TaskStatus::Running | TaskStatus::Zombie => {
                return true;
            }
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
    let mut removed_from = None;
    for (hart, rq) in RUN_QUEUES.iter().enumerate() {
        if rq.lock().remove_task(&task) {
            removed_from = Some(hart);
            break;
        }
    }
    let mut task_inner = task.inner_exclusive_access();
    task_inner.sched.on_rq = false;
    task_inner.sched.cfs_rq_key = None;
    task_inner.sched.eevdf_deadline_ns = 0;
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
    let task = RUN_QUEUES[hart].lock().stop_task.take();
    if let Some(task) = task {
        // Process teardown and exec wait with Acquire before reclaiming this
        // task's trap frame/address-space resources. Keep it on-CPU until the
        // architectural context switch has made its kernel stack unreachable.
        task.on_cpu.store(false, Ordering::Release);
        drop(task);
    }
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
