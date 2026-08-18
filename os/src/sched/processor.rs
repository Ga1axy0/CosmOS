//! Implementation of [`Processor`] and Intersection of control flow
//!
//! Here, the continuous operation of user apps in CPU is maintained,
//! the current running state of CPU is recorded,
//! and the replacement and transfer of control flow of different applications are executed.

use super::__switch;
use super::{add_task, pick_next_task, TaskContext};
use crate::config::MAX_HARTS;
use crate::hal::traits::AddressSpaceToken;
use crate::hal::{
    activate_address_space, current_address_space_token, enable_irqs_and_wait, hartid,
};
use crate::mm::AddressSpaceRoot;
use crate::sync::SpinNoIrqLock;
use crate::task::{ProcessControlBlock, SchedPolicy, TaskControlBlock, TaskStatus, INITPROC};
use crate::timer::get_time;
use crate::trap::TrapContext;
use alloc::sync::Arc;
use core::array;
#[cfg(feature = "current_task_cache")]
use core::ptr;
#[cfg(feature = "current_task_cache")]
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::Ordering;
use core::sync::atomic::AtomicUsize;
use lazy_static::*;

static DIAG_SCHED_EVENTS: AtomicUsize = AtomicUsize::new(0);
static DIAG_IDLE_EVENTS: AtomicUsize = AtomicUsize::new(0);

/// Processor management structure
pub struct Processor {
    current: Option<Arc<TaskControlBlock>>,
    pending_task_release: Option<Arc<TaskControlBlock>>,
    /// Address space currently installed on this hart.
    ///
    /// Idle borrows the last process root instead of switching through the
    /// permanent kernel page table.  The strong root guard prevents exit/exec
    /// from reclaiming a root that is still loaded by the hardware walker.
    active_address_space: Option<AddressSpaceRoot>,

    ///The basic control flow of each core, helping to select and switch process
    idle_task_cx: TaskContext,
}

impl Processor {
    pub fn new() -> Self {
        Self {
            current: None,
            pending_task_release: None,
            active_address_space: None,
            idle_task_cx: TaskContext::zero_init(),
        }
    }

    ///Get mutable reference to `idle_task_cx`
    fn get_idle_task_cx_ptr(&mut self) -> *mut TaskContext {
        &mut self.idle_task_cx as *mut _
    }

    ///Get current task in moving semanteme
    pub fn take_current(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.current.take()
    }

    ///Get current task in cloning semanteme
    pub fn current(&self) -> Option<Arc<TaskControlBlock>> {
        self.current.as_ref().map(Arc::clone)
    }

    pub fn set_current(&mut self, task: Arc<TaskControlBlock>) {
        self.current = Some(task);
    }

    pub(super) fn current_ptr(&self) -> Option<usize> {
        self.current.as_ref().map(|t| Arc::as_ptr(t) as usize)
    }

    fn set_pending_task_release(&mut self, task: Arc<TaskControlBlock>) {
        assert!(self.pending_task_release.is_none());
        self.pending_task_release = Some(task);
    }

    fn take_pending_task_release(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.pending_task_release.take()
    }

    fn replace_active_address_space(&mut self, next: AddressSpaceRoot) -> Option<AddressSpaceRoot> {
        self.active_address_space.replace(next)
    }
}

lazy_static! {
    pub static ref PROCESSORS: [SpinNoIrqLock<Processor>; MAX_HARTS] =
        array::from_fn(|_| SpinNoIrqLock::new(Processor::new()));
}

#[cfg(feature = "current_task_cache")]
static CURRENT_TASK_PTRS: [AtomicPtr<TaskControlBlock>; MAX_HARTS] =
    [const { AtomicPtr::new(ptr::null_mut()) }; MAX_HARTS];

#[cfg(feature = "current_task_cache")]
#[inline]
fn publish_current_task(task: Option<&Arc<TaskControlBlock>>) {
    let ptr = task.map_or(ptr::null_mut(), |task| Arc::as_ptr(task).cast_mut());
    CURRENT_TASK_PTRS[hartid()].store(ptr, Ordering::Release);
}

#[cfg(feature = "current_task_cache")]
#[inline]
fn current_task_ptr() -> *mut TaskControlBlock {
    CURRENT_TASK_PTRS[hartid()].load(Ordering::Acquire)
}

/// 返回当前 hart 对应的 `Processor` 存储入口。
///
/// 这里会根据 `hartid()` 选择 `PROCESSORS[hartid]`，从而让“当前任务”
/// 与“idle 调度上下文”都变成每个 hart 独立维护的状态。
pub fn current_processor() -> &'static SpinNoIrqLock<Processor> {
    processor_for_hart(hartid())
}

/// 返回指定 hart 对应的 `Processor` 存储入口。
pub fn processor_for_hart(hart_id: usize) -> &'static SpinNoIrqLock<Processor> {
    PROCESSORS
        .get(hart_id)
        .unwrap_or_else(|| panic!("hart {} exceeds MAX_HARTS {}", hart_id, MAX_HARTS))
}

