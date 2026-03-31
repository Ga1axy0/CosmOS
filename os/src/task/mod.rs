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
#[path = "../sched/manager.rs"]
mod manager;
mod action;
mod process;
#[path ="../sched/processor.rs"]
mod processor;
mod signal;
#[path ="../sched/switch.rs"]
mod switch;
mod wait_queue;
#[allow(clippy::module_inception)]
mod task;

use self::id::TaskUserRes;
use crate::fs::{open_file, OpenFlags};
use crate::poll::task_has_inflight_keyed_poll_wait;
use crate::task::manager::add_stopping_task;
use crate::timer::remove_timer;
use crate::timer::get_time;
use crate::mm::{MapPermission, VirtAddr};
use alloc::{sync::Arc, vec::Vec};
use lazy_static::*;
use manager::fetch_task;
use process::ProcessControlBlock;
use switch::__switch;

pub use context::TaskContext;
pub use id::{kstack_alloc, pid_alloc, KernelStack, PidHandle, IDLE_PID};
pub use manager::{
    add_task, add_task_global, add_task_pre_blocked, add_task_pre_ready, pid2process,
    promote_pre_blocked_tasks, promote_pre_ready_tasks,
    remove_from_pid2process, remove_task, wakeup_task,
};
pub use action::{SignalAction, SignalActions};
pub use processor::{
    current_kstack_top, current_process, current_processor, current_task, current_trap_cx,
    current_trap_cx_user_va, current_user_token, restore_current_task, run_tasks, schedule,
    take_current_task,
};
pub use wait_queue::{WaitQueue, WaitQueueKeyed};
pub use process::{ExitReason, FdEntry, FdFlags};
pub use signal::{SignalFlags, MAX_SIG};
pub use task::{TaskControlBlock, TaskStatus, WaitReason};

/// Make current task suspended and switch to the next task
pub fn suspend_current_and_run_next() {
    current_process().pause_cpu_accounting(get_time());
    // There must be an application running.
    let task = take_current_task().unwrap();

    // ---- access current TCB exclusively
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
    // 当前任务先进入预就绪状态，等本 hart 切回 idle 后再正式发布。
    task_inner.task_status = TaskStatus::PreReady;
    drop(task_inner);
    // ---- release current TCB

    // 延迟发布到真正的就绪队列，避免其他 hart 在本 hart 尚未切走时抢到该任务。
    add_task_pre_ready(task);
    // jump to scheduling cycle
    schedule(task_cx_ptr);
}

/// Make current task blocked and switch to the next task.
pub fn block_current_and_run_next(reason: WaitReason) {
    let task = take_current_task().unwrap();
    let process = task.process.upgrade().unwrap();
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
    task_inner.task_status = TaskStatus::Blocked;
    task_inner.wait_reason = Some(reason);
    task_inner.wake_pending = false;
    drop(task_inner);
    process.pause_cpu_accounting(get_time());
    schedule(task_cx_ptr);
}

/// Complete transition from `PreBlocked` to `Blocked` and switch out current task.
///
/// If a wakeup arrives before the context switch happens, the task remains
/// running on current hart and this function returns without scheduling away.
pub fn block_current_preblocked_and_run_next() {
    let task = take_current_task().unwrap();
    let process = task.process.upgrade().unwrap();
    let task_cx_ptr: *mut TaskContext;
    {
        let mut task_inner = task.inner_exclusive_access();
        task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
        debug_assert!(matches!(task_inner.task_status, TaskStatus::PreBlocked));
        
        if task_inner.wake_pending {
            task_inner.task_status = TaskStatus::Running;
            task_inner.wait_reason = None;
            task_inner.wake_pending = false;
            drop(task_inner);
            restore_current_task(task);
            return;
        }

        // 注意：这里不要提前发布为 Blocked。
        // 否则在真正 __switch 之前，其他 hart 可能看到 Blocked 并把该任务唤醒入队，
        // 导致同一任务在两个 hart 上并发执行（共享同一内核栈）。
        task_inner.task_status = TaskStatus::PreBlocked;
    }
    process.pause_cpu_accounting(get_time());

    // 延迟发布到 pre_blocked 队列，等本 hart 切回 idle 后再转成 Blocked/Ready。
    add_task_pre_blocked(task);
    schedule(task_cx_ptr);
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
        remove_from_pid2process(pid);
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
            // if other tasks are Ready in TaskManager or waiting for a timer to be
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

        let mut process_inner = process.inner_exclusive_access();
        process_inner.children.clear();
        // deallocate other data in user space i.e. program code/data section
        process_inner.memory_set.recycle_data_pages();
        // drop file descriptors
        process_inner.fd_table.clear();
        // remove all tasks
        process_inner.tasks.clear();

        let parent_weak = process_inner.parent.clone();
        
        if let Some(parent) = parent_weak.and_then(|pw| pw.upgrade()) {
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

    if should_notify_poll {
        crate::poll::notify_poll_signal_pid(pid);

        // `ppoll` 在注册表耗尽时会走 ENOSPC 回退路径，直接以 `WaitReason::Poll`
        // 阻塞并依赖短时 timer 唤醒重扫；该路径不在 keyed poll registry 中，
        // 需要在信号投递时主动唤醒，确保尽快返回 EINTR。
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
            let should_wake = {
                let task_inner = task.inner_exclusive_access();
                matches!(task_inner.wait_reason, Some(WaitReason::Poll))
                    && matches!(task_inner.task_status, TaskStatus::Blocked | TaskStatus::PreBlocked)
            };
            if should_wake {
                wakeup_task(task);
            }
        }
    }
}

/// Add signal to the current task
pub fn current_add_signal(signal: SignalFlags) {
    let process = current_process();
    add_signal_to_process(&process, signal);
}

/// the inactive(blocked) tasks are removed when the PCB is deallocated.(called by exit_current_and_run_next)
pub fn remove_inactive_task(task: Arc<TaskControlBlock>) {
    remove_task(Arc::clone(&task));
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
