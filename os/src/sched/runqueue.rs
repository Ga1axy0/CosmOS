//! Per-hart RT runqueue management for phase 1 SCHED_RR.

use super::{current_task, mark_current_task_need_resched, ProcessControlBlock, SCHED_RT_PRIO_MAX};
use super::{all_cpu_affinity_mask, TaskControlBlock, TaskStatus};
use crate::config::MAX_HARTS;
use crate::hart::hartid;
use crate::sbi::send_ipi_mask;
use crate::sync::SpinNoIrqLock;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use core::array;
use lazy_static::*;

const RT_QUEUE_LEVELS: usize = SCHED_RT_PRIO_MAX as usize + 1;

/// Local runnable queues owned by one hart.
struct RunQueue {
    queues: [VecDeque<Arc<TaskControlBlock>>; RT_QUEUE_LEVELS],
    highest_prio: Option<u8>,
    nr_running: usize,
    /// Keep a reference to the last exiting task so its kernel stack
    /// is not freed while this hart is still running on it.
    stop_task: Option<Arc<TaskControlBlock>>,
}

impl RunQueue {
    fn new() -> Self {
        Self {
            queues: array::from_fn(|_| VecDeque::new()),
            highest_prio: None,
            //保留字段，负责计数当前queue上runable的task，用于负载均衡。
            nr_running: 0,
            stop_task: None,
        }
    }

    fn enqueue(&mut self, task: Arc<TaskControlBlock>, prio: u8) {
        self.queues[prio as usize].push_back(task);
        self.nr_running += 1;
        self.highest_prio = Some(self.highest_prio.map_or(prio, |curr| curr.max(prio)));
    }

    fn dequeue_highest(&mut self) -> Option<Arc<TaskControlBlock>> {
        let prio = self.highest_prio?;
        let task = self.queues[prio as usize].pop_front()?;
        self.nr_running = self.nr_running.saturating_sub(1);
        self.refresh_highest_prio();
        Some(task)
    }

    fn remove_task(&mut self, task: &Arc<TaskControlBlock>, prio: u8) -> bool {
        if let Some((idx, _)) = self.queues[prio as usize]
            .iter()
            .enumerate()
            .find(|(_, t)| Arc::as_ptr(t) == Arc::as_ptr(task))
        {
            self.queues[prio as usize].remove(idx);
            self.nr_running = self.nr_running.saturating_sub(1);
            self.refresh_highest_prio();
            true
        } else {
            false
        }
    }

    fn highest_prio(&self) -> Option<u8> {
        self.highest_prio
    }

    fn has_same_or_higher(&self, prio: u8) -> bool {
        self.highest_prio.is_some_and(|highest| highest >= prio)
    }

