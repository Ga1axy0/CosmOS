//! Types related to task management & Functions for completely changing TCB

use super::id::{TaskUserRes, TaskUserResAlloc};
use super::wait_queue::WaitQueueHandle;
use super::{kstack_alloc, KernelStack, ProcessControlBlock, SigInfo, SignalBit, MAX_SIG};
use crate::config::MAX_HARTS;
use crate::hal::traits::AddressSpaceToken;
use crate::mm::MmError;
use crate::mm::PhysPageNum;
use crate::sched::{ReschedReason, SchedAttr, SchedPolicy, TaskContext, NICE_0_LOAD};
use crate::sync::{SpinNoIrqLock, SpinNoIrqLockGuard};
use crate::timer::get_time_ns;
use crate::trap::TrapContext;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

const TASK_CONTROL_BLOCK_NEW_TIMING_WARN_THRESHOLD_NS: u64 = 1_000_000;
const RETURN_WORK_SIGNAL: u32 = 1 << 0;
#[cfg(feature = "return_work_cache")]
const RETURN_WORK_RESCHED: u32 = 1 << 1;
#[cfg(feature = "return_work_cache")]
const RETURN_WORK_ZOMBIE: u32 = 1 << 2;

// Keep the accounting mode and its raw-counter timestamp in one atomic word.
// The timer counter is reduced modulo 2^(usize::BITS - 2); subtraction below
// uses the same modulus, so a counter wrap is handled correctly as long as a
// single running slice is shorter than that (hundreds of years at GHz rates).
const CPU_ACCOUNTING_MODE_SHIFT: u32 = usize::BITS - 2;
const CPU_ACCOUNTING_TIMESTAMP_MASK: usize = (1usize << CPU_ACCOUNTING_MODE_SHIFT) - 1;
const CPU_ACCOUNTING_INACTIVE: usize = 0;
const CPU_ACCOUNTING_USER: usize = 1;
const CPU_ACCOUNTING_KERNEL: usize = 2;

#[inline(always)]
const fn pack_cpu_accounting(mode: usize, timestamp: usize) -> usize {
    (mode << CPU_ACCOUNTING_MODE_SHIFT) | (timestamp & CPU_ACCOUNTING_TIMESTAMP_MASK)
}

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
    /// Whether this task is currently running on a CPU. A lock-free atomic so a
    /// remote waker can observe the post-context-switch clear without taking the
    /// inner lock. Written `Release` by the post-switch cleanup after the task's
    /// registers are saved; a remote waker spins on it with `Acquire`. All other
    /// accesses are under the inner lock and use `Relaxed`.
    pub on_cpu: AtomicBool,
    /// Per-task CPU accounting state: mode in the top two bits and the raw
    /// timer timestamp of the last transition in the remaining bits.
    ///
    /// Only the hart currently owning this task writes the stamp. Keeping it
    /// task-local makes simultaneous threads of one process independent.
    cpu_accounting_stamp: AtomicUsize,
    /// Lock-free bitmap for work that must run before returning to userspace.
    ///
    /// Bits are conservative hints for signal delivery, rescheduling and
    /// process exit. The trap exit fast path needs only one acquire load;
    /// authoritative state remains protected by the existing locks.
    return_work: AtomicU32,
    /// Physical trap-frame page, userspace VA and address-space token cached
    /// for the lifetime of the current exec image.
    #[cfg(feature = "trap_context_cache")]
    trap_cx_ppn_cache: AtomicUsize,
    #[cfg(feature = "trap_context_cache")]
    trap_cx_user_va_cache: usize,
    #[cfg(feature = "trap_context_cache")]
    user_token_cache: AtomicUsize,
}

