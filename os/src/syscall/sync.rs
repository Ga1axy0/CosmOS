use crate::mm::translated_refmut;
use crate::sync::{Condvar, Mutex, MutexBlocking, MutexSpin, Semaphore};
use crate::syscall_body;
use crate::task::{
    WaitReason, block_current_and_run_next, current_process, current_task, current_user_token
};
use crate::timer::{add_timer, get_time_ms};
use crate::syscall::errno::{ERRNO, OrErrno};
use alloc::sync::Arc;

const DEADLOCK_DETECTED: isize = -0xDEAD;

fn current_tid() -> usize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .tid
}

/// UtsName struct for uname syscall
#[repr(C)]
pub struct Timespec {
    tv_sec: usize,
    tv_nsec: usize,
}

/// sleep syscall
/// Though the syscall is named `nanosleep`, it actually takes milliseconds as input for simplicity.
pub fn sys_nanosleep(req: *const Timespec, rem: *mut Timespec) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_nanosleep",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    
    let token = current_user_token();
    syscall_body!({
        let _ = rem;
        let timespec = translated_refmut(token, req as *mut Timespec).or_errno(ERRNO::EFAULT)?;
        let current_time = get_time_ms();
        let expire_ms = current_time + timespec.tv_sec * 1_000 + timespec.tv_nsec / 1_000_000;
        debug!(
            "nanosleep: current_time = {}, expire_time = {}",
            current_time,
            expire_ms,
        );
        let task = current_task().unwrap();
        add_timer(expire_ms, task);
        block_current_and_run_next(WaitReason::Nanosleep);
        Ok(0)
    })
}

/// mutex create syscall
pub fn sys_mutex_create(blocking: bool) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mutex: Option<Arc<dyn Mutex>> = if !blocking {
        Some(Arc::new(MutexSpin::new()))
    } else {
        Some(Arc::new(MutexBlocking::new()))
    };
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .mutex_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.mutex_list[id] = mutex;
        id
    } else {
        process_inner.mutex_list.push(mutex);
        process_inner.mutex_list.len() - 1
    };
    process_inner.mutex_detector.init_resource(id, 1);
    id as isize
}
/// mutex lock syscall
pub fn sys_mutex_lock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_lock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let tid = current_tid();
    // Retrieve (and optionally deadlock-check) while holding inner, then drop before blocking.
    let mutex: Arc<dyn Mutex> = {
        let mut process_inner = process.inner_exclusive_access();
        let mutex = match process_inner
            .mutex_list
            .get(mutex_id)
            .and_then(|m| m.as_ref())
        {
            Some(m) => Arc::clone(m),
            None => return -(ERRNO::EINVAL as isize),
        };
        if process_inner.deadlock_enabled
            && !process_inner.mutex_detector.begin_request(tid, mutex_id)
        {
            return DEADLOCK_DETECTED;
        }
        mutex
    };
    mutex.lock();
    let mut process_inner = process.inner_exclusive_access();
    process_inner.mutex_detector.finish_request(tid, mutex_id);
    0
}
/// mutex unlock syscall
pub fn sys_mutex_unlock(mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_mutex_unlock",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let tid = current_tid();
    let mutex: Arc<dyn Mutex> = {
        let process_inner = process.inner_exclusive_access();
        match process_inner
            .mutex_list
            .get(mutex_id)
            .and_then(|m| m.as_ref())
        {
            Some(m) => Arc::clone(m),
            None => return -(ERRNO::EINVAL as isize),
        }
    };
    mutex.unlock();
    let mut process_inner = process.inner_exclusive_access();
    process_inner.mutex_detector.release(tid, mutex_id);
    0
}
/// semaphore create syscall
pub fn sys_semaphore_create(res_count: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .semaphore_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.semaphore_list[id] = Some(Arc::new(Semaphore::new(res_count)));
        id
    } else {
        process_inner
            .semaphore_list
            .push(Some(Arc::new(Semaphore::new(res_count))));
        process_inner.semaphore_list.len() - 1
    };
    process_inner.semaphore_detector.init_resource(id, res_count);
    id as isize
}
/// semaphore up syscall
pub fn sys_semaphore_up(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_up",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let tid = current_tid();
    let sem: Arc<Semaphore> = {
        let process_inner = process.inner_exclusive_access();
        match process_inner
            .semaphore_list
            .get(sem_id)
            .and_then(|s| s.as_ref())
        {
            Some(s) => Arc::clone(s),
            None => return -(ERRNO::EINVAL as isize),
        }
    };
    sem.up();
    let mut process_inner = process.inner_exclusive_access();
    process_inner.semaphore_detector.release(tid, sem_id);
    0
}
/// semaphore down syscall
pub fn sys_semaphore_down(sem_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_semaphore_down",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let tid = current_tid();
    let sem: Arc<Semaphore> = {
        let mut process_inner = process.inner_exclusive_access();
        let sem = match process_inner
            .semaphore_list
            .get(sem_id)
            .and_then(|s| s.as_ref())
        {
            Some(s) => Arc::clone(s),
            None => return -(ERRNO::EINVAL as isize),
        };
        if process_inner.deadlock_enabled
            && !process_inner.semaphore_detector.begin_request(tid, sem_id)
        {
            return DEADLOCK_DETECTED;
        }
        sem
    };
    sem.down();
    let mut process_inner = process.inner_exclusive_access();
    process_inner.semaphore_detector.finish_request(tid, sem_id);
    0
}
/// condvar create syscall
pub fn sys_condvar_create() -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_create",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .condvar_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.condvar_list[id] = Some(Arc::new(Condvar::new()));
        id
    } else {
        process_inner
            .condvar_list
            .push(Some(Arc::new(Condvar::new())));
        process_inner.condvar_list.len() - 1
    };
    id as isize
}
/// condvar signal syscall
pub fn sys_condvar_signal(condvar_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_signal",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    drop(process_inner);
    condvar.signal();
    0
}
/// condvar wait syscall
pub fn sys_condvar_wait(condvar_id: usize, mutex_id: usize) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_condvar_wait",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_task()
            .unwrap()
            .inner_exclusive_access()
            .res
            .as_ref()
            .unwrap()
            .tid
    );
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    condvar.wait(mutex);
    0
}
/// enable deadlock detection syscall
///
/// YOUR JOB: Implement deadlock detection, but might not all in this syscall
pub fn sys_enable_deadlock_detect(_enabled: usize) -> isize {
    trace!("kernel: sys_enable_deadlock_detect");
    match _enabled {
        0 => {
            let process = current_process();
            process.inner_exclusive_access().deadlock_enabled = false;
            0
        }
        1 => {
            let process = current_process();
            let mut process_inner = process.inner_exclusive_access();
            if !process_inner.mutex_detector.is_safe_state()
                || !process_inner.semaphore_detector.is_safe_state()
            {
                return -1;
            }
            process_inner.deadlock_enabled = true;
            0
        }
        _ => -1,
    }
}