    fn refresh_highest_prio(&mut self) {
        self.highest_prio = (1..RT_QUEUE_LEVELS)
            .rev()
            .find(|prio| !self.queues[*prio].is_empty())
            .map(|prio| prio as u8);
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

fn select_target_hart(preferred_hart: usize, affinity_mask: usize) -> usize {
    let affinity_mask = if affinity_mask == 0 {
        all_cpu_affinity_mask()
    } else {
        affinity_mask & all_cpu_affinity_mask()
    };
    let preferred_hart = normalize_hart(preferred_hart);
    if affinity_mask & (1usize << preferred_hart) != 0 {
        preferred_hart
    } else {
        affinity_mask.trailing_zeros() as usize
    }
}

/// Request a reschedule on the selected hart, using an IPI when needed.
pub fn resched_hart(hart: usize) {
    let target_hart = normalize_hart(hart);
    if target_hart == hartid() {
        mark_current_task_need_resched();
        return;
    }
    send_ipi_mask(1usize << target_hart);
}

fn maybe_preempt_current_on_this_hart(incoming_prio: u8) {
    if let Some(task) = current_task() {
        let task_inner = task.inner_exclusive_access();
        if task_inner.on_cpu
            && matches!(task_inner.task_status, TaskStatus::Running)
            && incoming_prio > task_inner.rt_priority
        {
            drop(task_inner);
            mark_current_task_need_resched();
        }
    }
}

/// Returns whether this hart already has runnable work at or above `prio`.
pub fn has_runnable_task_at_or_above(hart: usize, prio: u8) -> bool {
    RUN_QUEUES[normalize_hart(hart)]
        .lock()
        .has_same_or_higher(prio)
}

/// Returns the highest runnable RT priority on the selected hart.
pub fn highest_runnable_prio(hart: usize) -> Option<u8> {
    RUN_QUEUES[normalize_hart(hart)].lock().highest_prio()
}

/// Add a task to the current hart's runqueue.
pub fn add_task(task: Arc<TaskControlBlock>) {
    enqueue_task_on(task, hartid());
}

/// Add a task to a specific hart's runqueue.
pub fn enqueue_task_on(task: Arc<TaskControlBlock>, hart: usize) {
    let (target_hart, prio) = {
        let mut task_inner = task.inner_exclusive_access();
        if task_inner.on_rq || task_inner.on_cpu || matches!(task_inner.task_status, TaskStatus::Zombie) {
            return;
        }
        let target_hart = select_target_hart(hart, task_inner.cpu_affinity_mask);
        task_inner.task_status = TaskStatus::Runnable;
        task_inner.wait_reason = None;
        task_inner.last_cpu = target_hart;
        task_inner.on_rq = true;
        (target_hart, task_inner.rt_priority)
    };
    RUN_QUEUES[target_hart].lock().enqueue(task, prio);
}

/// Pop one runnable task from the selected hart's runqueue.
pub fn dequeue_task(hart: usize) -> Option<Arc<TaskControlBlock>> {
    let task = RUN_QUEUES[normalize_hart(hart)].lock().dequeue_highest()?;
    {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.on_rq = false;
    }
    Some(task)
}

/// Pick the next task for the selected hart.
pub fn pick_next_task(hart: usize) -> Option<Arc<TaskControlBlock>> {
    dequeue_task(hart)
}

/// Wake up a sleeping task and place it on its target hart runqueue.
pub fn wakeup_task(task: Arc<TaskControlBlock>) -> bool {
    let wake_target = {
        let mut task_inner = task.inner_exclusive_access();
        match task_inner.task_status {
            TaskStatus::Interruptible | TaskStatus::Uninterruptible => {
                if task_inner.on_rq {
                    return false;
                }
                task_inner.task_status = TaskStatus::Runnable;
                task_inner.wait_reason = None;
                task_inner.reset_time_slice();
                if task_inner.on_cpu {
                    None
                } else {
                    let target_hart = select_target_hart(task_inner.last_cpu, task_inner.cpu_affinity_mask);
                    task_inner.on_rq = true;
                    task_inner.last_cpu = target_hart;
                    Some(target_hart)
                }
            }
            TaskStatus::Running | TaskStatus::Runnable | TaskStatus::Zombie => return false,
        }
    };
    if let Some(target_hart) = wake_target {
        let prio = {
            let task_inner = task.inner_exclusive_access();
            task_inner.rt_priority
        };
        RUN_QUEUES[target_hart].lock().enqueue(Arc::clone(&task), prio);
        if target_hart == hartid() {
            maybe_preempt_current_on_this_hart(prio);
        } else {
            resched_hart(target_hart);
        }
        // trace!("kernel: wakeup_task -> hart {} prio {}", target_hart, prio);
    }
    true
}

/// Remove a task from all local runqueues.
pub fn remove_task(task: Arc<TaskControlBlock>) {
    let prio = {
        let task_inner = task.inner_exclusive_access();
        task_inner.rt_priority
    };
    for rq in RUN_QUEUES.iter() {
        if rq.lock().remove_task(&task, prio) {
            break;
        }
    }
    let mut task_inner = task.inner_exclusive_access();
    task_inner.on_rq = false;
}

/// Set a task to stop-wait status on the current hart, keeping its kernel
/// stack alive until the next context switch on this hart.
pub fn add_stopping_task(task: Arc<TaskControlBlock>) {
    let hart = hartid();
    RUN_QUEUES[hart].lock().stop_task = Some(task);
}

/// Get process by pid.
pub fn pid2process(pid: usize) -> Option<Arc<ProcessControlBlock>> {
    let map = PID2PCB.lock();
    map.get(&pid).map(Arc::clone)
}

/// Insert item(pid, pcb) into PID2PCB map.
pub fn insert_into_pid2process(pid: usize, process: Arc<ProcessControlBlock>) {
    PID2PCB.lock().insert(pid, process);
}

/// Remove item(pid, _some_pcb) from PID2PCB map.
pub fn remove_from_pid2process(pid: usize) {
    let mut map = PID2PCB.lock();
    if map.remove(&pid).is_none() {
        panic!("cannot find pid {} in pid2task!", pid);
    }
}
