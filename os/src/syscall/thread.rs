use crate::{
    mm::kernel_token,
    syscall::errno::ERRNO,
    task::{add_task, current_task, TaskControlBlock},
    trap::{trap_handler, TrapContext},
};
use alloc::sync::Arc;
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
    // create a new thread
    let new_task = Arc::new(TaskControlBlock::new(
        Arc::clone(&process),
        task.inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .ustack_base,
        true,
    ));
    // add new task to scheduler
    add_task(Arc::clone(&new_task));
    let new_task_inner = new_task.inner_exclusive_access();
    let new_task_res = new_task_inner.res.as_ref().unwrap();
    let new_task_tid = new_task_res.tid;
    let mut process_inner = process.inner_exclusive_access();
    // add new thread to current process
    let tasks = &mut process_inner.tasks;
    while tasks.len() < new_task_tid + 1 {
        tasks.push(None);
    }
    tasks[new_task_tid] = Some(Arc::clone(&new_task));
    let new_task_trap_cx = new_task_inner.get_trap_cx();
    *new_task_trap_cx = TrapContext::app_init_context(
        entry,
        new_task_res.ustack_top(),
        kernel_token(),
        new_task.kstack.get_top(),
        trap_handler as usize,
    );
    (*new_task_trap_cx).x[10] = arg;
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
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .tid as isize
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
        // dealloc the exited thread
        process_inner.tasks[tid] = None;
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
    current_task()
    .unwrap()
    .inner_exclusive_access()
    .res
    .as_ref()
    .unwrap()
    .tid
    .try_into()
    .unwrap()
}