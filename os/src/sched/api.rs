//! Scheduling control-flow entry points.

use super::{
    boost_process_cfs_tasks, cfs_should_preempt, current_process, current_processor, current_task,
    defer_task_release_after_switch, has_runnable_task_at_or_above, schedule, take_current_task,
    TaskContext,
};
use crate::hal::hartid;
use crate::sched::CFS_YIELD_PENALTY_NS;
use crate::task::{ReschedReason, SchedPolicy, TaskStatus, WaitReason};
use crate::timer::{get_time, get_time_ns};
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

fn suspend_current_and_run_next_inner(
    apply_cfs_yield_penalty: bool,
    reset_slice: bool,
    rt_enqueue_head: Option<bool>,
) {
    current_process().pause_cpu_accounting(get_time());
    // IRQ-atomic transition window — see `block_current_and_run_next` for the
    // rationale. `take_current_task()` clears `processor.current` while
    // `on_cpu` is only dropped later by `finish_pending_task_release()` after
    // `__switch`; keep local interrupts disabled across that window so no
    // same-hart hardirq can wake the half-suspended task. (The confirmed
    // deadlock is the `Interruptible` case in `block_current_and_run_next`;
    // this path marks the task `Runnable`, but we close the identical window
    // for consistency and to guard against future regressions.)
    let _irq = crate::hal::LocalIrqSave::new();
    let task = take_current_task().unwrap();
    let task_cx_ptr = {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.account_cfs_runtime(get_time_ns());
        if matches!(task_inner.task_status, TaskStatus::Zombie) {
            task_inner.sched.on_rq = false;
            task_inner.wait_reason = None;
            task_inner.sched.resched_reason = None;
        } else {
            task_inner.sched.on_rq = false;
            task_inner.task_status = TaskStatus::Runnable;
            task_inner.wait_reason = None;
            task_inner.sched.resched_reason = None;
            if reset_slice {
                task_inner.reset_time_slice();
            }
            if task_inner.sched.policy.is_rt() {
                if let Some(rt_enqueue_head) = rt_enqueue_head {
                    task_inner.sched.rt_enqueue_head = rt_enqueue_head;
                }
            }
            if apply_cfs_yield_penalty && matches!(task_inner.sched.policy, SchedPolicy::Other) {
                task_inner.sched.vruntime_ns = task_inner
                    .sched
                    .vruntime_ns
                    .saturating_add(CFS_YIELD_PENALTY_NS);
            }
        }
        &mut task_inner.task_cx as *mut TaskContext
    };
    defer_task_release_after_switch(task);
    schedule(task_cx_ptr);
}

/// Make current task suspended and switch to the next task.
pub fn suspend_current_and_run_next() {
    suspend_current_and_run_next_inner(false, false, Some(false));
}

/// Make current CFS task yield by charging a small vruntime penalty.
pub fn yield_current_and_run_next() {
    suspend_current_and_run_next_inner(true, false, Some(false));
}

/// Make current task suspended and optionally replenish its RR time slice.
pub fn suspend_current_and_run_next_with_slice_reset(reset_slice: bool) {
    suspend_current_and_run_next_inner(false, reset_slice, Some(false));
}

