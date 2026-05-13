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

#[path ="../sched/context.rs"]
mod context;
mod id;
#[path = "../sched/runqueue.rs"]
mod runqueue;
mod process;
#[path ="../sched/processor.rs"]
mod processor;
#[path ="../sched/switch.rs"]
mod switch;
mod wait_queue;
#[allow(clippy::module_inception)]
mod task;

use self::id::TaskUserRes;
use crate::fs::{open_file, OpenFlags};
use crate::poll::task_has_inflight_keyed_poll_wait;
use crate::task::runqueue::add_stopping_task;
use crate::timer::remove_timer;
use crate::timer::get_time;
use crate::mm::{DeferredUserReclaim, MapPermission, VirtAddr};
use alloc::{sync::Arc, vec::Vec};
use lazy_static::*;
use switch::__switch;

pub use context::TaskContext;
pub use id::{kstack_alloc, pid_alloc, KernelStack, PidHandle, IDLE_PID};
pub use runqueue::{
    add_task, dequeue_task, enqueue_task_on, has_runnable_task_at_or_above, highest_runnable_prio,
    pid2process, pick_next_task, remove_from_pid2process, remove_task, resched_hart, wakeup_task,
};
pub use crate::signal::{
    check_signals_of_current, handle_signals, MContext, MAX_SIG, SaFlags, SigInfo, SignalAction,
    SignalActions, SignalFlags, StackT, UContext, SIG_DFL, SIG_IGN,
};
pub use processor::{
    current_kstack_top, current_process, current_processor, current_task, current_trap_cx,
    current_trap_cx_user_va, current_user_token, run_tasks, schedule, take_current_task,
};
pub use wait_queue::{WaitQueue, WaitQueueHandle, WaitQueueKeyed};
pub use process::{ExitReason, FdEntry, FdFlags};
pub(crate) use process::ProcessControlBlock;
pub use task::{
    all_cpu_affinity_mask, SchedAttr, SchedPolicy, TaskControlBlock, TaskStatus, WaitReason,
    DEFAULT_TIME_SLICE_TICKS, SCHED_RT_PRIO_MAX, SCHED_RT_PRIO_MIN,
};

