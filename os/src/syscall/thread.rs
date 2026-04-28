use crate::{
    mm::kernel_token,
    sched::activate_task,
    syscall::errno::ERRNO,
    task::current_task,
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
    let (ustack_base, sched_attr) = {
        let task_inner = task.inner_exclusive_access();
        (
            task_inner.res.as_ref().unwrap().ustack_base,
            task_inner.sched_attr(),
        )
    };
    // create a new thread
    let new_task = process.create_task(ustack_base, true, sched_attr);
    {
        let affinity_mask = task.inner_exclusive_access().sched.cpu_affinity_mask;
        new_task.inner_exclusive_access().sched.cpu_affinity_mask = affinity_mask;
    }
    let new_task_inner = new_task.inner_exclusive_access();
    let new_task_res = new_task_inner.res.as_ref().unwrap();
    let new_task_tid = new_task_res.tid;
    let new_task_trap_cx = new_task_inner.get_trap_cx();
    *new_task_trap_cx = TrapContext::app_init_context(
        entry,
        new_task_res.ustack_top(),
        kernel_token(),
        new_task.kstack.get_top(),
        trap_handler as usize,
    );
    (*new_task_trap_cx).x[10] = arg;
    drop(new_task_inner);
    process.attach_task(Arc::clone(&new_task));
    activate_task(new_task);
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
