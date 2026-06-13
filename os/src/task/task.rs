//! Types related to task management & Functions for completely changing TCB

use super::id::TaskUserRes;
use super::wait_queue::WaitQueueHandle;
use super::{kstack_alloc, KernelStack, ProcessControlBlock, SigInfo, SignalBit, MAX_SIG};
use crate::mm::MmError;
use crate::config::MAX_HARTS;
use crate::hal::traits::AddressSpaceToken;
use crate::mm::PhysPageNum;
use crate::sched::{ReschedReason, SchedAttr, SchedPolicy, TaskContext, NICE_0_LOAD};
use crate::sync::{SpinNoIrqLock, SpinNoIrqLockGuard};
use crate::trap::TrapContext;
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
    /// User-visible Linux scheduling policy value.
    pub linux_policy: i32,
    /// Real-time priority. Larger value means higher priority.
    pub rt_priority: u8,
    /// Configured round-robin time slice, in timer ticks.
    pub time_slice_ticks: u32,
    /// Remaining time slice budget, in timer ticks.
    pub remaining_slice_ticks: u32,
    /// Linux nice value used by CFS.
    pub nice: i32,
    /// CFS load weight derived from nice.
    pub weight: u64,
    /// Raw Linux `sched_attr.sched_flags`.
    pub sched_flags: u64,
    /// Linux `sched_attr.sched_runtime` for `SCHED_DEADLINE`.
    pub sched_runtime: u64,
    /// Linux `sched_attr.sched_deadline` for `SCHED_DEADLINE`.
    pub sched_deadline: u64,
    /// Linux `sched_attr.sched_period` for `SCHED_DEADLINE`.
    pub sched_period: u64,
    /// Linux util clamp minimum hint.
    pub sched_util_min: u32,
    /// Linux util clamp maximum hint.
    pub sched_util_max: u32,
    /// Virtual runtime used as the CFS ordering key, in nanoseconds.
    pub vruntime_ns: u64,
    /// Last timestamp at which execution accounting was started, in nanoseconds.
    pub exec_start_ns: u64,
    /// Total runtime accounted by CFS, in nanoseconds.
    pub sum_exec_runtime_ns: u64,
    /// Runtime accounting baseline for the current CFS CPU slice, in nanoseconds.
    pub cfs_slice_start_ns: u64,
    /// Current key while this task is linked into a CFS runqueue.
    pub cfs_rq_key: Option<(u64, usize)>,
    /// Whether the task has been placed on a CFS runqueue before.
    pub cfs_initialized: bool,
    /// Deferred reschedule request handled at safe scheduling points.
    pub resched_reason: Option<ReschedReason>,
    /// Insert the task at the head of its RT priority queue on the next enqueue.
    pub rt_enqueue_head: bool,
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
            linux_policy: sched_attr.linux_policy,
            rt_priority: sched_attr.rt_priority,
            time_slice_ticks: sched_attr.time_slice_ticks,
            remaining_slice_ticks: sched_attr.time_slice_ticks,
            nice: sched_attr.nice,
            weight: sched_attr.weight,
            sched_flags: sched_attr.sched_flags,
            sched_runtime: sched_attr.sched_runtime,
            sched_deadline: sched_attr.sched_deadline,
            sched_period: sched_attr.sched_period,
            sched_util_min: sched_attr.sched_util_min,
            sched_util_max: sched_attr.sched_util_max,
            vruntime_ns: 0,
            exec_start_ns: 0,
            sum_exec_runtime_ns: 0,
            cfs_slice_start_ns: 0,
            cfs_rq_key: None,
            cfs_initialized: false,
            resched_reason: None,
            rt_enqueue_head: false,
            cpu_affinity_mask: all_cpu_affinity_mask(),
        }
    }

    /// Get the scheduling attributes corresponding to the current state.
    pub fn sched_attr(&self) -> SchedAttr {
        SchedAttr {
            policy: self.policy,
            linux_policy: self.linux_policy,
            rt_priority: self.rt_priority,
            time_slice_ticks: self.time_slice_ticks,
            nice: self.nice,
            weight: self.weight,
            sched_flags: self.sched_flags,
            sched_runtime: self.sched_runtime,
            sched_deadline: self.sched_deadline,
            sched_period: self.sched_period,
            sched_util_min: self.sched_util_min,
            sched_util_max: self.sched_util_max,
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
    /// Get the current user address-space token for this task.
    pub fn get_user_token(&self) -> AddressSpaceToken {
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
    /// Userspace TID address to clear on thread exit for Linux clone compatibility.
    pub clear_child_tid: usize,
    /// Signals pending specifically for this thread.
    pub pending_signals: SignalBit,
    /// Per-signal metadata paired with `pending_signals`.
    pub pending_siginfo: [SigInfo; MAX_SIG + 1],
    /// Per-thread blocked signal mask.
    pub signal_mask: SignalBit,
    /// Backup of the pre-sigsuspend mask, restored by rt_sigreturn or when no handler runs.
    pub signal_mask_backup: Option<SignalBit>,
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

    /// Account CFS runtime up to `now_ns` for a currently running regular task.
    pub fn account_cfs_runtime(&mut self, now_ns: u64) {
        if !matches!(self.sched.policy, SchedPolicy::Other) {
            return;
        }
        if self.sched.exec_start_ns == 0 {
            self.sched.exec_start_ns = now_ns;
            self.sched.cfs_slice_start_ns = now_ns;
            return;
        }
        let delta_exec = now_ns.saturating_sub(self.sched.exec_start_ns);
        if delta_exec == 0 {
            return;
        }
        self.sched.exec_start_ns = now_ns;
        self.sched.sum_exec_runtime_ns = self.sched.sum_exec_runtime_ns.saturating_add(delta_exec);
        let delta_fair = if self.sched.weight == NICE_0_LOAD {
            delta_exec
        } else {
            (delta_exec as u128)
                .saturating_mul(NICE_0_LOAD as u128)
                .checked_div(self.sched.weight.max(1) as u128)
                .unwrap_or(0) as u64
        };
        self.sched.vruntime_ns = self.sched.vruntime_ns.saturating_add(delta_fair);
    }
}

impl TaskControlBlock {
    /// Create a new task
    pub fn new(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        alloc_user_res: bool,
        sched_attr: SchedAttr,
    ) -> Result<Self, MmError> {
        let res = TaskUserRes::new(Arc::clone(&process), ustack_base, alloc_user_res)?;
        let trap_cx_ppn = res.trap_cx_ppn();
        let kstack = kstack_alloc()?;
        let kstack_top = kstack.get_top();
        Ok(Self {
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
                clear_child_tid: 0,
                pending_signals: SignalBit::empty(),
                pending_siginfo: [SigInfo::default(); MAX_SIG + 1],
                signal_mask: SignalBit::empty(),
                signal_mask_backup: None,
            }),
        })
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
    /// Waiting on a Linux futex word.
    Futex,
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
