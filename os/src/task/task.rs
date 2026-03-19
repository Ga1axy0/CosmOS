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

    /// Maintain the execution status of the current process
    pub task_status: TaskStatus,
    /// Why this task is blocked (if blocked by a sleep queue/event).
    pub wait_reason: Option<WaitReason>,
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
                    task_status: TaskStatus::Ready,
                    wait_reason: None,
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
}

#[derive(Copy, Clone, PartialEq)]
/// The execution status of the current process
pub enum TaskStatus {
    /// 已在本地 hart 上切出，等待转入真正可运行队列
    PreReady,
    /// ready to run
    Ready,
    /// running
    Running,
    /// blocked
    Blocked,
}
