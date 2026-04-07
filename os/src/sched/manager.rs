//! Per-hart runqueue management in a Linux-like layout.

use super::{ProcessControlBlock, TaskControlBlock, TaskStatus};
use crate::config::MAX_HARTS;
use crate::hart::hartid;
use crate::sync::SpinNoIrqLock;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use core::array;
use lazy_static::*;

/// Local runnable queue owned by one hart.
struct RunQueue {
    runnable: VecDeque<Arc<TaskControlBlock>>,
    /// Keep a reference to the last exiting task so its kernel stack
    /// is not freed while this hart is still running on it.
    stop_task: Option<Arc<TaskControlBlock>>,
}

impl RunQueue {
    fn new() -> Self {
        Self {
            runnable: VecDeque::new(),
            stop_task: None,
        }
    }

    fn enqueue(&mut self, task: Arc<TaskControlBlock>) {
        self.runnable.push_back(task);
    }

    fn dequeue(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.runnable.pop_front()
    }

    fn remove_task(&mut self, task: &Arc<TaskControlBlock>) {
        if let Some((idx, _)) = self
            .runnable
            .iter()
            .enumerate()
            .find(|(_, t)| Arc::as_ptr(t) == Arc::as_ptr(task))
        {
            self.runnable.remove(idx);
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

/// Add a task to the current hart's runqueue.
pub fn add_task(task: Arc<TaskControlBlock>) {
    enqueue_task_on(task, hartid());
}

/// Add a task to a specific hart's runqueue.
pub fn enqueue_task_on(task: Arc<TaskControlBlock>, hart: usize) {
    let target_hart = normalize_hart(hart);
    {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.task_status = TaskStatus::Runnable;
        task_inner.wait_reason = None;
        task_inner.last_cpu = target_hart;
    }
    RUN_QUEUES[target_hart].lock().enqueue(task);
}

/// Pop one runnable task from the selected hart's runqueue.
pub fn dequeue_task(hart: usize) -> Option<Arc<TaskControlBlock>> {
    RUN_QUEUES[normalize_hart(hart)].lock().dequeue()
}

/// Pick the next task for the selected hart.
pub fn pick_next_task(hart: usize) -> Option<Arc<TaskControlBlock>> {
    dequeue_task(hart)
}

/// Wake up a sleeping task and place it on its target hart runqueue.
pub fn wakeup_task(task: Arc<TaskControlBlock>) -> bool {
    trace!("kernel: TaskManager::wakeup_task");
    let target_hart = {
        let mut task_inner = task.inner_exclusive_access();
        match task_inner.task_status {
            TaskStatus::Interruptible | TaskStatus::Uninterruptible => {
                task_inner.task_status = TaskStatus::Runnable;
                task_inner.wait_reason = None;
                normalize_hart(task_inner.last_cpu)
            }
            TaskStatus::Running | TaskStatus::Runnable | TaskStatus::Zombie => return false,
        }
    };
    RUN_QUEUES[target_hart].lock().enqueue(task);
    true
}

/// Remove a task from all local runqueues.
pub fn remove_task(task: Arc<TaskControlBlock>) {
    for rq in RUN_QUEUES.iter() {
        rq.lock().remove_task(&task);
    }
}

/// Set a task to stop-wait status on the current hart, keeping its kernel
/// stack alive until the next context switch on this hart.
pub fn add_stopping_task(task: Arc<TaskControlBlock>) {
    let hart = hartid();
    RUN_QUEUES[hart].lock().stop_task = Some(task);
}

/// Get process by pid
pub fn pid2process(pid: usize) -> Option<Arc<ProcessControlBlock>> {
    let map = PID2PCB.lock();
    map.get(&pid).map(Arc::clone)
}

/// Insert item(pid, pcb) into PID2PCB map (called by do_fork AND ProcessControlBlock::new)
pub fn insert_into_pid2process(pid: usize, process: Arc<ProcessControlBlock>) {
    PID2PCB.lock().insert(pid, process);
}

/// Remove item(pid, _some_pcb) from PID2PCB map (called by exit_current_and_run_next)
pub fn remove_from_pid2process(pid: usize) {
    let mut map = PID2PCB.lock();
    if map.remove(&pid).is_none() {
        panic!("cannot find pid {} in pid2task!", pid);
    }
}