/// Install and pin one address space on the current hart.
///
/// The caller supplies a strong root guard before the hardware token changes.
/// The previous guard is released only after the new root is active, so exit
/// and exec cannot recycle a page-table root underneath the hardware walker.
#[inline]
pub(crate) fn activate_current_address_space(next: AddressSpaceRoot) {
    let token = next.token();
    let irqs_were_enabled = crate::hal::local_irqs_enabled();
    if irqs_were_enabled {
        unsafe { crate::hal::disable_local_irqs() };
    }
    unsafe {
        if current_address_space_token() != token {
            activate_address_space(token);
        }
    }
    let previous = current_processor()
        .lock()
        .replace_active_address_space(next);
    drop(previous);
    if irqs_were_enabled {
        unsafe { crate::hal::enable_local_irqs() };
    }
}

///The main part of process execution and scheduling
///Loop `fetch_task` to get the process that needs to run, and switch the process through `__switch`
pub(crate) fn run_tasks() {
    loop {
        #[cfg(feature = "sched_invariant_checks")]
        if crate::hal::hartid() == 0 {
            crate::sched::check_sched_invariants();
        }
        // Drop any stopped-task reference left by the previous exit on this hart.
        // The previous task's kernel stack is now guaranteed unused.
        super::clear_stopping_task();
        crate::task::maybe_dump_pending_debug_pgrp_tasks();
        if let Some(task) = pick_next_task(hartid()) {
            let event = DIAG_SCHED_EVENTS.fetch_add(1, Ordering::Relaxed);
            if event < 32 {
                let pid = task
                    .process
                    .upgrade()
                    .map(|process| process.getpid())
                    .unwrap_or(usize::MAX);
                debug!(
                    "[diag][sched] pick event={} hart={} task={:#x} pid={}",
                    event,
                    hartid(),
                    Arc::as_ptr(&task) as usize,
                    pid,
                );
            }
            // debug!(
            //     "kernel: hart {} run_tasks, pid[{}]",
            //     hartid(),
            //     task.process.upgrade().unwrap().getpid()
            // );
            // dequeue_task() publishes on_cpu=true while holding task-inner,
            // but a remote exec/exit owner may mark the task Zombie before we
            // get here.  Commit the Runnable -> Running transition under the
            // same lock and refuse to resurrect a stopped task.  Releasing
            // on_cpu after this check is also the handoff that lets teardown
            // reclaim a task which was selected but never switched to.
            let next_task_cx_ptr = {
                let mut task_inner = task.inner_exclusive_access();
                if task_inner.exit_code.is_some()
                    || !matches!(task_inner.task_status, TaskStatus::Runnable)
                {
                    if task_inner.exit_code.is_some() {
                        task_inner.task_status = TaskStatus::Zombie;
                    }
                    task_inner.sched.on_rq = false;
                    task.on_cpu.store(false, Ordering::Release);
                    continue;
                }
                task_inner.task_status = TaskStatus::Running;
                task_inner.wait_reason = None;
                task_inner.sched.last_cpu = hartid();
                task_inner.sched.on_rq = false;
                task.set_resched_reason_locked(&mut task_inner, None);
                if matches!(task_inner.sched.policy, SchedPolicy::Other) {
                    let now_ns = crate::timer::get_time_ns();
                    task_inner.sched.exec_start_ns = now_ns;
                    task_inner.sched.cfs_slice_start_ns = now_ns;
                }
                &task_inner.task_cx as *const TaskContext
            };

            let process = task.process.upgrade().unwrap();
            // Read the PCB's authoritative token instead of the task's cached
            // trap metadata. During exec the MemorySet is replaced before the
            // new trap frame/cache is fully constructed, and this task may be
            // preempted inside that interval.
            let has_user_context = task.has_user_context();
            let next_address_space = {
                let process_inner = process.inner_exclusive_access();
                // The address space remains in the active mask across normal
                // traps. Activate it once when a task is installed on this
                // hart; the matching scheduler pause clears it.
                #[cfg(not(feature = "trap_active_harts_probe"))]
                if has_user_context {
                    process_inner.memory_set.mark_user_active(hartid());
                }
                process_inner.memory_set.address_space_root()
            };
            let mut processor = current_processor().lock();
            let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();

            process.resume_in_kernel(task.as_ref(), get_time());
            super::bais::on_task_running(&task, hartid());
            processor.current = Some(task);
            #[cfg(feature = "current_task_cache")]
            publish_current_task(processor.current.as_ref());
            drop(processor);
            // Switch directly from the previously borrowed process root to the
            // next one. Same-address-space scheduling performs no CSR write.
            activate_current_address_space(next_address_space);
            unsafe {
                __switch(idle_task_cx_ptr, next_task_cx_ptr);
            }
            finish_pending_task_release();
        } else {
            let event = DIAG_IDLE_EVENTS.fetch_add(1, Ordering::Relaxed);
            if event < 16 {
                debug!("[diag][sched] idle event={} hart={}", event, hartid());
            }
            // idle: enable interrupts and wait for next interrupt (timer/UART/etc.)
            if INITPROC.inner_exclusive_access().is_zombie() {
                info!("Goodbye!");
                crate::sbi::shutdown();
            }

            // debug!("No task to run, idle...");
            if !crate::platform::console_rx_irq_ready() {
                // Keep the old cooperative polling path only as a pre-init
                // fallback before the EXTIOI/PCH-PIC chain is configured.
                crate::fs::console_receive();
            }

            // A task can become Runnable without being owned by either a
            // runqueue or a hart if a wake/block transition loses the enqueue.
            // Scan only from the idle path, where no local task can make
            // progress anyway; the scanner is internally rate-limited and
            // re-enqueues every orphan it finds.
            // Disabled while investigating an SMP lockup: this diagnostic
            // scanner acquires process/task locks from the idle path and can
            // contend with the scheduler's blocking path.
            // super::warn_lost_runnable_tasks("idle_no_task");

            crate::trap::set_kernel_trap_entry();

            unsafe { enable_irqs_and_wait() };
        }
    }
}

