//! Scheduler module.
//!
//! This module owns CPU-local scheduling state and context switching
//! primitives. Task and process object definitions remain under `task`.

mod api;
mod context;
mod policy;
mod processor;
mod runqueue;
mod switch;

pub use api::{
    block_current_and_run_next, current_task_need_resched, mark_current_task_need_resched,
    on_timer_tick, request_current_task_resched, schedule_if_needed, suspend_current_and_run_next,
    suspend_current_and_run_next_with_slice_reset, yield_current_and_run_next,
};
pub use context::TaskContext;
pub use policy::{
    clamp_nice, nice_to_weight, ReschedReason, SchedAttr, SchedPolicy, CFS_MIN_GRANULARITY_NS,
    CFS_TARGET_LATENCY_NS, CFS_WAKEUP_GRANULARITY_NS, CFS_YIELD_PENALTY_NS,
    DEFAULT_TIME_SLICE_TICKS, MAX_NICE, MIN_NICE, NICE_0_LOAD, SCHED_RT_PRIO_MAX,
    SCHED_RT_PRIO_MIN,
};
pub(crate) use processor::{
    current_kstack_top, current_processor, defer_task_release_after_switch, run_tasks, schedule,
    take_current_task,
};
pub use processor::{
    current_process, current_task, current_trap_cx, current_trap_cx_user_va, current_user_token,
};
pub use runqueue::wakeup_task;
pub(crate) use runqueue::{
    add_stopping_task, add_task, cfs_should_preempt, enqueue_task_on,
    has_runnable_task_at_or_above, insert_into_pid2process, list_pids, pick_next_task, pid2process,
    remove_from_pid2process, remove_task, resched_hart, PID2PCB,
};
pub use switch::__switch;
