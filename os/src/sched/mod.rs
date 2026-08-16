//! Scheduler module.
//!
//! This module owns CPU-local scheduling state and context switching
//! primitives. Task and process object definitions remain under `task`.

mod api;
mod autogroup;
mod bais;
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
pub use autogroup::{autogroup_enabled, set_autogroup_enabled};
pub use bais::{
    account_ai_device_control as account_bais_ai_device_control,
    account_ai_memory_control as account_bais_ai_memory_control,
    account_ai_other_syscall as account_bais_ai_other_syscall,
    account_ai_page_fault as account_bais_ai_page_fault,
    account_block_deferred as account_bais_block_deferred, account_irq as account_bais_irq,
    account_net_deferred as account_bais_net_deferred, apply_control as apply_bais_control,
    enabled as bais_enabled,
    hint_current as bais_hint_current, note_block_irq as note_bais_block_irq,
    note_net_irq as note_bais_net_irq, render as render_bais,
};
pub use context::TaskContext;
pub use policy::{
    clamp_nice, nice_to_weight, ReschedReason, SchedAttr, SchedPolicy, CFS_MIN_GRANULARITY_NS,
    CFS_TARGET_LATENCY_NS, CFS_WAKEUP_GRANULARITY_NS, CFS_YIELD_PENALTY_NS,
    DEFAULT_TIME_SLICE_TICKS, MAX_NICE, MIN_NICE, NICE_0_LOAD, SCHED_RT_PRIO_MAX,
    SCHED_RT_PRIO_MIN,
};
pub(crate) use processor::{
    activate_current_address_space, current_kstack_top, defer_task_release_after_switch,
    restore_current_task, run_tasks, schedule, take_current_task,
};
pub use processor::{
    current_process, current_task, current_trap_cx, current_trap_cx_user_va, current_user_token,
};
#[cfg(feature = "sched_invariant_checks")]
pub(crate) use runqueue::check_sched_invariants;
pub use runqueue::wakeup_task;
pub(crate) use runqueue::{
    add_stopping_task, add_task, boost_process_cfs_tasks, cfs_should_preempt, clear_stopping_task,
    enqueue_task_on, has_runnable_task_at_or_above, insert_into_pid2process, list_pids,
    pick_next_task, pid2process, remove_from_pid2process, remove_task, resched_hart,
    warn_lost_runnable_tasks, PID2PCB,
};
pub use switch::__switch;
