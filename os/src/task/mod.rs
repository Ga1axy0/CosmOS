//! Implementation of process [`ProcessControlBlock`] and task(thread) [`TaskControlBlock`] management mechanism
//!
//! Here is the entry for task scheduling required by other modules
//! (such as syscall or clock interrupt).
//! By suspending or exiting the current task, you can
//! modify the task state, manage the task queue through TASK_MANAGER (in task/manager.rs) ,
//! and switch the control flow through PROCESSOR (in task/processor.rs) .
//!
//! Be careful when you see [`__switch`]. Control flow around this function
//! might not be what you expect.

mod id;
mod process;
#[allow(clippy::module_inception)]
mod task;
mod wait_queue;

use self::id::TaskUserRes;
use crate::fs::{open_file_at, OpenFlags};
use crate::ipc;
use crate::mm::{reclaim_kernel_heap_if_needed, unregister_file_mappings_for_process};
use crate::mm::{DeferredUserReclaim, MapPermission, VirtAddr};
use crate::poll::task_has_inflight_keyed_poll_wait;
use crate::sched::{
    add_stopping_task, list_pids, pid2process, remove_from_pid2process, remove_task, schedule,
    resched_hart, take_current_task, TaskContext,
};
pub use crate::sched::{
    block_current_and_run_next, current_process, current_task, current_trap_cx,
    current_trap_cx_user_va, current_user_token, schedule_if_needed, suspend_current_and_run_next,
    suspend_current_and_run_next_with_slice_reset, wakeup_task, yield_current_and_run_next,
};
use crate::signal::cleanup_signal_wait_for_task;
use crate::sync::{cleanup_futex_wait_for_task, futex_wake_addr_in_process};
use crate::syscall::write_process_accounting_on_exit;
use crate::syscall::{read_pod_from_process_user, write_pod_to_process_user};
use crate::timer::get_time;
use crate::timer::get_time_ns;
use crate::timer::remove_timer;
use alloc::{collections::BTreeMap, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

/// Terminate every sibling task before the current process installs a new
/// image with `execve`.
///
/// `execve` keeps the calling thread and the process identity, but all other
/// threads must disappear before the old address space is recycled.  This is
/// deliberately separate from `exit_group_current_and_run_next`: the current
/// task must continue running and the PCB must remain usable after the image
/// replacement.
pub(crate) fn terminate_other_threads_for_exec(
    process: &Arc<ProcessControlBlock>,
    leader: &Arc<TaskControlBlock>,
) {
    let siblings = {
        let process_inner = process.inner_exclusive_access();
        process_inner
            .tasks
            .iter()
            .filter_map(|slot| slot.as_ref())
            .filter(|task| !Arc::ptr_eq(task, leader))
            .cloned()
            .collect::<Vec<_>>()
    };
    if siblings.is_empty() {
        return;
    }

    debug!(
        "[exec] terminate sibling threads: pid={} count={}",
        process.getpid(),
        siblings.len()
    );

    let mut active_siblings = Vec::<(Arc<TaskControlBlock>, usize)>::new();
    let mut running_siblings = Vec::<(Arc<TaskControlBlock>, usize)>::new();
    let mut recycle_res = Vec::<TaskUserRes>::new();

    for sibling in siblings {
        let state = {
            let mut task_inner = sibling.inner_exclusive_access();
            let already_exiting = task_inner.exit_code.is_some();
            let tid = task_inner.res.as_ref().map(|res| res.tid);
            let thread_id = task_inner.res.as_ref().map(|res| res.thread_id());
            let clear_child_tid = task_inner.clear_child_tid;
            let wait_handle = task_inner.current_wq_handle.take();
            let was_on_cpu = sibling.on_cpu.load(Ordering::Acquire);
            let last_cpu = task_inner.sched.last_cpu;
            if !already_exiting {
                task_inner.exit_code = Some(0);
            }
            task_inner.task_status = TaskStatus::Zombie;
            task_inner.wait_reason = None;
            sibling.set_resched_reason_locked(
                &mut task_inner,
                Some(crate::sched::ReschedReason::HigherRtPriority),
            );
            task_inner.clear_child_tid = 0;
            (
                tid,
                thread_id,
                clear_child_tid,
                wait_handle,
                was_on_cpu,
                last_cpu,
                already_exiting,
            )
        };
        let (tid, thread_id, clear_child_tid, wait_handle, was_on_cpu, last_cpu, already_exiting) =
            state;

        // A sibling may already be part-way through its own exit path.  It
        // still belongs to the old image and must be waited/reclaimed here,
        // but its one-shot waiter/timer/futex cleanup must not be repeated.
        if !already_exiting {
            if let Some(wait_handle) = wait_handle {
                wait_handle.remove_waiter(&sibling);
            }
            cleanup_signal_wait_for_task(&sibling);
            cleanup_futex_wait_for_task(&sibling);
            if should_remove_non_futex_timers_on_exit(&sibling) {
                remove_timer(Arc::clone(&sibling));
            }
            if let Some(thread_id) = thread_id {
                remove_from_tid2task(thread_id);
            }
            if let Some(tid) = tid {
                let mut process_inner = process.inner_exclusive_access();
                process_inner.mutex_detector.clear_thread(tid);
                process_inner.semaphore_detector.clear_thread(tid);
            }
        }

        if !already_exiting && clear_child_tid != 0 {
            if let Err(err) = write_pod_to_process_user(
                process,
                clear_child_tid as *mut i32,
                &0i32,
            ) {
                warn!(
                    "[exec] failed to clear sibling child_tid: pid={} tid={:?} addr={:#x} err={:?}",
                    process.getpid(),
                    tid,
                    clear_child_tid,
                    err
                );
            }
            if let Err(err) = futex_wake_addr_in_process(process, clear_child_tid, 1, false) {
                warn!(
                    "[exec] failed to wake sibling child_tid futex: pid={} tid={:?} addr={:#x} err={:?}",
                    process.getpid(),
                    tid,
                    clear_child_tid,
                    err
                );
            }
        }

        remove_task(Arc::clone(&sibling));
        if let Some(tid) = tid {
            active_siblings.push((Arc::clone(&sibling), tid));
        }
        if was_on_cpu {
            resched_hart(last_cpu);
            running_siblings.push((sibling, last_cpu));
        } else {
            quiesce_stopped_task(&sibling);
            if let Some(res) = sibling.inner_exclusive_access().res.take() {
                recycle_res.push(res);
            }
        }
    }

    // A sibling may currently be executing on another hart.  Its next
    // scheduling transition observes TaskStatus::Zombie and will not return
    // it to a runqueue.  Wait until its kernel stack and old user context are
    // no longer active before replacing the address space.
    while running_siblings
        .iter()
        .any(|(task, _)| task.on_cpu.load(Ordering::Acquire))
    {
        core::hint::spin_loop();
    }
    for (sibling, _) in running_siblings {
        quiesce_stopped_task(&sibling);
        if let Some(res) = sibling.inner_exclusive_access().res.take() {
            recycle_res.push(res);
        }
    }

    // Remove the dead tasks from the process task table.  Their resources are
    // dropped only after releasing process_inner because TaskUserRes::drop
    // needs to acquire that same lock while removing the old VMAs.
    {
        let mut process_inner = process.inner_exclusive_access();
        for (sibling, tid) in active_siblings {
            let slot_matches = process_inner
                .tasks
                .get(tid)
                .and_then(|slot| slot.as_ref())
                .is_some_and(|registered| Arc::ptr_eq(registered, &sibling));
            if slot_matches {
                process_inner.tasks[tid] = None;
            }
        }
    }
    drop(recycle_res);
}
#[cfg(feature = "cosmos-meminfo")]
pub(crate) use id::cached_kstack_count;
pub(crate) use id::reclaim_cached_kstacks;
pub(crate) use id::recycle_deferred_kstack_ids;
pub use id::{
    kstack_alloc, pid_alloc, KernelStack, PidHandle, TaskUserResAlloc, IDLE_PID, PID_MAX,
};
use lazy_static::*;

fn should_remove_non_futex_timers_on_exit(task: &Arc<TaskControlBlock>) -> bool {
    task.inner_exclusive_access().may_have_non_futex_timer
}

/// Finalize wait state after a stopped task has published on_cpu=false.
///
/// This second, idempotent sweep closes the window where a syscall installs a
/// waiter after a remote exec/exit owner took its first snapshot. No new wait
/// can be published after the context-switch handoff.
fn quiesce_stopped_task(task: &Arc<TaskControlBlock>) {
    let (wait_handle, remove_non_futex_timers) = {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.task_status = TaskStatus::Zombie;
        task_inner.wait_reason = None;
        task_inner.sched.on_rq = false;
        task.set_resched_reason_locked(&mut task_inner, None);
        (
            task_inner.current_wq_handle.take(),
            task_inner.may_have_non_futex_timer,
        )
    };
    // Zombie is now irreversible, so a concurrent waker can no longer add a
    // fresh runqueue node after this removal pass.
    remove_task(Arc::clone(task));
    if let Some(wait_handle) = wait_handle {
        wait_handle.remove_waiter(task);
    }
    crate::poll::cleanup_poll_wait_for_task(task);
    cleanup_signal_wait_for_task(task);
    cleanup_futex_wait_for_task(task);
    if remove_non_futex_timers {
        remove_timer(Arc::clone(task));
    }
}

static DEBUG_DUMP_PGRP: AtomicUsize = AtomicUsize::new(0);
static DEBUG_DUMP_REMAINING: AtomicUsize = AtomicUsize::new(0);
static DEBUG_DUMP_DEADLINE_NS: AtomicUsize = AtomicUsize::new(0);
const DEBUG_DUMP_INTERVAL_NS: usize = 1_000_000_000;

#[cfg(feature = "cosmos-meminfo")]
static PROCESS_CREATE_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static PROCESS_EXEC_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static PROCESS_EXIT_CALLS: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "cosmos-meminfo")]
#[derive(Clone, Copy, Debug, Default)]
/// Cumulative process lifecycle counters exported through `/proc/cosmos_meminfo`.
pub struct ProcessLifecycleStats {
    /// Number of successfully published processes.
    pub create_calls: usize,
    /// Number of successful `execve` transitions.
    pub exec_calls: usize,
    /// Number of processes transitioned to zombie state.
    pub exit_calls: usize,
}

