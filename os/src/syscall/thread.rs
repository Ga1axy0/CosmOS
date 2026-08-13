use crate::{
    mm::kernel_token,
    sched::add_task,
    syscall::errno::ERRNO,
    task::{current_process, current_task, remove_from_tid2task},
    trap::{trap_handler, TrapContext},
};
use alloc::sync::Arc;

fn linux_visible_tid() -> isize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .thread_id() as isize
}
/// thread create syscall
pub fn sys_thread_create(entry: usize, arg: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_thread_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let task = current_task().unwrap();
    let process = task.process.upgrade().unwrap();
    let (ustack_base, sched_attr, affinity_mask, signal_mask) = {
        let task_inner = task.inner_exclusive_access();
        (
            task_inner.res.as_ref().unwrap().ustack_base,
            task_inner.sched_attr(),
            task_inner.sched.cpu_affinity_mask,
            task_inner.signal_mask,
        )
    };
    // create a new thread
    let new_task = match process.create_task(ustack_base, true, sched_attr) {
        Ok(task) => task,
        Err(_) => return -(ERRNO::ENOMEM as isize),
    };
    {
        let mut new_task_inner = new_task.inner_exclusive_access();
        new_task_inner.sched.cpu_affinity_mask = affinity_mask;
        new_task_inner.signal_mask = signal_mask;
    }
    let new_task_inner = new_task.inner_exclusive_access();
    let new_task_res = new_task_inner.res.as_ref().unwrap();
    let new_task_tid = new_task_res.thread_id();
    let new_task_trap_cx = new_task_inner.get_trap_cx();
    *new_task_trap_cx = TrapContext::app_init_context(
        entry,
        new_task_res.ustack_top(),
        kernel_token(),
        new_task.kstack.get_top(),
        trap_handler as usize,
    );
    new_task_trap_cx.set_user_arg(0, arg);
    drop(new_task_inner);
    if process.attach_task(Arc::clone(&new_task)).is_err() {
        return -(ERRNO::EAGAIN as isize);
    }
    add_task(new_task);
    new_task_tid as isize
}
/// get current thread id syscall
pub fn sys_gettid() -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_gettid",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    linux_visible_tid()
}

/// wait for a thread to exit syscall
///
/// - Returns `EINVAL`  if `tid` is the current thread (cannot wait for self).
/// - Returns `ESRCH`   if the thread does not exist.
/// - Returns `EAGAIN`  if the thread has not exited yet.
/// - Otherwise returns the thread's exit code.
pub fn sys_waittid(tid: usize) -> i32 {
    trace!(
        "kernel:pid[{}] tid[{}] sys_waittid",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let task = current_task().unwrap();
    let process = task.process.upgrade().unwrap();
    // thread_create() and gettid() expose the globally allocated Linux thread
    // id, while ProcessControlBlock::tasks is indexed by a process-local slot.
    // Snapshot the table, then inspect each task without holding process-inner
    // so that task -> process lock ordering cannot be inverted.
    let tasks = {
        let process_inner = process.inner_exclusive_access();
        process_inner
            .tasks
            .iter()
            .filter_map(|slot| slot.as_ref().cloned())
            .collect::<alloc::vec::Vec<_>>()
    };
    let waited_task = tasks.into_iter().find(|candidate| {
        candidate
            .inner_exclusive_access()
            .res
            .as_ref()
            .is_some_and(|res| res.thread_id() == tid)
    });
    let Some(waited_task) = waited_task else {
        return -(ERRNO::ESRCH as i32);
    };
    if Arc::ptr_eq(&task, &waited_task) {
        return -(ERRNO::EINVAL as i32);
    }
    let (local_tid, exit_code) = {
        let waited_inner = waited_task.inner_exclusive_access();
        (
            waited_inner.res.as_ref().map(|res| res.tid),
            waited_inner.exit_code,
        )
    };
    let (Some(local_tid), Some(code)) = (local_tid, exit_code) else {
        return -(ERRNO::EAGAIN as i32);
    };

    // Revalidate and detach atomically with respect to another waiter.  The
    // target task lock is deliberately acquired only after process-inner has
    // been released.
    let detached_task = {
        let mut process_inner = process.inner_exclusive_access();
        let slot_matches = process_inner
            .tasks
            .get(local_tid)
            .and_then(|slot| slot.as_ref())
            .is_some_and(|registered| Arc::ptr_eq(registered, &waited_task));
        if slot_matches {
            process_inner.tasks[local_tid].take()
        } else {
            None
        }
    };
    let Some(detached_task) = detached_task else {
        return -(ERRNO::ESRCH as i32);
    };

    let (thread_id, res) = {
        let mut waited_inner = detached_task.inner_exclusive_access();
        (
            waited_inner.res.as_ref().map(|res| res.thread_id()),
            waited_inner.res.take(),
        )
    };
    if let Some(thread_id) = thread_id {
        remove_from_tid2task(thread_id);
    }
    // TaskUserRes::drop() acquires process-inner, so dropping it is kept out
    // of both the task and process critical sections.
    drop(res);
    drop(detached_task);
    code
}

/// 临时实现，只返回当前线程的 tid
pub fn sys_set_tid_address(tidptr: *mut i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_set_tid_address",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    inner.clear_child_tid = tidptr as usize;
    drop(inner);
    let _process = current_process();
    linux_visible_tid()
}
