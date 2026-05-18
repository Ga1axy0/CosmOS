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
mod wait_queue;
#[allow(clippy::module_inception)]
mod task;

use self::id::TaskUserRes;
use crate::sched::{
    add_stopping_task, remove_from_pid2process, remove_task, schedule, take_current_task,
    TaskContext,
};
use crate::fs::{open_file, OpenFlags};
use crate::ipc;
use crate::poll::task_has_inflight_keyed_poll_wait;
use crate::syscall::{futex_wake_addr, write_pod_to_process_user};
use crate::mm::{DeferredUserReclaim, MapPermission, VirtAddr};
use crate::timer::get_time;
use crate::timer::remove_timer;
use alloc::{sync::Arc, vec::Vec};
use lazy_static::*;
pub use id::{kstack_alloc, pid_alloc, KernelStack, PidHandle, IDLE_PID};
pub use crate::sched::{
    block_current_and_run_next, current_process, current_task, current_trap_cx,
    current_trap_cx_user_va, current_user_token, schedule_if_needed,
    suspend_current_and_run_next, suspend_current_and_run_next_with_slice_reset, wakeup_task,
    yield_current_and_run_next,
};
pub use crate::signal::{
    check_signals_of_current, handle_signals, MContext, MAX_SIG, SaFlags, SigInfo, SignalAction,
    SignalActions, SignalBit, StackT, UContext, SIG_DFL, SIG_IGN,
};
pub use wait_queue::{WaitQueue, WaitQueueHandle, WaitQueueKeyed};
pub use process::{ExitReason, FdEntry, FdFlags, ShmAttachment};
pub(crate) use process::ProcessControlBlock;
pub use crate::sched::{
    clamp_nice, nice_to_weight, DEFAULT_TIME_SLICE_TICKS, MAX_NICE, MIN_NICE, NICE_0_LOAD,
    ReschedReason, SchedAttr, SchedPolicy, SCHED_RT_PRIO_MAX, SCHED_RT_PRIO_MIN,
};
pub use task::{
    all_cpu_affinity_mask, TaskControlBlock, TaskSchedState, TaskStatus, WaitReason,
};
pub(crate) use task::TaskControlBlockInner;

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
    let clear_child_tid = task_inner.clear_child_tid;
    // record exit code
    task_inner.exit_code = Some(task_exit_code);
    task_inner.task_status = TaskStatus::Zombie;
    task_inner.sched.on_cpu = false;
    task_inner.sched.on_rq = false;
    task_inner.sched.resched_reason = None;
    task_inner.res = None;
    task_inner.clear_child_tid = 0;
    // here we do not remove the thread since we are still using the kstack
    // it will be deallocated when sys_waittid is called
    drop(task_inner);
    if clear_child_tid != 0 {
        let _ = write_pod_to_process_user(&process, clear_child_tid as *mut i32, &0i32);
        let _ = futex_wake_addr(clear_child_tid, 1);
    }

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

        let (closed_fds, parent_weak, reclaim, shm_attachments) = {
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
            let shm_attachments = core::mem::take(&mut process_inner.shm_attachments);
            (closed_fds, parent_weak, reclaim, shm_attachments)
        };
        reclaim.flush_then_release();
        drop(closed_fds);
        for attachment in shm_attachments {
            ipc::detach_segment(attachment.shmid);
        }

        if let Some(parent) = parent_weak.and_then(|pw| pw.upgrade()) {
            add_signal_to_process(&parent, SignalBit::SIGCHLD);
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

lazy_static! {
    /// Creation of initial process
    ///
    /// the name "initproc" may be changed to any other app name like "usertests",
    /// but we have user_shell, so we don't need to change it.
    pub static ref INITPROC: Arc<ProcessControlBlock> = {
        let inode = open_file("initproc", OpenFlags::RDONLY).expect("Initproc not found! Rebuild image to include initproc.");
        let v = inode.read_all();
        ProcessControlBlock::new(v.as_slice(), String::from("/initproc"))
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
pub fn add_signal_to_process(process: &Arc<ProcessControlBlock>, signal: SignalBit) {
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
pub fn current_add_signal(signal: SignalBit) {
    let process = current_process();
    add_signal_to_process(&process, signal);
}

/// 扫描所有进程的 interval timer，到期则投递对应信号。
pub fn check_itimers_of_all_processes(now_raw: usize, now_realtime_ns: u64) {
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

/// Sync a mapped range in current process.
pub fn msync_current_process(start: VirtAddr, end: VirtAddr) -> Result<(), crate::syscall::errno::ERRNO> {
    current_process().msync(start, end)
}

/// Change permissions on a range in current process.
pub fn mprotect_current_process(start: VirtAddr, end: VirtAddr, perm: MapPermission) -> bool {
    current_process().mprotect(start, end, perm)
}