#[cfg(feature = "cosmos-meminfo")]
pub(crate) fn account_process_create() {
    PROCESS_CREATE_CALLS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "cosmos-meminfo")]
pub(crate) fn account_process_exec() {
    PROCESS_EXEC_CALLS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "cosmos-meminfo")]
pub(crate) fn account_process_exit() {
    PROCESS_EXIT_CALLS.fetch_add(1, Ordering::Relaxed);
}

/// Return cumulative process lifecycle counters.
#[cfg(feature = "cosmos-meminfo")]
pub fn process_lifecycle_stats() -> ProcessLifecycleStats {
    ProcessLifecycleStats {
        create_calls: PROCESS_CREATE_CALLS.load(Ordering::Acquire),
        exec_calls: PROCESS_EXEC_CALLS.load(Ordering::Acquire),
        exit_calls: PROCESS_EXIT_CALLS.load(Ordering::Acquire),
    }
}
pub use crate::sched::{
    clamp_nice, nice_to_weight, ReschedReason, SchedAttr, SchedPolicy, DEFAULT_TIME_SLICE_TICKS,
    MAX_NICE, MIN_NICE, NICE_0_LOAD, SCHED_RT_PRIO_MAX, SCHED_RT_PRIO_MIN,
};
pub use crate::signal::{
    check_signals_of_current, handle_signals, SaFlags, SigInfo, SignalAction, SignalActions,
    SignalBit, SignalNum, MAX_SIG, SIG_DFL, SIG_IGN,
};
pub(crate) use process::ProcessControlBlock;
pub use process::{
    CloneResourceFlags, ExitReason, FdEntry, FdFlags, ProcessKeyrings, ShmAttachment,
};
pub(crate) use task::TaskControlBlockInner;
pub use task::{
    all_cpu_affinity_mask, LastSchedOp, TaskControlBlock, TaskSchedState, TaskStatus, WaitReason,
};
pub use wait_queue::{WaitQueue, WaitQueueHandle, WaitQueueKeyed};

use alloc::string::String;

fn child_exit_autoreap(parent: &Arc<ProcessControlBlock>, exit_signal: u32) -> bool {
    if exit_signal != SignalNum::SIGCHLD as u32 {
        return false;
    }
    let parent_inner = parent.inner_exclusive_access();
    let action = parent_inner.signal_actions.table[SignalNum::SIGCHLD as usize];
    action.handler == SIG_IGN || action.sa_flags & SaFlags::SA_NOCLDWAIT.bits() != 0
}

