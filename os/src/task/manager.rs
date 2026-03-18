//! Per-hart run queues with work-stealing scheduler.
//!
//! Each hart owns a local FIFO ready queue protected by a [`SpinNoIrqLock`].
//! `fetch_task()` first tries the local queue; if empty it attempts to steal
//! from other harts' queues in round-robin order.
//!
//! A global overflow queue is used when the caller cannot determine the hart
//! (e.g. `wakeup_task` from an arbitrary context); local queues drain the
//! overflow queue as part of the steal loop.

use super::{ProcessControlBlock, TaskControlBlock, TaskStatus};
use crate::config::MAX_HARTS;
use crate::hart::hartid;
use crate::sync::{SpinNoIrqLock};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use core::array;
use lazy_static::*;

/// Per-hart local run queue.
struct LocalRunQueue {
    /// 当前 hart 私有的预就绪队列；其中任务尚未对其他 hart 可见
    pre_ready_queue: VecDeque<Arc<TaskControlBlock>>,
    ready_queue: VecDeque<Arc<TaskControlBlock>>,
    /// Keep a reference to the last task that exited so its kernel stack
    /// is not freed while we are still running on it.
    stop_task: Option<Arc<TaskControlBlock>>,
}

impl LocalRunQueue {
    fn new() -> Self {
        Self {
            pre_ready_queue: VecDeque::new(),
            ready_queue: VecDeque::new(),
            stop_task: None,
        }
    }
    fn push_pre_ready(&mut self, task: Arc<TaskControlBlock>) {
        self.pre_ready_queue.push_back(task);
    }
    fn push(&mut self, task: Arc<TaskControlBlock>) {
        self.ready_queue.push_back(task);
    }
    fn pop(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.ready_queue.pop_front()
    }
    /// 从就绪队列中移除指定任务。
    fn remove_ready(&mut self, task: &Arc<TaskControlBlock>) {
        if let Some((idx, _)) = self
            .ready_queue
            .iter()
            .enumerate()
            .find(|(_, t)| Arc::as_ptr(t) == Arc::as_ptr(task))
        {
            self.ready_queue.remove(idx);
        }
    }
    /// Steal up to half of the tasks from this queue.
    fn steal_batch(&mut self) -> VecDeque<Arc<TaskControlBlock>> {
        let n = (self.ready_queue.len() + 1) / 2;
        self.ready_queue.drain(..n).collect()
    }
    fn len(&self) -> usize {
        self.ready_queue.len()
    }
    /// 将本 hart 上已安全切出的任务转入真正的就绪队列
    fn promote_pre_ready(&mut self) {
        while let Some(task) = self.pre_ready_queue.pop_front() {
            {
                let mut task_inner = task.inner_exclusive_access();
                debug_assert!(matches!(task_inner.task_status, TaskStatus::PreReady));
                task_inner.task_status = TaskStatus::Ready;
            }
            self.ready_queue.push_back(task);
        }
    }
}

lazy_static! {
    /// Per-hart run queues, indexed by hart id.
    static ref RUN_QUEUES: [SpinNoIrqLock<LocalRunQueue>; MAX_HARTS] =
        array::from_fn(|_| SpinNoIrqLock::new(LocalRunQueue::new()));

    /// Global overflow queue for tasks that cannot be assigned to a
    /// specific hart at the time of enqueue.
    static ref GLOBAL_QUEUE: SpinNoIrqLock<VecDeque<Arc<TaskControlBlock>>> =
        SpinNoIrqLock::new(VecDeque::new());

    /// PID2PCB instance (map of pid to pcb)
    pub static ref PID2PCB: SpinNoIrqLock<BTreeMap<usize, Arc<ProcessControlBlock>>> =
        SpinNoIrqLock::new(BTreeMap::new());
}

/// Add a task to the current hart's local run queue.
pub fn add_task(task: Arc<TaskControlBlock>) {
    let hart = hartid();
    RUN_QUEUES[hart].lock().push(task);
}

/// 向当前 hart 的预就绪队列添加任务。
pub fn add_task_pre_ready(task: Arc<TaskControlBlock>) {
    let hart = hartid();
    RUN_QUEUES[hart].lock().push_pre_ready(task);
}

/// Add a task to the global overflow queue.
///
/// Use this when the caller does not know (or should not depend on) which
/// hart will pick up the task — e.g. `wakeup_task` from an interrupt
/// handler that may run on any hart.
pub fn add_task_global(task: Arc<TaskControlBlock>) {
    GLOBAL_QUEUE.lock().push_back(task);
}

/// Wake up a task by marking it `Ready` and placing it on the global queue.
pub fn wakeup_task(task: Arc<TaskControlBlock>) {
    trace!("kernel: TaskManager::wakeup_task");
    let mut task_inner = task.inner_exclusive_access();
    task_inner.task_status = TaskStatus::Ready;
    drop(task_inner);
    // Put on global queue so that any idle hart can pick it up quickly.
    add_task_global(task);
}

/// Remove a task from **all** queues (local + global).
///
/// Used when a process exits and needs to cancel tasks that may be sitting
/// in a ready queue (e.g. timer-blocked threads).
pub fn remove_task(task: Arc<TaskControlBlock>) {
    // Try the global queue first.
    GLOBAL_QUEUE.lock().retain(|t| Arc::as_ptr(t) != Arc::as_ptr(&task));
    // 然后尝试从每个 hart 的真正就绪队列中删除。
    for rq in RUN_QUEUES.iter() {
        rq.lock().remove_ready(&task);
    }
}

/// 将当前 hart 延迟发布的预就绪任务转入真正的就绪队列。
pub fn promote_pre_ready_tasks() {
    let hart = hartid();
    RUN_QUEUES[hart].lock().promote_pre_ready();
}

/// Fetch the next task to run on the current hart.
///
/// Order:
/// 1. Local queue of the current hart.
/// 2. Global overflow queue (drain up to a batch).
/// 3. Steal from another hart's local queue.
pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    let hart = hartid();

    // 1. Try local queue.
    {
        let mut local = RUN_QUEUES[hart].lock();
        if let Some(task) = local.pop() {
            return Some(task);
        }
    }

    // 2. Try global queue — take one for ourselves, spread the rest.
    {
        let mut global = GLOBAL_QUEUE.lock();
        if let Some(task) = global.pop_front() {
            // Drain a few more into our local queue to amortise the lock.
            let n = core::cmp::min(global.len(), MAX_HARTS);
            let batch: VecDeque<Arc<TaskControlBlock>> = global
                .drain(..n)
                .collect();
            drop(global);
            if !batch.is_empty() {
                let mut local = RUN_QUEUES[hart].lock();
                for t in batch {
                    local.push(t);
                }
            }
            return Some(task);
        }
    }

    // 3. Work stealing — try each other hart once.
    for offset in 1..MAX_HARTS {
        let victim = (hart + offset) % MAX_HARTS;
        let mut victim_rq = RUN_QUEUES[victim].lock();
        if victim_rq.len() > 0 {
            let stolen = victim_rq.steal_batch();
            drop(victim_rq);
            let mut local = RUN_QUEUES[hart].lock();
            for t in stolen {
                local.push(t);
            }
            return local.pop();
        }
    }

    None
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
