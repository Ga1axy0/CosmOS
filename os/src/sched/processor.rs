//! Implementation of [`Processor`] and Intersection of control flow
//!
//! Here, the continuous operation of user apps in CPU is maintained,
//! the current running state of CPU is recorded,
//! and the replacement and transfer of control flow of different applications are executed.

use super::__switch;
use super::{add_task, pick_next_task, TaskContext};
use crate::config::MAX_HARTS;
use crate::hal::traits::AddressSpaceToken;
use crate::hal::{enable_irqs_and_wait, hartid};
use crate::sync::SpinNoIrqLock;
use crate::task::{ProcessControlBlock, SchedPolicy, TaskControlBlock, TaskStatus, INITPROC};
use crate::timer::get_time;
use crate::trap::TrapContext;
use alloc::sync::Arc;
use core::array;
use core::sync::atomic::Ordering;
use lazy_static::*;

/// Processor management structure
pub struct Processor {
    current: Option<Arc<TaskControlBlock>>,
    pending_task_release: Option<Arc<TaskControlBlock>>,

    ///The basic control flow of each core, helping to select and switch process
    idle_task_cx: TaskContext,
}

impl Processor {
    pub fn new() -> Self {
        Self {
            current: None,
            pending_task_release: None,
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

    /// Identity of the current task on this hart for the debug invariant
    /// checker (raw pointer value, no refcount bump).
    #[cfg(feature = "sched_invariant_checks")]
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
}

lazy_static! {
    pub static ref PROCESSORS: [SpinNoIrqLock<Processor>; MAX_HARTS] =
        array::from_fn(|_| SpinNoIrqLock::new(Processor::new()));
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
            // debug!(
            //     "kernel: hart {} run_tasks, pid[{}]",
            //     hartid(),
            //     task.process.upgrade().unwrap().getpid()
            // );
            let process = task.process.upgrade().unwrap();
            let mut processor = current_processor().lock();
            let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();

            let mut task_inner = task.inner_exclusive_access();
            let next_task_cx_ptr = &task_inner.task_cx as *const TaskContext;
            task_inner.task_status = TaskStatus::Running;
            task_inner.wait_reason = None;
            task_inner.sched.last_cpu = hartid();
            task.on_cpu.store(true, Ordering::Relaxed);
            task_inner.sched.on_rq = false;
            task_inner.sched.resched_reason = None;
            if matches!(task_inner.sched.policy, SchedPolicy::Other) {
                let now_ns = crate::timer::get_time_ns();
                task_inner.sched.exec_start_ns = now_ns;
                task_inner.sched.cfs_slice_start_ns = now_ns;
            }
            drop(task_inner);

            processor.current = Some(task);
            drop(processor);
            process.resume_in_kernel(get_time());

            unsafe {
                __switch(idle_task_cx_ptr, next_task_cx_ptr);
            }
            finish_pending_task_release();
        } else {
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
    current_processor().lock().take_current()
}

/// Get a copy of the current task
pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    current_processor().lock().current()
}

/// get current process
pub fn current_process() -> Arc<ProcessControlBlock> {
    current_task().unwrap().process.upgrade().unwrap()
}

/// Get the current user address-space token.
pub fn current_user_token() -> AddressSpaceToken {
    let task = current_task().unwrap();
    task.get_user_token()
}

/// Get the mutable reference to trap context of current task
pub fn current_trap_cx() -> &'static mut TrapContext {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .get_trap_cx()
}

/// get the user virtual address of trap context
pub fn current_trap_cx_user_va() -> usize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .trap_cx_user_va()
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
    unsafe {
        __switch(switched_task_cx_ptr, idle_task_cx_ptr);
    }
    if irqs_were_enabled {
        unsafe { crate::hal::enable_local_irqs() };
    }
}