fn notify_parent_child_exit(parent: &Arc<ProcessControlBlock>, exit_signal: u32) -> bool {
    let autoreap = child_exit_autoreap(parent, exit_signal);
    if exit_signal != 0 {
        if let Some(signal) = SignalBit::from_signum(exit_signal) {
            add_signal_to_process(parent, signal);
        }
    }
    parent.wait_exit_queue.wake_one();
    autoreap
}

fn reap_zombie_child_from_parent(
    parent: &Arc<ProcessControlBlock>,
    child: &Arc<ProcessControlBlock>,
) -> bool {
    let removed_child = {
        let mut parent_inner = parent.inner_exclusive_access();
        let Some(idx) = parent_inner
            .children
            .iter()
            .position(|candidate| Arc::ptr_eq(candidate, child))
        else {
            return false;
        };
        parent_inner.children.remove(idx)
    };

    // The child must be inspected after releasing the parent PCB lock. The
    // exit path can hold a child PCB lock while notifying/reparenting it.
    let child_data = {
        let accounting_finalized = removed_child.cpu_accounting_finalized();
        // The Acquire finalized observation must precede the relaxed total
        // loads so the exit owner's Release publishes every final increment.
        let (user_time, kernel_time) = removed_child.committed_cpu_times();
        let child_inner = removed_child.inner_exclusive_access();
        if child_inner.is_zombie && accounting_finalized {
            Some((
                user_time,
                child_inner.child_user_time,
                kernel_time,
                child_inner.child_kernel_time,
            ))
        } else {
            None
        }
    };

    let Some((user_time, child_user_time, kernel_time, child_kernel_time)) = child_data else {
        // A concurrent state transition made the snapshot stale. Restore the
        // relationship without holding the child's lock.
        let mut parent_inner = parent.inner_exclusive_access();
        if !parent_inner
            .children
            .iter()
            .any(|candidate| Arc::ptr_eq(candidate, &removed_child))
        {
            parent_inner.children.push(Arc::clone(&removed_child));
        }
        return false;
    };

    // Update parent accounting only after the child lock has been released.
    {
        let mut parent_inner = parent.inner_exclusive_access();
        parent_inner.child_user_time = parent_inner
            .child_user_time
            .saturating_add(user_time)
            .saturating_add(child_user_time);
        parent_inner.child_kernel_time = parent_inner
            .child_kernel_time
            .saturating_add(kernel_time)
            .saturating_add(child_kernel_time);
    }

    let found_pid = removed_child.getpid();
    unregister_file_mappings_for_process(&removed_child);
    remove_from_pid2process(found_pid);
    drop(removed_child);
    reclaim_cached_kstacks(0);
    reclaim_kernel_heap_if_needed();
    true
}

/// Exit the current 'Running' task and run the next task in task list.
pub fn exit_current_and_run_next(reason: ExitReason) {
    exit_current_and_run_next_inner(reason, false);
}

/// Terminate the whole thread group from the current task.
pub fn exit_group_current_and_run_next(reason: ExitReason) {
    exit_current_and_run_next_inner(reason, true);
}

fn reap_clear_child_tid_thread(
    process: &Arc<ProcessControlBlock>,
    task: &Arc<TaskControlBlock>,
    tid: usize,
) {
    let detached_task = {
        let mut process_inner = process.inner_exclusive_access();
        process_inner.mutex_detector.clear_thread(tid);
        process_inner.semaphore_detector.clear_thread(tid);

        let slot_matches = process_inner
            .tasks
            .get(tid)
            .and_then(|slot| slot.as_ref())
            .is_some_and(|registered| Arc::ptr_eq(registered, task));
        if slot_matches {
            process_inner.tasks[tid].take()
        } else {
            warn!(
                "exit_current_and_run_next: pid={} tid={} clear_child_tid task was already detached",
                process.getpid(),
                tid
            );
            None
        }
    };

    // The PCB no longer owns this zombie. Its kernel stack remains protected by
    // the current hart's stop_task reference until the context switch completes.
    let user_res = task.inner_exclusive_access().res.take();
    if user_res.is_none() {
        warn!(
            "exit_current_and_run_next: pid={} tid={} clear_child_tid resources were already reclaimed",
            process.getpid(),
            tid
        );
    }
    drop(detached_task);
    drop(user_res);
}