/// Make current task blocked and switch to the next task.
pub fn block_current_and_run_next(reason: WaitReason) {
    // IRQ-atomic transition window.
    //
    // `take_current_task()` clears `processor.current` but leaves the task's
    // `on_cpu` set; it is only cleared later by `finish_pending_task_release()`
    // once this hart returns to its idle loop after `__switch`. If a timer
    // interrupt fires on THIS hart during that window, its handler calls
    // `wakeup_task()` on the half-blocked task, which spins on `on_cpu` — and
    // since the hart that must clear `on_cpu` is now pinned inside that very
    // hardirq, the spin never ends (self-deadlock, 100% CPU). Keep local
    // interrupts disabled across the whole `take_current_task() .. schedule()`
    // window so no same-hart hardirq can observe the half-state. The guard
    // restores the saved state on drop, including when the caller is resumed
    // after the context switch.
    let _irq = crate::hal::LocalIrqSave::new();
    let task = take_current_task().unwrap();
    let mut boost_same_process_cfs = false;
    let task_cx_ptr = {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.account_cfs_runtime(get_time_ns());
        if matches!(task_inner.task_status, TaskStatus::Runnable) {
            task_inner.task_status = TaskStatus::Running;
            task_inner.wait_reason = None;
            task.on_cpu.store(true, Ordering::Relaxed);
            #[cfg(feature = "sched_invariant_checks")]
            assert!(
                !task_inner.sched.on_rq,
                "[sched-inv] block_current race on hart {}: task is Runnable here but on_rq=true \
                 — a remote wakeup enqueued us while we were still current, because \
                 take_current_task() had already cleared processor.current before we committed \
                 to not switching. This is the SMP wake/block race.",
                crate::hal::hartid(),
            );
            task_inner.sched.on_rq = false;
            task_inner.sched.resched_reason = None;
            None
        } else {
            task_inner.sched.on_rq = false;
            task_inner.task_status = TaskStatus::Interruptible;
            task_inner.wait_reason = Some(reason);
            task_inner.sched.resched_reason = None;
            boost_same_process_cfs =
                task_inner.sched.policy.is_rt() && matches!(reason, WaitReason::Nanosleep);
            Some(&mut task_inner.task_cx as *mut TaskContext)
        }
    };
    if task_cx_ptr.is_none() {
        current_processor().lock().set_current(task);
        return;
    }
    let process = task.process.upgrade().unwrap();
    if boost_same_process_cfs {
        let boost_candidates = {
            let process_inner = process.inner_exclusive_access();
            process_inner
                .tasks
                .iter()
                .filter_map(|task| task.as_ref().cloned())
                .collect::<Vec<_>>()
        };
        boost_process_cfs_tasks(hartid(), boost_candidates.as_slice());
    }
    process.pause_cpu_accounting(get_time());
    defer_task_release_after_switch(task);
    schedule(task_cx_ptr.unwrap());
}

/// Mark the current task for deferred rescheduling.
pub fn mark_current_task_need_resched() {
    request_current_task_resched(ReschedReason::CfsPreempt);
}

/// Mark the current task for deferred rescheduling with a concrete reason.
pub fn request_current_task_resched(reason: ReschedReason) {
    if let Some(task) = current_task() {
        task.inner_exclusive_access().sched.resched_reason = Some(reason);
    }
}

/// Returns whether the current task has a pending reschedule request.
pub fn current_task_need_resched() -> bool {
    current_task()
        .map(|task| task.inner_exclusive_access().sched.resched_reason.is_some())
        .unwrap_or(false)
}

/// Handle deferred rescheduling at a safe scheduling point.
pub fn schedule_if_needed() {
    let reason = current_task().and_then(|task| task.inner_exclusive_access().sched.resched_reason);
    let Some(reason) = reason else {
        return;
    };
    match reason {
        ReschedReason::HigherRtPriority => {
            suspend_current_and_run_next_inner(false, false, Some(true));
        }
        ReschedReason::RrTimesliceExpired => {
            suspend_current_and_run_next_inner(false, true, Some(false));
        }
        ReschedReason::Yield => {
            suspend_current_and_run_next_inner(true, false, Some(false));
        }
        ReschedReason::CfsPreempt | ReschedReason::Migration => {
            suspend_current_and_run_next_inner(false, false, None);
        }
    }
}

/// Account one timer tick for the current RR task and request rescheduling if its slice expires.
pub fn on_timer_tick() {
    let Some(task) = current_task() else {
        return;
    };
    let mut task_inner = task.inner_exclusive_access();
    if !matches!(task_inner.task_status, TaskStatus::Running) {
        return;
    }
    match task_inner.sched.policy {
        SchedPolicy::Fifo => {
            let prio = task_inner.sched.rt_priority;
            if has_runnable_task_at_or_above(hartid(), prio.saturating_add(1)) {
                task_inner.sched.resched_reason = Some(ReschedReason::HigherRtPriority);
            }
        }
        SchedPolicy::Rr => {
            if task_inner.sched.remaining_slice_ticks > 0 {
                task_inner.sched.remaining_slice_ticks -= 1;
            }
            if task_inner.sched.remaining_slice_ticks > 0 {
                return;
            }
            let prio = task_inner.sched.rt_priority;
            if has_runnable_task_at_or_above(hartid(), prio) {
                task_inner.sched.resched_reason = Some(ReschedReason::RrTimesliceExpired);
            } else {
                task_inner.reset_time_slice();
            }
        }
        SchedPolicy::Other => {
            let now_ns = get_time_ns();
            task_inner.account_cfs_runtime(now_ns);
            let slice_exec = now_ns.saturating_sub(task_inner.sched.cfs_slice_start_ns);
            if cfs_should_preempt(
                hartid(),
                task_inner.sched.vruntime_ns,
                task_inner.sched.weight,
                slice_exec,
            ) {
                task_inner.sched.resched_reason = Some(ReschedReason::CfsPreempt);
            }
        }
        SchedPolicy::Idle => {}
    }
}
