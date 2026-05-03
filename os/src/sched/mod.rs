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
    on_timer_tick, schedule_if_needed, suspend_current_and_run_next,
    suspend_current_and_run_next_with_slice_reset,
};
pub use context::TaskContext;
pub use policy::{
    DEFAULT_TIME_SLICE_TICKS, SchedAttr, SchedPolicy, SCHED_RT_PRIO_MAX, SCHED_RT_PRIO_MIN,
};
pub use processor::{
    current_kstack_top, current_process, current_processor, current_task, current_trap_cx,
    current_trap_cx_user_va, current_user_token, run_tasks, schedule, take_current_task,
};
pub use runqueue::{
    activate_task, activate_task_on, add_stopping_task, add_task, dequeue_task, enqueue_task_on,
    has_runnable_task_at_or_above, highest_runnable_prio, insert_into_pid2process, pid2process,
    pick_next_task, PID2PCB, remove_from_pid2process, remove_task, resched_hart, wakeup_task,
};
pub use switch::__switch;
