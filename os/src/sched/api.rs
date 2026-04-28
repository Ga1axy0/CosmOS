//! Scheduling control-flow entry points.

use super::{
    add_task, current_process, current_processor, current_task,
    has_runnable_task_at_or_above, schedule, take_current_task, TaskContext,
};
use crate::hart::hartid;
use crate::task::{SchedPolicy, TaskStatus, WaitReason};
use crate::timer::get_time;

/// Make current task suspended and switch to the next task.
pub fn suspend_current_and_run_next() {
    current_process().pause_cpu_accounting(get_time());
    let task = take_current_task().unwrap();
    let task_cx_ptr = {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.sched.on_cpu = false;
        task_inner.sched.on_rq = false;
        task_inner.task_status = TaskStatus::Runnable;
        task_inner.wait_reason = None;
        task_inner.sched.need_resched = false;
        &mut task_inner.task_cx as *mut TaskContext
    };
    add_task(task);
    schedule(task_cx_ptr);
}

/// Make current task suspended and optionally replenish its RR time slice.
pub fn suspend_current_and_run_next_with_slice_reset(reset_slice: bool) {
    current_process().pause_cpu_accounting(get_time());
    let task = take_current_task().unwrap();
    let task_cx_ptr = {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.sched.on_cpu = false;
        task_inner.sched.on_rq = false;
        task_inner.task_status = TaskStatus::Runnable;
        task_inner.wait_reason = None;
        task_inner.sched.need_resched = false;
        if reset_slice {
            task_inner.reset_time_slice();
        }
        &mut task_inner.task_cx as *mut TaskContext
    };
    add_task(task);
    schedule(task_cx_ptr);
}

/// Make current task blocked and switch to the next task.
pub fn block_current_and_run_next(reason: WaitReason) {
    let task = take_current_task().unwrap();
    let task_cx_ptr = {
        let mut task_inner = task.inner_exclusive_access();
        if matches!(task_inner.task_status, TaskStatus::Runnable) {
            task_inner.task_status = TaskStatus::Running;
            task_inner.wait_reason = None;
            task_inner.sched.on_cpu = true;
            task_inner.sched.on_rq = false;
            task_inner.sched.need_resched = false;
            None
        } else {
            task_inner.sched.on_cpu = false;
            task_inner.sched.on_rq = false;
            task_inner.task_status = TaskStatus::Interruptible;
            task_inner.wait_reason = Some(reason);
            task_inner.sched.need_resched = false;
            Some(&mut task_inner.task_cx as *mut TaskContext)
        }
    };
    if task_cx_ptr.is_none() {
        current_processor().lock().set_current(task);
        return;
    }
    let process = task.process.upgrade().unwrap();
    process.pause_cpu_accounting(get_time());
    schedule(task_cx_ptr.unwrap());
}

/// Mark the current task for deferred rescheduling.
pub fn mark_current_task_need_resched() {
    if let Some(task) = current_task() {
        task.inner_exclusive_access().sched.need_resched = true;
    }
}

/// Returns whether the current task has a pending reschedule request.
pub fn current_task_need_resched() -> bool {
    current_task()
        .map(|task| task.inner_exclusive_access().sched.need_resched)
        .unwrap_or(false)
}

/// Handle deferred rescheduling at a safe scheduling point.
pub fn schedule_if_needed() {
    if current_task_need_resched() {
        suspend_current_and_run_next();
    }
}

/// Account one timer tick for the current RR task and request rescheduling if its slice expires.
pub fn on_timer_tick() {
    let Some(task) = current_task() else {
        return;
    };
    let mut task_inner = task.inner_exclusive_access();
    if !matches!(task_inner.task_status, TaskStatus::Running)
        || !matches!(task_inner.sched.policy, SchedPolicy::Rr)
    {
        return;
    }
    if task_inner.sched.remaining_slice_ticks > 0 {
        task_inner.sched.remaining_slice_ticks -= 1;
    }
    if task_inner.sched.remaining_slice_ticks > 0 {
        return;
    }
    let prio = task_inner.sched.rt_priority;
    task_inner.reset_time_slice();
    if has_runnable_task_at_or_above(hartid(), prio) {
        task_inner.sched.need_resched = true;
    }
}
