//! Types related to task management & Functions for completely changing TCB

use super::id::TaskUserRes;
use super::wait_queue::WaitQueueHandle;
use super::{kstack_alloc, KernelStack, ProcessControlBlock};
use crate::sched::{SchedAttr, SchedPolicy, TaskContext};
use crate::config::MAX_HARTS;
use crate::trap::TrapContext;
use crate::{mm::PhysPageNum};
use crate::sync::{SpinNoIrqLock, SpinNoIrqLockGuard};
use alloc::sync::{Arc, Weak};

/// Return a mask containing all online harts supported by the kernel.
pub const fn all_cpu_affinity_mask() -> usize {
    if MAX_HARTS == 0 {
        0
    } else if MAX_HARTS >= usize::BITS as usize {
        usize::MAX
    } else {
        (1usize << MAX_HARTS) - 1
    }
}

/// Scheduler-owned mutable runtime state associated with one task.
pub struct TaskSchedState {
    /// Last hart that ran this task.
    pub last_cpu: usize,
    /// Whether the task is currently running on a CPU.
    pub on_cpu: bool,
    /// Whether the task is currently queued on a runqueue.
    pub on_rq: bool,
    /// Current scheduling policy.
    pub policy: SchedPolicy,
    /// Real-time priority. Larger value means higher priority.
    pub rt_priority: u8,
    /// Configured round-robin time slice, in timer ticks.
    pub time_slice_ticks: u32,
    /// Remaining time slice budget, in timer ticks.
    pub remaining_slice_ticks: u32,
    /// Deferred reschedule request handled at safe scheduling points.
    pub need_resched: bool,
    /// Allowed target harts for this task. Bit `n` corresponds to hart `n`.
    pub cpu_affinity_mask: usize,
}

impl TaskSchedState {
    /// Create a new `TaskSchedState` with the given scheduling attributes and default values.
    pub fn new(sched_attr: SchedAttr) -> Self {
        Self {
            last_cpu: 0,
            on_cpu: false,
            on_rq: false,
            policy: sched_attr.policy,
            rt_priority: sched_attr.rt_priority,
            time_slice_ticks: sched_attr.time_slice_ticks,
            remaining_slice_ticks: sched_attr.time_slice_ticks,
            need_resched: false,
            cpu_affinity_mask: all_cpu_affinity_mask(),
        }
    }

    /// Get the scheduling attributes corresponding to the current state.
    pub fn sched_attr(&self) -> SchedAttr {
        SchedAttr {
            policy: self.policy,
            rt_priority: self.rt_priority,
            time_slice_ticks: self.time_slice_ticks,
        }
    }

    /// Reset the remaining time slice to the full length according to the current scheduling attributes.
    pub fn reset_time_slice(&mut self) {
        self.remaining_slice_ticks = self.time_slice_ticks;
    }
}

/// Task control block structure
pub struct TaskControlBlock {
    /// immutable
    pub process: Weak<ProcessControlBlock>,
    /// Kernel stack corresponding to PID
    pub kstack: KernelStack,
    /// mutable
    inner: SpinNoIrqLock<TaskControlBlockInner>,
}

impl TaskControlBlock {
    /// Get the mutable reference of the inner TCB
    pub fn inner_exclusive_access(&self) -> SpinNoIrqLockGuard<'_, TaskControlBlockInner> {
        self.inner.lock()
    }
    /// Get the address of app's page table
    pub fn get_user_token(&self) -> usize {
        let process = self.process.upgrade().unwrap();
        let inner = process.inner_exclusive_access();
        inner.memory_set.token()
    }
}

pub struct TaskControlBlockInner {
    pub res: Option<TaskUserRes>,
    /// The physical page number of the frame where the trap context is placed
    pub trap_cx_ppn: PhysPageNum,
    /// Save task context
    pub task_cx: TaskContext,

    /// Maintain the execution status of the current task.
    pub task_status: TaskStatus,
    /// Why this task is blocked (if blocked by a sleep queue/event).
    pub wait_reason: Option<WaitReason>,
    /// It is set when active exit or execution error occurs
    pub exit_code: Option<i32>,
    /// Scheduler-private mutable runtime state.
    pub sched: TaskSchedState,
    /// Handle to the WaitQueue this task is currently sleeping in (if any).
    /// Used by signal delivery to properly remove the task from the queue.
    pub current_wq_handle: Option<WaitQueueHandle>,
}

impl TaskControlBlockInner {
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.trap_cx_ppn.get_mut()
    }

    #[allow(unused)]
    fn get_status(&self) -> TaskStatus {
        self.task_status
    }

    pub fn sched_attr(&self) -> SchedAttr {
        self.sched.sched_attr()
    }

    pub fn reset_time_slice(&mut self) {
        self.sched.reset_time_slice();
    }
}

impl TaskControlBlock {
    /// Create a new task
    pub fn new(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        alloc_user_res: bool,
        sched_attr: SchedAttr,
    ) -> Self {
        let res = TaskUserRes::new(Arc::clone(&process), ustack_base, alloc_user_res);
        let trap_cx_ppn = res.trap_cx_ppn();
        let kstack = kstack_alloc();
        let kstack_top = kstack.get_top();
        Self {
            process: Arc::downgrade(&process),
            kstack,
            inner: SpinNoIrqLock::new(TaskControlBlockInner {
                res: Some(res),
                trap_cx_ppn,
                task_cx: TaskContext::goto_trap_return(kstack_top),
                task_status: TaskStatus::Runnable,
                wait_reason: None,
                exit_code: None,
                sched: TaskSchedState::new(sched_attr),
                current_wq_handle: None,
            }),
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
/// Task blocking reason for unified sleep/wakeup semantics.
pub enum WaitReason {
    /// Unknown or unspecified wait reason.
    Unknown,
    /// Waiting on a legacy condition variable.
    Condvar,
    /// Waiting for a semaphore to become available.
    Semaphore,
    /// Waiting for a mutex to become available.
    Mutex,
    /// Parent is waiting for child process exit.
    ProcessWaitExit,
    /// Waiting for UART RX data.
    UartRx,
    /// Waiting for pipe to become readable.
    PipeReadable,
    /// Waiting for pipe to become writable.
    PipeWritable,
    /// Waiting for nanosleep timer expiration.
    Nanosleep,
    /// Waiting for block device I/O completion.
    BlockDeviceIo,
    /// Waiting for poll/ppoll readiness notification.
    Poll,
    /// Waiting for network device TX completion.
    NetDeviceTx,
    /// Waiting for socket data to become readable.
    SocketReadable,
    /// Waiting for socket buffer space / writable state.
    SocketWritable,
    /// Waiting for signal delivery in sigsuspend.
    SignalSuspend,
    /// Waiting for one of a selected signal set in sigtimedwait.
    SignalTimedWait,
}

#[derive(Copy, Clone, PartialEq)]
/// Linux-like task lifecycle states.
pub enum TaskStatus {
    /// Running
    Running,
    /// Ready to run but not currently running.
    Runnable,
    /// Sleeping and can be woken by ordinary events/signals.
    Interruptible,
    /// Sleeping and should only be woken by the waited event.
    Uninterruptible,
    /// Exited and must not be scheduled again.
    Zombie,
}