impl TaskControlBlock {
    /// Get the mutable reference of the inner TCB
    pub fn inner_exclusive_access(&self) -> SpinNoIrqLockGuard<'_, TaskControlBlockInner> {
        self.inner.lock()
    }

    /// Whether this scheduler entity owns a userspace trap frame/address
    /// space. Kernel threads deliberately keep the cached PPN at zero.
    #[inline(always)]
    pub fn has_user_context(&self) -> bool {
        #[cfg(feature = "trap_context_cache")]
        {
            return self.trap_cx_ppn_cache.load(Ordering::Relaxed) != 0;
        }
        #[cfg(not(feature = "trap_context_cache"))]
        self.inner.lock().res.is_some()
    }

    /// Commit the previous running slice and publish a new accounting mode.
    ///
    /// The task's `on_cpu` ownership invariant gives this stamp one writer at
    /// a time. PCB totals are intentionally relaxed counters: ordering CPU
    /// time against unrelated process state is unnecessary, while each atomic
    /// counter remains monotonic and race-free across harts.
    #[inline(always)]
    fn transition_cpu_accounting(
        &self,
        process: &ProcessControlBlock,
        new_mode: usize,
        now: usize,
    ) {
        let old = self.cpu_accounting_stamp.swap(
            pack_cpu_accounting(new_mode, now),
            Ordering::AcqRel,
        );
        let old_mode = old >> CPU_ACCOUNTING_MODE_SHIFT;
        if old_mode == CPU_ACCOUNTING_INACTIVE {
            return;
        }
        let old_timestamp = old & CPU_ACCOUNTING_TIMESTAMP_MASK;
        let now_timestamp = now & CPU_ACCOUNTING_TIMESTAMP_MASK;
        let delta = now_timestamp
            .wrapping_sub(old_timestamp)
            & CPU_ACCOUNTING_TIMESTAMP_MASK;
        match old_mode {
            CPU_ACCOUNTING_USER => process.commit_user_cpu_time(delta),
            CPU_ACCOUNTING_KERNEL => process.commit_kernel_cpu_time(delta),
            _ => debug_assert!(false, "invalid packed CPU accounting mode"),
        }
    }

    /// Account a user slice ending at `now` and begin a kernel slice.
    #[inline(always)]
    pub fn enter_kernel(&self, process: &ProcessControlBlock, now: usize) {
        self.transition_cpu_accounting(process, CPU_ACCOUNTING_KERNEL, now);
    }

    /// Account a kernel slice ending at `now` and begin a user slice.
    #[inline(always)]
    pub fn enter_user(&self, process: &ProcessControlBlock, now: usize) {
        self.transition_cpu_accounting(process, CPU_ACCOUNTING_USER, now);
    }

    /// Start accounting a task selected by the scheduler in kernel mode.
    #[inline(always)]
    pub fn resume_in_kernel(&self, process: &ProcessControlBlock, now: usize) {
        self.transition_cpu_accounting(process, CPU_ACCOUNTING_KERNEL, now);
    }

    /// Commit the current slice when this task leaves its hart.
    #[inline(always)]
    pub fn pause_cpu_accounting(&self, process: &ProcessControlBlock, now: usize) {
        self.transition_cpu_accounting(process, CPU_ACCOUNTING_INACTIVE, now);
    }

    /// Roll an open kernel slice forward at a periodic kernel-mode tick.
    ///
    /// Unlike `enter_kernel`, this must not resurrect an INACTIVE task: exit
    /// cleanup and the post-switch handoff can remain interruptible after the
    /// task has stopped owning CPU-accounting time.
    pub fn flush_kernel_cpu_accounting(&self, process: &ProcessControlBlock, now: usize) {
        let new = pack_cpu_accounting(CPU_ACCOUNTING_KERNEL, now);
        let mut old = self.cpu_accounting_stamp.load(Ordering::Relaxed);
        loop {
            if old >> CPU_ACCOUNTING_MODE_SHIFT != CPU_ACCOUNTING_KERNEL {
                return;
            }
            match self.cpu_accounting_stamp.compare_exchange_weak(
                old,
                new,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    let old_timestamp = old & CPU_ACCOUNTING_TIMESTAMP_MASK;
                    let delta = (now & CPU_ACCOUNTING_TIMESTAMP_MASK)
                        .wrapping_sub(old_timestamp)
                        & CPU_ACCOUNTING_TIMESTAMP_MASK;
                    process.commit_kernel_cpu_time(delta);
                    return;
                }
                Err(observed) => old = observed,
            }
        }
    }
    /// Get the current user address-space token for this task.
    #[cfg(not(feature = "trap_context_cache"))]
    pub fn get_user_token(&self) -> AddressSpaceToken {
        let process = self.process.upgrade().unwrap();
        let inner = process.inner_exclusive_access();
        inner.memory_set.token()
    }
    /// Get the token snapshot for the task's current exec image.
    #[cfg(feature = "trap_context_cache")]
    #[inline]
    pub fn get_user_token(&self) -> AddressSpaceToken {
        self.user_token_cache.load(Ordering::Acquire)
    }

    /// Get the current trap frame without acquiring task-inner.
    #[cfg(feature = "trap_context_cache")]
    #[inline]
    pub fn cached_trap_cx(&self) -> &'static mut TrapContext {
        PhysPageNum(self.trap_cx_ppn_cache.load(Ordering::Acquire)).get_mut()
    }

    /// Get the fixed userspace trap-frame VA for this task.
    #[cfg(feature = "trap_context_cache")]
    #[inline]
    pub fn cached_trap_cx_user_va(&self) -> usize {
        self.trap_cx_user_va_cache
    }

    /// Get this task's current user trap frame. Exec may replace the cached
    /// frame, so callers must reacquire it after a syscall that can exec.
    #[inline]
    pub fn trap_cx(&self) -> &'static mut TrapContext {
        #[cfg(feature = "trap_context_cache")]
        {
            return self.cached_trap_cx();
        }
        #[cfg(not(feature = "trap_context_cache"))]
        self.inner_exclusive_access().get_trap_cx()
    }

    /// Get the userspace virtual address of this task's trap frame.
    #[inline]
    pub fn trap_cx_user_va(&self) -> usize {
        #[cfg(feature = "trap_context_cache")]
        {
            return self.cached_trap_cx_user_va();
        }
        #[cfg(not(feature = "trap_context_cache"))]
        self.inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .trap_cx_user_va()
    }

    /// Publish trap metadata after exec has installed the replacement image.
    #[cfg(feature = "trap_context_cache")]
    pub(crate) fn update_trap_context_cache(
        &self,
        trap_cx_ppn: PhysPageNum,
        user_token: AddressSpaceToken,
    ) {
        self.trap_cx_ppn_cache
            .store(trap_cx_ppn.0, Ordering::Release);
        self.user_token_cache.store(user_token, Ordering::Release);
    }

    /// Return whether user-return signal handling needs the locked slow path.
    #[inline]
    pub fn signal_work_pending(&self) -> bool {
        self.return_work.load(Ordering::Acquire) & RETURN_WORK_SIGNAL != 0
    }

    /// Publish that signal delivery or mask-restoration work may be pending.
    #[inline]
    pub(crate) fn mark_signal_work_pending(&self) {
        self.return_work
            .fetch_or(RETURN_WORK_SIGNAL, Ordering::Release);
    }

    /// Refresh the signal-work hint from state protected by signal locks.
    #[inline]
    pub(crate) fn set_signal_work_pending(&self, pending: bool) {
        if pending {
            self.mark_signal_work_pending();
        } else {
            self.return_work
                .fetch_and(!RETURN_WORK_SIGNAL, Ordering::AcqRel);
        }
    }

    /// Return whether any deferred work may be required before user return.
    #[cfg(feature = "return_work_cache")]
    #[inline]
    pub fn return_work_pending(&self) -> bool {
        self.return_work.load(Ordering::Acquire) != 0
    }

    /// Publish that process teardown requires this task to leave userspace.
    #[cfg(feature = "return_work_cache")]
    #[inline]
    pub(crate) fn mark_zombie_work_pending(&self) {
        self.return_work
            .fetch_or(RETURN_WORK_ZOMBIE, Ordering::Release);
    }

    /// Return whether trap exit needs to inspect the locked reschedule reason.
    #[cfg(feature = "return_work_cache")]
    #[inline]
    pub fn resched_work_pending(&self) -> bool {
        self.return_work.load(Ordering::Acquire) & RETURN_WORK_RESCHED != 0
    }

    /// Update the authoritative reschedule reason and its lock-free hint.
    ///
    /// The caller must hold this task's inner lock. Keeping both writes in one
    /// helper prevents a locked producer from racing with a lockless hint clear
    /// and leaving `Some(reason)` paired with a false hint.
    #[inline]
    pub(crate) fn set_resched_reason_locked(
        &self,
        task_inner: &mut TaskControlBlockInner,
        reason: Option<ReschedReason>,
    ) {
        task_inner.sched.resched_reason = reason;
        #[cfg(feature = "return_work_cache")]
        if reason.is_some() {
            self.return_work
                .fetch_or(RETURN_WORK_RESCHED, Ordering::Release);
        } else {
            self.return_work
                .fetch_and(!RETURN_WORK_RESCHED, Ordering::AcqRel);
        }
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
    /// Whether this task may still have non-futex timers that require eager removal on exit.
    pub may_have_non_futex_timer: bool,
    /// Debug: the last scheduler-container transition that touched this task.
    /// Updated best-effort at each enqueue/dequeue/wake/block/remove site via
    /// [`TaskControlBlockInner::stamp_sched`]. Dumped by the lost-runnable
    /// detector so an orphaned task's final operation is visible. Zero runtime
    /// cost except at transitions; produces no log output on its own.
    pub last_sched_op: LastSchedOp,
}