pub(crate) fn defer_task_release_after_switch(task: Arc<TaskControlBlock>) {
    current_processor().lock().set_pending_task_release(task);
}

fn finish_pending_task_release() {
    let Some(task) = current_processor().lock().take_pending_task_release() else {
        return;
    };
    let should_requeue = {
        let mut task_inner = task.inner_exclusive_access();
        // Post-switch: the task's registers are now safely saved. Publish
        // on_cpu=false with Release so any remote waker that observes it via
        // an Acquire load can safely enqueue and switch into this task.
        task.on_cpu.store(false, Ordering::Release);
        task_inner.sched.on_rq = false;
        matches!(task_inner.task_status, TaskStatus::Runnable)
    };
    if should_requeue {
        add_task(task);
    }
}

/// Get current task through take, leaving a None in its place
pub(crate) fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    let mut processor = current_processor().lock();
    let task = processor.take_current();
    #[cfg(feature = "current_task_cache")]
    publish_current_task(None);
    task
}

/// Restore ownership of the running task after a block attempt was cancelled.
pub(crate) fn restore_current_task(task: Arc<TaskControlBlock>) {
    let mut processor = current_processor().lock();
    processor.set_current(task);
    #[cfg(feature = "current_task_cache")]
    publish_current_task(processor.current.as_ref());
}

/// Get a copy of the current task
#[cfg(not(feature = "current_task_cache"))]
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    current_processor().lock().current()
}

/// Get a copy of the current task without taking Processor's spinlock.
///
/// The Processor-held `Arc` remains the ownership source. Only the owning hart
/// publishes or clears this pointer, with local interrupts disabled by the
/// Processor lock. CosmOS does not preempt executing kernel code, so this hart
/// cannot drop that owner between the load and strong-count increment.
#[cfg(feature = "current_task_cache")]
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    let ptr = current_task_ptr();
    if ptr.is_null() {
        return None;
    }
    unsafe {
        Arc::increment_strong_count(ptr);
        Some(Arc::from_raw(ptr))
    }
}

/// get current process
#[cfg(not(feature = "process_identity_cache"))]
pub fn current_process() -> Arc<ProcessControlBlock> {
    current_task().unwrap().process.upgrade().unwrap()
}

/// Get the current process without first constructing a temporary task Arc.
///
/// The Processor-owned task Arc cannot disappear while this hart executes
/// non-preemptible kernel code.  The process itself is still returned as an
/// owned Arc through the task's Weak pointer.
#[cfg(feature = "process_identity_cache")]
pub fn current_process() -> Arc<ProcessControlBlock> {
    let ptr = current_task_ptr();
    assert!(!ptr.is_null(), "current process requested without a task");
    unsafe { (*ptr).process.upgrade().unwrap() }
}

/// Get the current user address-space token.
pub fn current_user_token() -> AddressSpaceToken {
    let task = current_task().unwrap();
    task.get_user_token()
}

/// Get the mutable reference to trap context of current task
pub fn current_trap_cx() -> &'static mut TrapContext {
    current_task().unwrap().trap_cx()
}

/// get the user virtual address of trap context
pub fn current_trap_cx_user_va() -> usize {
    current_task().unwrap().trap_cx_user_va()
}

/// get the top addr of kernel stack
pub(crate) fn current_kstack_top() -> usize {
    current_task().unwrap().kstack.get_top()
}

/// Return to idle control flow for new scheduling
pub(crate) fn schedule(switched_task_cx_ptr: *mut TaskContext) {
    let irqs_were_enabled = crate::hal::local_irqs_enabled();
    if irqs_were_enabled {
        unsafe { crate::hal::disable_local_irqs() };
    }
    let mut processor = current_processor().lock();
    let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
    drop(processor);
    // Lazy active-mm: idle keeps the outgoing process root installed. Every
    // process root contains the kernel mappings needed by the scheduler, and
    // Processor::active_address_space pins the root until a direct switch to a
    // different address space has completed.
    unsafe {
        __switch(switched_task_cx_ptr, idle_task_cx_ptr);
    }
    if irqs_were_enabled {
        unsafe { crate::hal::enable_local_irqs() };
    }
}
