//! Types related to task management & Functions for completely changing TCB

use super::id::TaskUserRes;
use super::{kstack_alloc, KernelStack, ProcessControlBlock, TaskContext};
use crate::trap::TrapContext;
use crate::{mm::PhysPageNum};
use crate::sync::{SpinNoIrqLock, SpinNoIrqLockGuard};
use alloc::sync::{Arc, Weak};

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
    /// Last hart that ran this task.
    pub last_cpu: usize,
    /// whether the task is running on cpu
    pub on_cpu: bool,
    /// whether the task is in runqueue
    pub on_rq: bool,
    /// It is set when active exit or execution error occurs
    pub exit_code: Option<i32>,
}

impl TaskControlBlockInner {
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        self.trap_cx_ppn.get_mut()
    }

    #[allow(unused)]
    fn get_status(&self) -> TaskStatus {
        self.task_status
    }
}

impl TaskControlBlock {
    /// Create a new task
    pub fn new(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        alloc_user_res: bool,
    ) -> Self {
        let res = TaskUserRes::new(Arc::clone(&process), ustack_base, alloc_user_res);
        let trap_cx_ppn = res.trap_cx_ppn();
        let kstack = kstack_alloc();
        let kstack_top = kstack.get_top();
        Self {
            process: Arc::downgrade(&process),
            kstack,
            inner: unsafe {
                SpinNoIrqLock::new(TaskControlBlockInner {
                    res: Some(res),
                    trap_cx_ppn,
                    task_cx: TaskContext::goto_trap_return(kstack_top),
                    task_status: TaskStatus::Runnable,
                    wait_reason: None,
                    last_cpu: 0,
                    on_cpu: false,
                    on_rq: false,
                    exit_code: None,
                })
            },
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
