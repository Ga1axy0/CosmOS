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
    process.attach_task(Arc::clone(&new_task));
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
    let task_inner = task.inner_exclusive_access();
    let mut process_inner = process.inner_exclusive_access();
    // a thread cannot wait for itself
    if task_inner.res.as_ref().unwrap().tid == tid {
        return -(ERRNO::EINVAL as i32);
    }
    let waited_task = process_inner.tasks.get(tid).and_then(|t| t.as_ref());
    let exit_code = match waited_task {
        None => return -(ERRNO::ESRCH as i32), // thread does not exist
        Some(t) => t.inner_exclusive_access().exit_code,
    };
    if let Some(code) = exit_code {
        // Take the task out of the slot so we can drop it after releasing locks.
        let waited_task = process_inner.tasks[tid].take();
        if let Some(waited_task) = waited_task.as_ref() {
            let thread_id = waited_task
                .inner_exclusive_access()
                .res
                .as_ref()
                .unwrap()
                .thread_id();
            remove_from_tid2task(thread_id);
        }
        // Extract user resources from the zombie task to avoid deadlock:
        // TaskUserRes::drop() needs the process lock, so we must drop it first.
        let res = waited_task
            .as_ref()
            .and_then(|t| t.inner_exclusive_access().res.take());
        drop(process_inner);
        drop(task_inner);
        drop(res);
        drop(waited_task);
        code
    } else {
        -(ERRNO::EAGAIN as i32) // thread has not exited yet
    }
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