/// Make current task suspended and switch to the next task
pub fn suspend_current_and_run_next() {
    current_process().pause_cpu_accounting(get_time());
    let task = take_current_task().unwrap();
    let task_cx_ptr = {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.on_cpu = false;
        task_inner.on_rq = false;
        task_inner.task_status = TaskStatus::Runnable;
        task_inner.wait_reason = None;
        task_inner.need_resched = false;
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
        task_inner.on_cpu = false;
        task_inner.on_rq = false;
        task_inner.task_status = TaskStatus::Runnable;
        task_inner.wait_reason = None;
        task_inner.need_resched = false;
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
            task_inner.current_wq_handle = None;
            task_inner.on_cpu = true;
            task_inner.on_rq = false;
            task_inner.need_resched = false;
            None
        } else {
            task_inner.on_cpu = false;
            task_inner.on_rq = false;
            task_inner.task_status = TaskStatus::Interruptible;
            task_inner.wait_reason = Some(reason);
            task_inner.need_resched = false;
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

use crate::board::QEMUExit;

/// Exit the current 'Running' task and run the next task in task list.
pub fn exit_current_and_run_next(reason: ExitReason) {
    let exit_reason = reason;
    let task_exit_code = match exit_reason {
        ExitReason::Exit(code) => code,
        ExitReason::Signal(signum) => -(signum as i32),
    };
    trace!(
        "kernel: pid[{}] exit_current_and_run_next",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    // take from Processor
    let task = take_current_task().unwrap();
    let process = task.process.upgrade().unwrap();
    process.pause_cpu_accounting(get_time());
    let mut task_inner = task.inner_exclusive_access();
    let tid = task_inner.res.as_ref().unwrap().tid;
    // record exit code
    task_inner.exit_code = Some(task_exit_code);
    task_inner.task_status = TaskStatus::Zombie;
    task_inner.on_cpu = false;
    task_inner.on_rq = false;
    task_inner.need_resched = false;
    task_inner.res = None;
    // here we do not remove the thread since we are still using the kstack
    // it will be deallocated when sys_waittid is called
    drop(task_inner);

    // Move the task to stop-wait status, to avoid kernel stack from being freed
    if tid == 0 {
        add_stopping_task(task);
    } else {
        drop(task);
    }
    // however, if this is the main thread of current process
    // the process should terminate at once
    if tid == 0 {
        let pid = process.getpid();
        if pid == IDLE_PID {
            println!(
                "[kernel] Initproc process exit with exit_code {} ...",
                task_exit_code
            );
            if task_exit_code != 0 {
                //crate::sbi::shutdown(255); //255 == -1 for err hint
                crate::board::QEMU_EXIT_HANDLE.exit_failure();
            } else {
                //crate::sbi::shutdown(0); //0 for success hint
                crate::board::QEMU_EXIT_HANDLE.exit_success();
            }
        }
        let mut process_inner = process.inner_exclusive_access();
        // mark this process as a zombie process
        process_inner.is_zombie = true;
        // record process exit reason for wait4/waitpid
        process_inner.exit_reason = exit_reason;

        {
            // move all child processes under init process
            let mut initproc_inner = INITPROC.inner_exclusive_access();
            for child in process_inner.children.iter() {
                child.inner_exclusive_access().parent = Some(Arc::downgrade(&INITPROC));
                initproc_inner.children.push(child.clone());
            }
        }

        // deallocate user res (including tid/trap_cx/ustack) of all threads
        // it has to be done before we dealloc the whole memory_set
        // otherwise they will be deallocated twice
        let mut recycle_res = Vec::<TaskUserRes>::new();
        for task in process_inner.tasks.iter().filter(|t| t.is_some()) {
            let task = task.as_ref().unwrap();
            // if other tasks are Runnable in TaskManager or waiting for a timer to be
            // expired, we should remove them.
            //
            // Mention that we do not need to consider Mutex/Semaphore since they
            // are limited in a single process. Therefore, the blocked tasks are
            // removed when the PCB is deallocated.
            trace!("kernel: exit_current_and_run_next .. remove_inactive_task");
            remove_inactive_task(Arc::clone(&task));
            let mut task_inner = task.inner_exclusive_access();
            if let Some(res) = task_inner.res.take() {
                recycle_res.push(res);
            }
        }
        // dealloc_tid and dealloc_user_res require access to PCB inner, so we
        // need to collect those user res first, then release process_inner
        // for now to avoid deadlock/double borrow problem.
        drop(process_inner);
        recycle_res.clear();

        let (closed_fds, parent_weak, reclaim) = {
            let mut process_inner = process.inner_exclusive_access();
            process_inner.children.clear();
            // deallocate other data in user space i.e. program code/data section
            let token = process_inner.memory_set.token();
            let mask = process_inner.memory_set.loaded_user_harts();
            let release_batch = process_inner.memory_set.recycle_data_pages_deferred();
            let reclaim = DeferredUserReclaim::new(token, mask, release_batch);
            // 关键点：先把 fd 表项整体移出，避免在持有进程自旋锁时触发文件同步或块设备等待。
            let closed_fds = process_inner.take_all_fds();
            process_inner.fd_table.clear();
            // remove all tasks
            process_inner.tasks.clear();

            let parent_weak = process_inner.parent.clone();
            (closed_fds, parent_weak, reclaim)
        };
        reclaim.flush_then_release();
        drop(closed_fds);

        if let Some(parent) = parent_weak.and_then(|pw| pw.upgrade()) {
            add_signal_to_process(&parent, SignalFlags::SIGCHLD);
            parent.wait_exit_queue.wake_one();
        }
    } else {
        let mut process_inner = process.inner_exclusive_access();
        process_inner.mutex_detector.clear_thread(tid);
        process_inner.semaphore_detector.clear_thread(tid);
    }
    drop(process);
    // we do not have to save task context
    let mut _unused = TaskContext::zero_init();
    schedule(&mut _unused as *mut _);
}

/// Mark the current task for deferred rescheduling.
pub fn mark_current_task_need_resched() {
    if let Some(task) = current_task() {
        task.inner_exclusive_access().need_resched = true;
    }
}

/// Returns whether the current task has a pending reschedule request.
pub fn current_task_need_resched() -> bool {
    current_task()
        .map(|task| task.inner_exclusive_access().need_resched)
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
    let hart = crate::hart::hartid();
    let mut task_inner = task.inner_exclusive_access();
    if !matches!(task_inner.task_status, TaskStatus::Running) || !matches!(task_inner.policy, SchedPolicy::Rr) {
        return;
    }
    if task_inner.remaining_slice_ticks > 0 {
        task_inner.remaining_slice_ticks -= 1;
    }
    if task_inner.remaining_slice_ticks > 0 {
        return;
    }
    let prio = task_inner.rt_priority;
    task_inner.reset_time_slice();
    if has_runnable_task_at_or_above(hart, prio) {
        task_inner.need_resched = true;
    }
}

lazy_static! {
    /// Creation of initial process
    ///
    /// the name "initproc" may be changed to any other app name like "usertests",
    /// but we have user_shell, so we don't need to change it.
    pub static ref INITPROC: Arc<ProcessControlBlock> = {
        let inode = open_file("initproc", OpenFlags::RDONLY).expect("Initproc not found! Rebuild image to include initproc.");
        let v = inode.read_all();
        ProcessControlBlock::new(v.as_slice())
    };
}

///Add init process to the manager
pub fn add_initproc() {
    let _initproc = INITPROC.clone();
}

/// Check if the current task has any fatal signal to handle
/// 因为只检查致命信号，所以可不复位pending_signals
pub fn check_fatal_signals_of_current() -> Option<(i32, &'static str)> {
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let pending = process_inner.pending_signals & !process_inner.signal_mask;
    pending.check_error()
}



/// Check if the current process is a zombie process (i.e. has exited but not yet been reaped by its parent).
pub fn current_process_is_zombie() -> bool {
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    process_inner.is_zombie
}

/// Add signal to target process.
///
/// When the delivered signal introduces a **newly pending and unmasked** bit,
/// proactively wake poll waiters of this process so `ppoll` can return `EINTR`.
pub fn add_signal_to_process(process: &Arc<ProcessControlBlock>, signal: SignalFlags) {
    let (pid, should_notify_poll) = {
        let mut process_inner = process.inner_exclusive_access();
        let newly_pending = signal & !process_inner.pending_signals;
        process_inner.pending_signals |= signal;
        let newly_unmasked = newly_pending & !process_inner.signal_mask;
        (process.getpid(), !newly_unmasked.is_empty())
    };

    crate::signal::notify_signal_wait_pid(pid, signal.bits());

    if should_notify_poll {
        debug!(
            "add_signal_to_process: pid={} added signal {:#x} which is unmasked, should notify poll",
            pid,
            signal.bits()
        );
        crate::poll::notify_poll_signal_pid(pid);

        // When an unmasked signal arrives, wake interruptible tasks so they
        // return -EINTR.  Use the WaitQueueHandle (if the task is enqueued)
        // to properly remove it from its wait queue before waking.
        // Keyed-poll tasks are already handled by notify_poll_signal_pid above.
        let tasks: Vec<Arc<TaskControlBlock>> = {
            let process_inner = process.inner_exclusive_access();
            process_inner
                .tasks
                .iter()
                .filter_map(|slot| slot.as_ref().map(Arc::clone))
                .collect()
        };
        for task in tasks {
            if task_has_inflight_keyed_poll_wait(&task) {
                continue;
            }
            let handle = {
                let task_inner = task.inner_exclusive_access();
                task_inner.current_wq_handle.clone()
            };
            if let Some(handle) = handle {
                debug!("Wakeup with handle, reason = {:?}", task.inner_exclusive_access().wait_reason.unwrap_or(WaitReason::Unknown));
                handle.wake_waiter(&task);
            } else {
                debug!("Wakeup task without handle, reason = {:?}", task.inner_exclusive_access().wait_reason.unwrap_or(WaitReason::Unknown));
                let should_wake = {
                    let task_inner = task.inner_exclusive_access();
                    matches!(task_inner.task_status, TaskStatus::Interruptible)
                };
                if should_wake {
                    wakeup_task(task);
                }
            }
        }
    }
}

/// Add signal to the current task
pub fn current_add_signal(signal: SignalFlags) {
    let process = current_process();
    add_signal_to_process(&process, signal);
}

/// 扫描所有进程的 interval timer，到期则投递对应信号。
pub fn check_itimers_of_all_processes(now_raw: usize, now_realtime_ns: u64) {
    let processes: Vec<Arc<ProcessControlBlock>> = {
        let map = self::runqueue::PID2PCB.lock();
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
    crate::signal::cleanup_signal_wait_for_task(&task);
    trace!("kernel: remove_inactive_task .. remove_timer");
    remove_timer(Arc::clone(&task));
}

/// Map an anonymous area in current process with given permission.
pub fn mmap_current_process(start: VirtAddr, end: VirtAddr, perm: MapPermission) -> bool {
    current_process().mmap(start, end, perm)
}

/// Unmap an anonymous area in current process.
pub fn munmap_current_process(start: VirtAddr, end: VirtAddr) -> bool {
    current_process().munmap(start, end)
}

/// Change permissions on a range in current process.
pub fn mprotect_current_process(start: VirtAddr, end: VirtAddr, perm: MapPermission) -> bool {
    current_process().mprotect(start, end, perm)
}
