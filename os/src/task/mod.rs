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

mod context;
mod id;
mod manager;
mod action;
mod process;
mod processor;
mod signal;
mod switch;
#[allow(clippy::module_inception)]
mod task;

use self::id::TaskUserRes;
use crate::fs::{open_file, OpenFlags};
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
pub use manager::{add_task, pid2process, remove_from_pid2process, remove_task, wakeup_task};
pub use action::{SignalAction, SignalActions};
pub use processor::{
    current_kstack_top, current_process, current_task, current_trap_cx, current_trap_cx_user_va,
    current_user_token, run_tasks, schedule, take_current_task,
};
pub use process::ExitReason;
pub use signal::{SignalFlags, MAX_SIG};
pub use task::{TaskControlBlock, TaskStatus};

/// Make current task suspended and switch to the next task
pub fn suspend_current_and_run_next() {
    current_process().pause_cpu_accounting(get_time());
    // There must be an application running.
    let task = take_current_task().unwrap();

    // ---- access current TCB exclusively
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
    // Change status to Ready
    task_inner.task_status = TaskStatus::Ready;
    drop(task_inner);
    // ---- release current TCB

    // push back to ready queue.
    add_task(task);
    // jump to scheduling cycle
    schedule(task_cx_ptr);
}

/// Make current task blocked and switch to the next task.
pub fn block_current_and_run_next() {
    current_process().pause_cpu_accounting(get_time());
    let task = take_current_task().unwrap();
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
    task_inner.task_status = TaskStatus::Blocked;
    drop(task_inner);
    schedule(task_cx_ptr);
}

use crate::board::QEMUExit;

/// Exit the current 'Running' task and run the next task in task list.
pub fn exit_current_and_run_next(reason: ExitReason) {
    let exit_reason = reason.into();
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
            parent.wait_exit_condvar.signal();
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

/// Add signal to the current task
pub fn current_add_signal(signal: SignalFlags) {
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    process_inner.pending_signals |= signal;
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