/// Record of the last scheduler transition that touched a task, used to
/// diagnose lost-runnable orphans. `op` names the transition, `hart` is the
/// hart that performed it, `status`/`on_rq` are the resulting task state, and
/// `seq` is a per-task monotonic counter so the freshness of the stamp is
/// visible. `Default` is the "just constructed" stamp.
#[derive(Clone, Copy, Debug)]
pub struct LastSchedOp {
    /// Short name of the transition (e.g. `"enqueue_wake"`, `"dequeue_run"`).
    pub op: &'static str,
    /// Hart that performed the transition.
    pub hart: usize,
    /// Task status as observed *after* the transition.
    pub status: TaskStatus,
    /// `sched.on_rq` as observed after the transition.
    pub on_rq: bool,
    /// Per-task monotonic sequence number, so freshness of the stamp is visible.
    pub seq: u32,
}

impl Default for LastSchedOp {
    fn default() -> Self {
        Self {
            op: "init",
            hart: 0,
            status: TaskStatus::Runnable,
            on_rq: false,
            seq: 0,
        }
    }
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
        alloc_user_res: TaskUserResAlloc,
        sched_attr: SchedAttr,
    ) -> Result<Self, MmError> {
        let new_start_ns = get_time_ns();
        let task_user_res_start_ns = get_time_ns();
        let res = TaskUserRes::new(Arc::clone(&process), ustack_base, alloc_user_res)?;
        let task_user_res_ns = get_time_ns() - task_user_res_start_ns;
        let trap_cx_ppn = res.trap_cx_ppn();
        #[cfg(feature = "trap_context_cache")]
        let trap_cx_user_va = res.trap_cx_user_va();
        #[cfg(feature = "trap_context_cache")]
        let user_token = process.inner_exclusive_access().get_user_token();
        let tid = res.tid;
        let thread_id = res.thread_id();
        let kstack_alloc_start_ns = get_time_ns();
        let kstack = kstack_alloc()?;
        let kstack_alloc_ns = get_time_ns() - kstack_alloc_start_ns;
        let kstack_top = kstack.get_top();
        let build_start_ns = get_time_ns();
        let task = Self {
            process: Arc::downgrade(&process),
            kstack,
            on_cpu: AtomicBool::new(false),
            cpu_accounting_stamp: AtomicUsize::new(pack_cpu_accounting(
                CPU_ACCOUNTING_INACTIVE,
                0,
            )),
            return_work: AtomicU32::new(0),
            #[cfg(feature = "trap_context_cache")]
            trap_cx_ppn_cache: AtomicUsize::new(trap_cx_ppn.0),
            #[cfg(feature = "trap_context_cache")]
            trap_cx_user_va_cache: trap_cx_user_va,
            #[cfg(feature = "trap_context_cache")]
            user_token_cache: AtomicUsize::new(user_token),
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
                may_have_non_futex_timer: false,
                last_sched_op: LastSchedOp::default(),
            }),
        };
        let build_ns = get_time_ns() - build_start_ns;
        let total_ns = get_time_ns() - new_start_ns;
        if total_ns >= TASK_CONTROL_BLOCK_NEW_TIMING_WARN_THRESHOLD_NS {
            debug!(
                "[clone-timing] task_control_block_new pid={} tid={} thread_id={} alloc_user_res={} total_ns={} task_user_res_ns={} kstack_alloc_ns={} build_ns={}",
                process.getpid(),
                tid,
                thread_id,
                alloc_user_res as u8,
                total_ns,
                task_user_res_ns,
                kstack_alloc_ns,
                build_ns,
            );
        }
        Ok(task)
    }

    /// Create a kernel thread task that starts at `entry` and never returns to userspace.
    pub fn new_kernel_thread(
        process: Arc<ProcessControlBlock>,
        entry: fn() -> !,
        sched_attr: SchedAttr,
    ) -> Result<Self, MmError> {
        let kstack = kstack_alloc()?;
        let kstack_top = kstack.get_top();
        Ok(Self {
            process: Arc::downgrade(&process),
            kstack,
            on_cpu: AtomicBool::new(false),
            cpu_accounting_stamp: AtomicUsize::new(pack_cpu_accounting(
                CPU_ACCOUNTING_INACTIVE,
                0,
            )),
            return_work: AtomicU32::new(0),
            #[cfg(feature = "trap_context_cache")]
            trap_cx_ppn_cache: AtomicUsize::new(0),
            #[cfg(feature = "trap_context_cache")]
            trap_cx_user_va_cache: 0,
            #[cfg(feature = "trap_context_cache")]
            user_token_cache: AtomicUsize::new(0),
            inner: SpinNoIrqLock::new(TaskControlBlockInner {
                res: None,
                trap_cx_ppn: PhysPageNum(0),
                task_cx: TaskContext::goto_kernel_entry(entry, kstack_top),
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
                may_have_non_futex_timer: false,
                last_sched_op: LastSchedOp::default(),
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
    /// Waiting for a POSIX file lock to become available.
    FileLock,
    /// Waiting on a Linux futex word: `(user_address, expected_value)`.
    Futex(usize, i32),
    /// Parent is waiting for a child selected by the wait4/waitpid `pid` argument.
    ProcessWaitExit(isize),
    /// Waiting for UART RX data.
    UartRx,
    /// Waiting for pipe to become readable.
    PipeReadable,
    /// Waiting for pipe to become writable.
    PipeWritable,
    /// Waiting for an eventfd counter to become readable.
    EventFdReadable,
    /// Waiting for an eventfd counter to have writable capacity.
    EventFdWritable,
    /// Waiting for nanosleep timer expiration.
    Nanosleep,
    /// Waiting for block device I/O completion.
    BlockDeviceIo,
    /// Waiting for the active page-cache direct reclaimer to release ownership.
    PageCacheReclaim,
    /// Background page-cache readahead worker waiting for queued work.
    PageCacheReadahead,
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

#[derive(Copy, Clone, PartialEq, Debug)]
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