fn exit_current_and_run_next_inner(reason: ExitReason, force_process_exit: bool) {
    let exit_reason = reason;
    let task_exit_code = match exit_reason {
        ExitReason::Exit(code) => code,
        ExitReason::Signal(signum) => -(signum as i32),
    };
    // Initproc is the lifetime owner of the guest. Keep it as the current
    // task while flushing storage: `shutdown_with_code()` calls
    // `sync_storage_all()`, and filesystem sleep locks may block through a
    // WaitQueue. Taking the task out of Processor first leaves
    // `current_task()` empty and makes that otherwise valid shutdown path
    // panic in WaitQueue::prepare_to_wait().
    let current_pid = current_task()
        .as_ref()
        .and_then(|task| task.process.upgrade())
        .map(|process| process.getpid());
    if current_pid == Some(IDLE_PID) {
        println!(
            "[kernel] Initproc process exit with exit_code {} ...",
            task_exit_code
        );
        crate::sbi::shutdown_with_code(task_exit_code);
    }
    trace!(
        "kernel: pid[{}] exit_current_and_run_next",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    // take from Processor
    let task = take_current_task().unwrap();
    let process = task.process.upgrade().unwrap();
    let pid = process.getpid();
    process.pause_cpu_accounting(task.as_ref(), get_time());
    let mut task_inner = task.inner_exclusive_access();
    let (tid, thread_id) = match task_inner.res.as_ref() {
        Some(res) => (Some(res.tid), Some(res.thread_id())),
        None => {
            warn!(
                "exit_current_and_run_next: pid={} entered exit path after task user resources were reclaimed",
                process.getpid()
            );
            (None, None)
        }
    };
    let clear_child_tid = task_inner.clear_child_tid;
    // record exit code
    task_inner.exit_code = Some(task_exit_code);
    task_inner.task_status = TaskStatus::Zombie;
    task_inner.sched.on_rq = false;
    task.set_resched_reason_locked(&mut task_inner, None);
    task_inner.clear_child_tid = 0;
    // The current kernel stack must stay alive until after the context switch.
    // Legacy threads remain attached for sys_waittid; clear_child_tid threads
    // are detached below while stop_task keeps their TCB alive.
    drop(task_inner);
    if clear_child_tid != 0 {
        debug!(
            "exit_current_and_run_next: pid={} tid={} thread_id={} clear_child_tid={:#x}",
            process.getpid(),
            tid.unwrap_or(usize::MAX),
            thread_id.unwrap_or(usize::MAX),
            clear_child_tid
        );
        match write_pod_to_process_user(&process, clear_child_tid as *mut i32, &0i32) {
            Ok(()) => {
                let read_back = read_pod_from_process_user(&process, clear_child_tid as *const i32);
                debug!(
                    "exit_current_and_run_next: cleared child_tid at {:#x}, read_back={:?}",
                    clear_child_tid, read_back
                );
            }
            Err(err) => {
                warn!(
                    "exit_current_and_run_next: failed to clear child_tid at {:#x}: {:?}",
                    clear_child_tid, err
                );
            }
        }
        // CLONE_CHILD_CLEARTID specifies a plain FUTEX_WAKE. In particular,
        // musl points child_tid at its shared __thread_list_lock.
        match futex_wake_addr_in_process(&process, clear_child_tid, 1, false) {
            Ok(woke) => {
                debug!(
                    "exit_current_and_run_next: futex_wake_addr({:#x}, 1) -> {}",
                    clear_child_tid, woke
                );
            }
            Err(err) => {
                warn!(
                    "exit_current_and_run_next: failed to wake clear_child_tid futex at {:#x}: {:?}",
                    clear_child_tid,
                    err
                );
            }
        }
    }
    cleanup_signal_wait_for_task(&task);
    cleanup_futex_wait_for_task(&task);
    let remove_non_futex_timers = should_remove_non_futex_timers_on_exit(&task);
    if remove_non_futex_timers {
        remove_timer(Arc::clone(&task));
    }
    if let Some(thread_id) = thread_id {
        remove_from_tid2task(thread_id);
    }

    let exiting_task = task;
    // If this is the main thread or exit_group was requested, the process
    // should terminate at once.
    if tid == Some(0) || force_process_exit {
        // A vfork parent must also be released when the child exits before
        // reaching execve, for example when execve itself fails.
        process.release_vfork_parent();
        let mut process_inner = process.inner_exclusive_access();
        if process_inner.is_zombie {
            drop(process_inner);
            let mut process_inner = process.inner_exclusive_access();
            if let Some(tid) = tid {
                process_inner.mutex_detector.clear_thread(tid);
                process_inner.semaphore_detector.clear_thread(tid);
            }
            drop(process_inner);
            // Publish on_cpu=false only after this hart has switched off the
            // task's kernel stack; a concurrent exit_group/exec owner waits on
            // that handoff before reclaiming this task's user resources.
            add_stopping_task(exiting_task);
            drop(process);
            let mut _unused = TaskContext::zero_init();
            schedule(&mut _unused as *mut _);
            return;
        }
        // A successful exec claim linearizes before this exit_group request.
        // Let that sole owner reap this sibling; becoming a second process-
        // wide teardown owner here would make exec and exit wait on each
        // other's on_cpu handoff and could recycle the same address space.
        if process.exec_in_progress() {
            drop(process_inner);
            let mut process_inner = process.inner_exclusive_access();
            if let Some(tid) = tid {
                process_inner.mutex_detector.clear_thread(tid);
                process_inner.semaphore_detector.clear_thread(tid);
            }
            drop(process_inner);
            add_stopping_task(exiting_task);
            drop(process);
            let mut _unused = TaskContext::zero_init();
            schedule(&mut _unused as *mut _);
            return;
        }
        // mark this process as a zombie process
        #[cfg(feature = "cosmos-meminfo")]
        account_process_exit();
        process_inner.is_zombie = true;
        #[cfg(feature = "return_work_cache")]
        {
            // Publish the process-wide hint before the per-task return bit.
            // Observing a task bit with Acquire must never lead the slow path
            // to observe an older false process hint.
            process.mark_zombie_work_pending();
            // Publish exit work to every thread while process-inner keeps the
            // task table stable. Their common trap-exit path can then decide
            // whether to enter the slow path with one TCB bitmap load.
            for task in process_inner.tasks.iter().flatten() {
                task.mark_zombie_work_pending();
            }
        }
        // record process exit reason for wait4/waitpid
        process_inner.exit_reason = exit_reason;
        let clone_shared_resources = process_inner.clone_shared_resources;
        let clone_parent = process_inner.parent.clone();
        let clone_shared_fd_table = clone_shared_resources
            .contains(CloneResourceFlags::FILES)
            .then(|| process_inner.fd_table.clone());
        let clone_shared_cwd = clone_shared_resources
            .contains(CloneResourceFlags::FS)
            .then(|| process_inner.cwd.clone());
        let clone_shared_signal_actions = clone_shared_resources
            .contains(CloneResourceFlags::SIGHAND)
            .then(|| process_inner.signal_actions.clone());
        let children_to_reparent = core::mem::take(&mut process_inner.children);
        // Do not hold the exiting process's PCB lock while taking any child
        // PCB lock or INITPROC's PCB lock. Those paths can run concurrently
        // with wait4/child-exit notification and otherwise create a cycle.
        drop(process_inner);

        for child in &children_to_reparent {
                let mut child_inner = child.inner_exclusive_access();
                child_inner.parent = Some(Arc::downgrade(&INITPROC));
                #[cfg(feature = "process_identity_cache")]
                child.set_ppid_cached(INITPROC.getpid());
        }
        {
            let mut initproc_inner = INITPROC.inner_exclusive_access();
            for child in &children_to_reparent {
                initproc_inner.children.push(Arc::clone(child));
            }
        }
        // Recheck only after every child is visible in INITPROC.children. A
        // child can finalize between parent reassignment and insertion; its
        // own wake would then be early, so this post-insert check closes the
        // lost-wakeup window. Duplicate notifications are harmless because
        // reap_zombie_child_from_parent revalidates membership.
        for child in children_to_reparent {
            let finalized = child.cpu_accounting_finalized();
            let zombie = child.inner_exclusive_access().is_zombie;
            if !zombie || !finalized {
                continue;
            }
            let autoreap = notify_parent_child_exit(&INITPROC, child.clone_exit_signal);
            if autoreap {
                reap_zombie_child_from_parent(&INITPROC, &child);
            }
        }
        if !clone_shared_resources.is_empty() {
            if let Some(parent) = clone_parent.and_then(|parent| parent.upgrade()) {
                let mut parent_inner = parent.inner_exclusive_access();
                if let Some(fd_table) = clone_shared_fd_table {
                    parent_inner.fd_table = fd_table;
                }
                if let Some(cwd) = clone_shared_cwd {
                    parent_inner.cwd = cwd;
                }
                if let Some(signal_actions) = clone_shared_signal_actions {
                    parent_inner.signal_actions = signal_actions;
                }
            }
        }
        // deallocate user res (including tid/trap_cx/ustack) of all threads
        // it has to be done before we dealloc the whole memory_set
        // otherwise they will be deallocated twice
        let mut recycle_res = Vec::<TaskUserRes>::new();
        let mut running_tasks = Vec::new();
        let mut running_harts = Vec::new();
        // Snapshot task references under the PCB lock, then inspect and
        // mutate each task after releasing it.  Holding process-inner while
        // taking task-inner creates the opposite lock order to task paths
        // that need to update their process, which can deadlock SMP teardown.
        let tasks = {
            let process_inner = process.inner_exclusive_access();
            process_inner
                .tasks
                .iter()
                .filter_map(|slot| slot.as_ref().cloned())
                .collect::<Vec<_>>()
        };
        for task in tasks {
            let (thread_id, was_on_cpu, last_cpu, wait_handle) = {
                let mut task_inner = task.inner_exclusive_access();
                task_inner.exit_code.get_or_insert(task_exit_code);
                task_inner.task_status = TaskStatus::Zombie;
                task_inner.wait_reason = None;
                task_inner.sched.on_rq = false;
                task.set_resched_reason_locked(
                    &mut task_inner,
                    Some(ReschedReason::HigherRtPriority),
                );
                (
                    task_inner.res.as_ref().map(|res| res.thread_id()),
                    task.on_cpu.load(Ordering::Relaxed),
                    task_inner.sched.last_cpu,
                    task_inner.current_wq_handle.take(),
                )
            };
            if let Some(wait_handle) = wait_handle {
                wait_handle.remove_waiter(&task);
            }
            if let Some(thread_id) = thread_id {
                remove_from_tid2task(thread_id);
            }
            if was_on_cpu && !Arc::ptr_eq(&task, &exiting_task) {
                running_harts.push(last_cpu);
                running_tasks.push(Arc::clone(&task));
                continue;
            }
            // if other tasks are Runnable in TaskManager or waiting for a timer to be
            // expired, we should remove them.
            //
            // Mention that we do not need to consider Mutex/Semaphore since they
            // are limited in a single process. Therefore, the blocked tasks are
            // removed when the PCB is deallocated.
            trace!("kernel: exit_current_and_run_next .. remove_inactive_task");
            quiesce_stopped_task(&task);
            let mut task_inner = task.inner_exclusive_access();
            if let Some(res) = task_inner.res.take() {
                recycle_res.push(res);
            }
        }
        for hart in running_harts {
            crate::sched::resched_hart(hart);
        }
        while running_tasks
            .iter()
            .any(|task| task.on_cpu.load(Ordering::Acquire))
        {
            core::hint::spin_loop();
        }
        // Every running sibling has now crossed its scheduler/exit accounting
        // transition, so the atomic process totals form a complete exit
        // snapshot for BSD accounting and later wait/reap aggregation.
        write_process_accounting_on_exit(&process, exit_reason);
        // Do not reacquire process-inner while extracting resources.  The
        // TaskUserRes destructor needs that same PCB lock when the vector is
        // dropped below.
        for task in running_tasks {
            quiesce_stopped_task(&task);
            let mut task_inner = task.inner_exclusive_access();
            if let Some(res) = task_inner.res.take() {
                recycle_res.push(res);
            }
            task_inner.sched.on_rq = false;
        }
        recycle_res.clear();

        let exit_signal = process.clone_exit_signal;
        let (closed_fds, reclaim, shm_attachments, keyrings_to_release) = {
            let mut process_inner = process.inner_exclusive_access();
            // deallocate other data in user space i.e. program code/data section
            let token = process_inner.memory_set.token();
            let release_batch = process_inner.memory_set.recycle_data_pages_deferred();
            let mask = process_inner.memory_set.record_local_tlb_change();
            // warn_heap_state_lockfree("exit_after_vmas_clear", pid);
            let reclaim = DeferredUserReclaim::new(token, mask, release_batch);
            // 关键点：先把 fd 表项整体移出，避免在持有进程自旋锁时触发文件同步或块设备等待。
            let closed_fds = process_inner.take_all_fds();
            process_inner.fd_table.clear();
            // warn_heap_state_lockfree("exit_after_fd_take", pid);
            // remove all tasks
            process_inner.tasks.clear();
            // warn_heap_state_lockfree("exit_after_tasks_clear", pid);

            let shm_attachments = core::mem::take(&mut process_inner.shm_attachments);
            let keyrings_to_release = core::mem::take(&mut process_inner.keyrings);
            (
                closed_fds,
                reclaim,
                shm_attachments,
                keyrings_to_release,
            )
        };
        reclaim.flush_then_release();
        // warn_heap_state("exit_after_user_reclaim", pid);
        for entry in &closed_fds {
            entry.desc.release_posix_locks_for_owner(pid);
        }
        drop(closed_fds);
        // warn_heap_state("exit_after_fd_drop", pid);
        crate::keys::release_process_thread_keyring(keyrings_to_release);
        for attachment in shm_attachments {
            ipc::detach_segment(attachment.shmid);
        }

        // `is_zombie` was set early to stop sibling tasks. Publish the second
        // phase only after their CPU slices and process teardown are complete;
        // wait4/WNOHANG must not reap the PCB before this point.
        process.finalize_cpu_accounting();
        // Re-read the parent only after finalization. If reparenting happened
        // earlier, the old parent saw an unfinished zombie and deliberately
        // skipped notification; using a pre-teardown snapshot here would then
        // notify the wrong process and leave init's wait queue asleep.
        let parent_weak = process.inner_exclusive_access().parent.clone();
        if let Some(parent) = parent_weak.and_then(|pw| pw.upgrade()) {
            let autoreap = notify_parent_child_exit(&parent, exit_signal);
            if autoreap {
                reap_zombie_child_from_parent(&parent, &process);
            }
        }
        // warn_heap_state("exit_end", pid);
    } else {
        if let Some(tid) = tid {
            if clear_child_tid != 0 {
                reap_clear_child_tid_thread(&process, &exiting_task, tid);
            } else {
                let mut process_inner = process.inner_exclusive_access();
                process_inner.mutex_detector.clear_thread(tid);
                process_inner.semaphore_detector.clear_thread(tid);
            }
        }
    }
    // Move the exiting task reference off the stack. The idle loop publishes
    // on_cpu=false with Release and drops it only after __switch completes.
    add_stopping_task(exiting_task);
    drop(process);
    // we do not have to save task context
    let mut _unused = TaskContext::zero_init();
    schedule(&mut _unused as *mut _);
}

lazy_static! {
    /// Creation of initial process
    ///
    /// the name "initproc" may be changed to any other app name like "usertests",
    /// but we have user_shell, so we don't need to change it.
    pub static ref INITPROC: Arc<ProcessControlBlock> = {
        open_file_at("/", "/sbin/init", OpenFlags::RDONLY).expect("Init binary not found at /sbin/init! Rebuild image to include rootfs init.");
        ProcessControlBlock::new(String::from("/sbin/init"))
    };
    static ref TID2TASK: crate::sync::SpinNoIrqLock<BTreeMap<usize, alloc::sync::Weak<TaskControlBlock>>> =
        crate::sync::SpinNoIrqLock::new(BTreeMap::new());
}

///Add init process to the manager
pub fn add_initproc() {
    let _initproc = INITPROC.clone();
}

/// Spawn a scheduler-visible kernel thread owned by the init process context.
pub fn spawn_kernel_thread(entry: fn() -> !, sched_attr: SchedAttr) -> Arc<TaskControlBlock> {
    let task = Arc::new(
        TaskControlBlock::new_kernel_thread(INITPROC.clone(), entry, sched_attr)
            .expect("failed to allocate kernel thread"),
    );
    crate::sched::add_task(Arc::clone(&task));
    task
}

/// Look up a live task by its Linux-visible thread id.
pub fn thread_id2task(thread_id: usize) -> Option<Arc<TaskControlBlock>> {
    let mut map = TID2TASK.lock();
    let task = map.get(&thread_id).and_then(|task| task.upgrade());
    if task.is_none() {
        map.remove(&thread_id);
    }
    task
}

/// Publish one task in the global Linux-visible thread-id index.
pub fn insert_into_tid2task(thread_id: usize, task: &Arc<TaskControlBlock>) {
    TID2TASK.lock().insert(thread_id, Arc::downgrade(task));
}

/// Remove one task from the global Linux-visible thread-id index.
pub fn remove_from_tid2task(thread_id: usize) {
    TID2TASK.lock().remove(&thread_id);
}

fn wake_signal_waiters(tasks: Vec<Arc<TaskControlBlock>>) {
    for task in tasks {
        if task_has_inflight_keyed_poll_wait(&task) {
            continue;
        }
        let handle = {
            let task_inner = task.inner_exclusive_access();
            task_inner.current_wq_handle.clone()
        };
        if let Some(handle) = handle {
            handle.wake_waiter(&task);
            continue;
        }
        let should_wake = {
            let task_inner = task.inner_exclusive_access();
            matches!(task_inner.task_status, TaskStatus::Interruptible)
        };
        if should_wake {
            wakeup_task(task);
        }
    }
}

/// Check if the current task has any fatal signal to handle
/// 因为只检查致命信号，所以可不复位pending_signals
pub fn check_fatal_signals_of_current() -> Option<(i32, &'static str)> {
    let task = current_task().unwrap();
    if !task.signal_work_pending() {
        return None;
    }
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let task_inner = task.inner_exclusive_access();
    let pending = (task_inner.pending_signals | process_inner.pending_signals)
        & !task_inner.signal_mask.without_unblockable();
    let mut remaining = pending;
    while !remaining.is_empty() {
        let signum = remaining.bits().trailing_zeros() as usize + 1;
        let flag = SignalBit::from_signum(signum as u32).unwrap();
        remaining &= !flag;
        let action = process_inner.signal_actions.table[signum];
        if action.handler == SIG_DFL {
            if let Some(error) = flag.check_error() {
                return Some(error);
            }
        }
    }
    task.set_signal_work_pending(crate::signal::signal_work_needed(
        task_inner.pending_signals,
        process_inner.pending_signals,
        task_inner.signal_mask,
        task_inner.signal_mask_backup.is_some(),
    ));
    None
}

/// Check if the current process is a zombie process (i.e. has exited but not yet been reaped by its parent).
pub fn current_process_is_zombie() -> bool {
    let process = current_process();
    #[cfg(feature = "return_work_cache")]
    {
        return process.zombie_work_pending();
    }
    #[cfg(not(feature = "return_work_cache"))]
    let process_inner = process.inner_exclusive_access();
    #[cfg(not(feature = "return_work_cache"))]
    process_inner.is_zombie
}

fn first_signum_in_set(signal: SignalBit) -> Option<usize> {
    let bits = signal.bits();
    (bits != 0).then(|| bits.trailing_zeros() as usize + 1)
}

/// Add signal to target process.
///
/// Wake interruptible waiters whenever the delivered signal is currently
/// unmasked for them, even if that signal bit was already pending. Repeated
/// terminal-generated SIGINT must still be able to kick tasks out of sleep.
pub fn add_signal_to_process(process: &Arc<ProcessControlBlock>, signal: SignalBit) {
    let signum = first_signum_in_set(signal)
        .map(|num| num as i32)
        .unwrap_or_default();
    add_signal_to_process_with_siginfo(process, signal, SigInfo::for_kernel(signum));
}

/// Add signal to target process with explicit siginfo metadata.
pub fn add_signal_to_process_with_siginfo(
    process: &Arc<ProcessControlBlock>,
    signal: SignalBit,
    siginfo: SigInfo,
) {
    let (pid, _newly_pending, tasks) = {
        let mut process_inner = process.inner_exclusive_access();
        let tasks = process_inner
            .tasks
            .iter()
            .filter_map(|slot| slot.as_ref().map(Arc::clone))
            .collect::<Vec<_>>();
        let newly_pending = signal & !process_inner.pending_signals;
        process_inner.pending_signals |= signal;
        if let Some(signum) = first_signum_in_set(signal) {
            process_inner.pending_siginfo[signum] = siginfo;
        }
        // Publish the slow-path hint before releasing process-inner, so a
        // concurrently returning task cannot miss both the signal and its
        // notification.  Masked tasks may take one conservative slow path and
        // clear the hint again.
        for task in &tasks {
            task.mark_signal_work_pending();
        }
        (process.getpid(), newly_pending, tasks)
    };

    crate::signal::notify_signal_wait_pid(pid, signal.bits());
    crate::fs::signalfd::notify_signal_fd(signal.bits());

    let deliverable_tasks = tasks
        .into_iter()
        .filter(|task| {
            let task_inner = task.inner_exclusive_access();
            !(signal & !task_inner.signal_mask.without_unblockable()).is_empty()
        })
        .collect::<Vec<_>>();

    if !deliverable_tasks.is_empty() {
        debug!(
            "add_signal_to_process: pid={} added signal {:#x} deliverable to {} task(s)",
            pid,
            signal.bits(),
            deliverable_tasks.len()
        );
        crate::poll::notify_poll_signal_pid(pid);
        wake_signal_waiters(deliverable_tasks);
    }
}

/// Add one pending signal directly to a specific thread.
pub fn add_signal_to_task(task: &Arc<TaskControlBlock>, signal: SignalBit) {
    let signum = first_signum_in_set(signal)
        .map(|num| num as i32)
        .unwrap_or_default();
    add_signal_to_task_with_siginfo(task, signal, SigInfo::for_kernel(signum));
}

/// Add one pending signal directly to a specific thread with explicit siginfo.
pub fn add_signal_to_task_with_siginfo(
    task: &Arc<TaskControlBlock>,
    signal: SignalBit,
    siginfo: SigInfo,
) {
    let process = task.process.upgrade().unwrap();
    let pid = process.getpid();
    let (thread_id, inner_tid, signal_mask_bits, newly_unmasked) = {
        let mut task_inner = task.inner_exclusive_access();
        let newly_pending = signal & !task_inner.pending_signals;
        task_inner.pending_signals |= signal;
        if let Some(signum) = first_signum_in_set(signal) {
            task_inner.pending_siginfo[signum] = siginfo;
        }
        // Set the hint while holding task-inner.  The locked consumer can then
        // safely clear it only after observing the newly published bit.
        task.mark_signal_work_pending();
        (
            task_inner.res.as_ref().unwrap().thread_id(),
            task_inner.res.as_ref().unwrap().tid,
            task_inner.signal_mask.bits(),
            newly_pending & !task_inner.signal_mask.without_unblockable(),
        )
    };

    crate::signal::notify_signal_wait_task(task, signal.bits());
    crate::fs::signalfd::notify_signal_fd(signal.bits());

    if !newly_unmasked.is_empty() {
        crate::poll::notify_poll_signal_pid(pid);
        wake_signal_waiters(vec![Arc::clone(task)]);
    }
}

/// Broadcast a signal to every process belonging to process group `pgrp`.
///
/// This mirrors Linux `kill_pgrp()` / the `kill(2)` "negative pid" path and is
/// the mechanism the tty line discipline uses to deliver terminal-generated
/// signals (SIGINT/SIGQUIT/SIGTSTP from Ctrl+C / Ctrl+\\ / Ctrl+Z) to the
/// foreground process group of the controlling terminal.
///
/// Returns the number of processes that were signalled, so callers can map an
/// empty group to `ESRCH` the way Linux does.
pub fn send_signal_to_pgrp(pgrp: u32, signal: SignalBit, siginfo: SigInfo) -> usize {
    if pgrp == 0 {
        return 0;
    }
    // Snapshot the matching processes first; `add_signal_to_process_*` takes the
    // per-process lock and wakes waiters, so we must not hold the global pid
    // table lock (acquired inside `list_pids`/`pid2process`) across delivery.
    let targets: Vec<Arc<ProcessControlBlock>> = list_pids()
        .into_iter()
        .filter_map(pid2process)
        .filter(|process| process.getpgid() == pgrp)
        .collect();
    let count = targets.len();
    for process in targets {
        add_signal_to_process_with_siginfo(&process, signal, siginfo);
    }
    count
}

/// Dump a compact process-group task snapshot for diagnosing stuck foreground jobs.
///
/// This is intentionally log-only and low-frequency: callers should invoke it
/// at meaningful control points such as terminal-generated SIGINT, not on every
/// wait/wake operation.
pub fn debug_dump_pgrp_tasks(pgrp: u32, reason: &str) {
    if pgrp == 0 {
        return;
    }
    let targets: Vec<Arc<ProcessControlBlock>> = list_pids()
        .into_iter()
        .filter_map(pid2process)
        .filter(|process| process.getpgid() == pgrp)
        .collect();
    warn!(
        "[task-dump] reason={} pgrp={} process_count={}",
        reason,
        pgrp,
        targets.len()
    );
    for process in targets {
        let pid = process.getpid();
        let exec_path = process.exec_path();
        // Snapshot process + task state under the locks, then DROP the locks
        // before formatting/printing. Holding process_inner/task_inner (SpinNoIrq,
        // IRQs disabled) across the warn! calls — which do string formatting AND
        // byte-by-byte UART output, both slow — blocked in-flight global TLB
        // shootdown IPIs and wedged the machine when several harts dumped at
        // once after Ctrl+C. The lock is now held only to copy fields.
        let (ppid, pgid, is_zombie, pending, tasks) = {
            let process_inner = process.inner_exclusive_access();
            let p_ppid = process_inner
                .parent
                .as_ref()
                .and_then(|parent| parent.upgrade())
                .map(|parent| parent.getpid())
                .unwrap_or(0);
            let p_pgid = process_inner.cred.pgid;
            let p_zombie = process_inner.is_zombie;
            let p_pending = process_inner.pending_signals.bits();
            let tasks: Vec<(usize, Arc<TaskControlBlock>)> = process_inner
                .tasks
                .iter()
                .enumerate()
                .filter_map(|(tid, task)| task.as_ref().map(|task| (tid, Arc::clone(task))))
                .collect();
            (p_ppid, p_pgid, p_zombie, p_pending, tasks)
        };
        // Never hold process-inner while taking task-inner.  This diagnostic
        // runs from the Ctrl-C/scheduler path, where another hart may be
        // unwinding a task and acquiring the locks in the reverse order.
        let task_snaps: Vec<(
            usize,
            TaskStatus,
            Option<WaitReason>,
            bool,
            bool,
            usize,
            bool,
            u64,
            u64,
            Option<ReschedReason>,
            LastSchedOp,
        )> = tasks
            .into_iter()
            .map(|(tid, task)| {
                let task_inner = task.inner_exclusive_access();
                (
                    tid,
                    task_inner.task_status,
                    task_inner.wait_reason,
                    task.on_cpu.load(Ordering::Relaxed),
                    task_inner.sched.on_rq,
                    task_inner.sched.last_cpu,
                    task_inner.current_wq_handle.is_some(),
                    task_inner.pending_signals.bits(),
                    task_inner.signal_mask.bits(),
                    task_inner.sched.resched_reason,
                    task_inner.last_sched_op,
                )
            })
            .collect();
        warn!(
            "[task-dump] pid={} ppid={} pgid={} zombie={} pending_signals={:#x} exec={}",
            pid, ppid, pgid, is_zombie, pending, exec_path
        );
        for (
            tid,
            status,
            wait,
            on_cpu,
            on_rq,
            last_cpu,
            has_wq,
            task_pending,
            mask,
            resched,
            last_sched_op,
        ) in task_snaps
        {
            warn!(
                "[task-dump]   pid={} tid={} status={:?} wait={:?} on_cpu={} on_rq={} last_cpu={} has_wq={} task_pending={:#x} mask={:#x} resched={:?} last_sched_op={:?}",
                pid,
                tid,
                status,
                wait,
                on_cpu,
                on_rq,
                last_cpu,
                has_wq,
                task_pending,
                mask,
                resched,
                last_sched_op,
            );
            if let Some(WaitReason::Futex(uaddr, expected)) = wait {
                let current = read_pod_from_process_user::<i32>(&process, uaddr as *const i32).ok();
                warn!(
                    "[task-dump]     futex pid={} tid={} uaddr={:#x} expected={} current={:?}",
                    pid, tid, uaddr, expected, current
                );
            }
        }
    }
}

/// Request a few task snapshots from scheduler context after a terminal signal.
pub fn arm_debug_pgrp_task_dump(pgrp: u32) {
    if pgrp == 0 {
        return;
    }
    DEBUG_DUMP_PGRP.store(pgrp as usize, Ordering::Release);
    DEBUG_DUMP_DEADLINE_NS.store(get_time_ns() as usize, Ordering::Release);
    DEBUG_DUMP_REMAINING.store(3, Ordering::Release);
}

/// Emit pending debug snapshots from a non-IRQ scheduler safe point.
pub fn maybe_dump_pending_debug_pgrp_tasks() {
    let remaining = DEBUG_DUMP_REMAINING.load(Ordering::Acquire);
    if remaining == 0 {
        return;
    }
    let now_ns = get_time_ns() as usize;
    let deadline = DEBUG_DUMP_DEADLINE_NS.load(Ordering::Acquire);
    if now_ns < deadline {
        return;
    }
    if DEBUG_DUMP_REMAINING
        .compare_exchange(
            remaining,
            remaining - 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }
    let pgrp = DEBUG_DUMP_PGRP.load(Ordering::Acquire) as u32;
    debug_dump_pgrp_tasks(pgrp, "tty-sigint-followup");
    if remaining > 1 {
        DEBUG_DUMP_DEADLINE_NS.store(
            now_ns.saturating_add(DEBUG_DUMP_INTERVAL_NS),
            Ordering::Release,
        );
    } else {
        DEBUG_DUMP_PGRP.store(0, Ordering::Release);
    }
}

/// Add signal to the current task
pub fn current_add_signal(signal: SignalBit) {
    let task = current_task().unwrap();
    add_signal_to_task(&task, signal);
}

/// 扫描所有进程的 interval timer，到期则投递对应信号。
///
/// 该函数运行在时钟中断（硬 IRQ）上下文中。对齐 Linux 的两点做法以避免把
/// 重活放进每个 hart 的每个 tick：
///
/// 1. **无 timer 时不做任何工作**：若系统范围内没有任何已武装的 interval
///    timer（绝大多数负载，例如 hackbench），直接返回——不取锁、不分配内存。
///    这消除了此前“每 tick 都在硬中断里持 `PID2PCB` 锁分配一个包含全部进程
///    的 `Vec`”的反模式，正是该反模式 + 非中断安全的堆锁导致了 SMP 死锁。
/// 2. **全局周期性工作只在单个 hart 上做**：类似 Linux 的 `tick_do_timer_cpu`，
///    只让 0 号 hart 执行这次全进程扫描，避免 8 个 hart 在每个 tick 上对
///    `PID2PCB` 的冗余争用与重复投递。
pub fn check_itimers_of_all_processes(now_raw: usize, now_realtime_ns: u64) {
    if process::armed_itimers_count() == 0 {
        return;
    }
    if crate::hal::hartid() != 0 {
        return;
    }
    let processes: Vec<Arc<ProcessControlBlock>> = {
        let map = crate::sched::PID2PCB.lock();
        map.values().cloned().collect()
    };

    for process in processes {
        let pending = process.consume_expired_itimers(now_raw, now_realtime_ns);
        if !pending.is_empty() {
            add_signal_to_process(&process, pending);
        }
    }
}

/// the inactive(blocked) tasks are removed when the PCB is deallocated.(called by exit_current_and_run_next)
pub fn remove_inactive_task(task: Arc<TaskControlBlock>) {
    remove_task(Arc::clone(&task));
    cleanup_signal_wait_for_task(&task);
    cleanup_futex_wait_for_task(&task);
    trace!("kernel: remove_inactive_task .. remove_timer");
    let remove_non_futex_timers = should_remove_non_futex_timers_on_exit(&task);
    if remove_non_futex_timers {
        remove_timer(Arc::clone(&task));
    }
}

/// Map an anonymous area in current process with given permission.
pub fn mmap_current_process(
    start: VirtAddr,
    end: VirtAddr,
    perm: MapPermission,
) -> Result<(), crate::syscall::errno::ERRNO> {
    current_process().mmap(start, end, perm, false)
}

/// Unmap an anonymous area in current process.
pub fn munmap_current_process(start: VirtAddr, end: VirtAddr) -> bool {
    current_process().munmap(start, end)
}

/// Sync a mapped range in current process.
pub fn msync_current_process(
    start: VirtAddr,
    end: VirtAddr,
) -> Result<(), crate::syscall::errno::ERRNO> {
    current_process().msync(start, end)
}

/// Change permissions on a range in current process.
pub fn mprotect_current_process(start: VirtAddr, end: VirtAddr, perm: MapPermission) -> bool {
    current_process().mprotect(start, end, perm)
}
