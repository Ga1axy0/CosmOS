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
    add_stopping_task, list_pids, pid2process, remove_from_pid2process, remove_task, schedule,
    take_current_task, TaskContext,
};
use crate::fs::{open_file, OpenFlags};
use crate::ipc;
use crate::poll::task_has_inflight_keyed_poll_wait;
use crate::signal::cleanup_signal_wait_for_task;
use crate::sync::{futex_wake_addr, cleanup_futex_wait_for_task};
use crate::syscall::{read_pod_from_process_user, write_pod_to_process_user};
use crate::mm::{DeferredUserReclaim, MapPermission, VirtAddr};
use crate::timer::get_time;
use crate::timer::remove_timer;
use alloc::{collections::BTreeMap, sync::Arc, vec, vec::Vec};
use lazy_static::*;
pub(crate) use id::recycle_deferred_kstack_ids;
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
use alloc::string::String;

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
    task_inner.sched.on_cpu = false;
    task_inner.sched.on_rq = false;
    task_inner.sched.resched_reason = None;
    task_inner.clear_child_tid = 0;
    // here we do not remove the thread since we are still using the kstack
    // it will be deallocated when sys_waittid is called
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
                    clear_child_tid,
                    read_back
                );
            }
            Err(err) => {
                warn!(
                    "exit_current_and_run_next: failed to clear child_tid at {:#x}: {:?}",
                    clear_child_tid,
                    err
                );
            }
        }
        let woke = futex_wake_addr(clear_child_tid, 1);
        debug!(
            "exit_current_and_run_next: futex_wake_addr({:#x}, 1) -> {}",
            clear_child_tid,
            woke
        );
    }
    cleanup_signal_wait_for_task(&task);
    cleanup_futex_wait_for_task(&task);
    remove_timer(Arc::clone(&task));
    if let Some(thread_id) = thread_id {
        remove_from_tid2task(thread_id);
    }

    // Move the task to stop-wait status, to avoid kernel stack from being freed
    if tid == Some(0) {
        add_stopping_task(task);
    } else {
        drop(task);
    }
    // however, if this is the main thread of current process
    // the process should terminate at once
    if tid == Some(0) {
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
            let thread_id = {
                let mut task_inner = task.inner_exclusive_access();
                task_inner.exit_code.get_or_insert(task_exit_code);
                task_inner.task_status = TaskStatus::Zombie;
                task_inner.wait_reason = None;
                task_inner.sched.on_cpu = false;
                task_inner.sched.on_rq = false;
                task_inner.sched.resched_reason = None;
                task_inner.current_wq_handle = None;
                task_inner.res.as_ref().map(|res| res.thread_id())
            };
            if let Some(thread_id) = thread_id {
                remove_from_tid2task(thread_id);
            }
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
        if let Some(tid) = tid {
            process_inner.mutex_detector.clear_thread(tid);
            process_inner.semaphore_detector.clear_thread(tid);
        }
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
    static ref TID2TASK: crate::sync::SpinNoIrqLock<BTreeMap<usize, alloc::sync::Weak<TaskControlBlock>>> =
        crate::sync::SpinNoIrqLock::new(BTreeMap::new());
}

///Add init process to the manager
pub fn add_initproc() {
    let _initproc = INITPROC.clone();
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
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let task_inner = task.inner_exclusive_access();
    let pending = (task_inner.pending_signals | process_inner.pending_signals)
        & !task_inner.signal_mask.without_unblockable();
    for signum in 1..=MAX_SIG {
        let Some(flag) = SignalBit::from_signum(signum as u32) else {
            continue;
        };
        if !pending.contains(flag) {
            continue;
        }
        let action = process_inner.signal_actions.table[signum];
        if action.handler == SIG_DFL {
            if let Some(error) = flag.check_error() {
                return Some(error);
            }
        }
    }
    None
}



/// Check if the current process is a zombie process (i.e. has exited but not yet been reaped by its parent).
pub fn current_process_is_zombie() -> bool {
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    process_inner.is_zombie
}

fn first_signum_in_set(signal: SignalBit) -> Option<usize> {
    for signum in 1..=MAX_SIG {
        let Some(flag) = SignalBit::from_signum(signum as u32) else {
            continue;
        };
        if signal.contains(flag) {
            return Some(signum);
        }
    }
    None
}

/// Add signal to target process.
///
/// When the delivered signal introduces a **newly pending and unmasked** bit,
/// proactively wake poll waiters of this process so `ppoll` can return `EINTR`.
pub fn add_signal_to_process(process: &Arc<ProcessControlBlock>, signal: SignalBit) {
    let signum = first_signum_in_set(signal).map(|num| num as i32).unwrap_or_default();
    add_signal_to_process_with_siginfo(process, signal, SigInfo::for_kernel(signum));
}

/// Add signal to target process with explicit siginfo metadata.
pub fn add_signal_to_process_with_siginfo(
    process: &Arc<ProcessControlBlock>,
    signal: SignalBit,
    siginfo: SigInfo,
) {
    let (pid, newly_pending, tasks) = {
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
        (process.getpid(), newly_pending, tasks)
    };

    crate::signal::notify_signal_wait_pid(pid, signal.bits());

    let deliverable_tasks = tasks
        .into_iter()
        .filter(|task| {
            let task_inner = task.inner_exclusive_access();
            !(newly_pending & !task_inner.signal_mask.without_unblockable()).is_empty()
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
    let signum = first_signum_in_set(signal).map(|num| num as i32).unwrap_or_default();
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
        (
            task_inner.res.as_ref().unwrap().thread_id(),
            task_inner.res.as_ref().unwrap().tid,
            task_inner.signal_mask.bits(),
            newly_pending & !task_inner.signal_mask.without_unblockable(),
        )
    };

    crate::signal::notify_signal_wait_task(task, signal.bits());

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
    if crate::hart::hartid() != 0 {
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
    remove_timer(Arc::clone(&task));
}

/// Map an anonymous area in current process with given permission.
pub fn mmap_current_process(
    start: VirtAddr,
    end: VirtAddr,
    perm: MapPermission,
) -> Result<(), crate::syscall::errno::ERRNO> {
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
