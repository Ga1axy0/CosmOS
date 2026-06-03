use crate::sync::{Condvar, Mutex, MutexBlocking, MutexSpin, Semaphore, futex_queue, futex_wait_mark_ready, futex_wake_addr, futex_wait_addr};
use crate::syscall_body;
use crate::syscall::{read_pod_from_user, times::Timespec};
use crate::sched::block_current_and_run_next;
use crate::task::{current_process, current_task, WaitReason};
use crate::timer::{add_timer_ns, get_time_ns};
use crate::syscall::errno::ERRNO;
use alloc::sync::Arc;


const DEADLOCK_DETECTED: isize = -0xDEAD;
const FUTEX_WAIT: i32 = 0;
const FUTEX_WAKE: i32 = 1;
const FUTEX_REQUEUE: i32 = 3;
const FUTEX_WAIT_BITSET: i32 = 9;
const FUTEX_WAKE_BITSET: i32 = 10;
const FUTEX_CMD_MASK: i32 = 0x7f;
const FUTEX_PRIVATE_FLAG: i32 = 128;
const FUTEX_CLOCK_REALTIME: i32 = 256;
const FUTEX_BITSET_MATCH_ANY: i32 = -1;

fn current_tid() -> usize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .tid
}


/// Minimal Linux futex syscall implementation for pthread wait/wake paths.
pub fn sys_futex(
    uaddr: *const i32,
    op: i32,
    val: i32,
    timeout: usize,
    uaddr2: usize,
    val3: i32,
) -> isize {
    trace!(
        "kernel:pid[{}] tid[{}] sys_futex",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        current_tid()
    );
    syscall_body!({
        if uaddr.is_null() || (uaddr as usize) & (core::mem::align_of::<i32>() - 1) != 0 {
            return Err(ERRNO::EINVAL);
        }
        match op & FUTEX_CMD_MASK {
            FUTEX_WAIT => {
                let flags = op & !FUTEX_CMD_MASK;
                let current = read_pod_from_user(uaddr);
                debug!(
                    "sys_futex WAIT: uaddr={:#x} expected={} current={:?} flags={:#x} timeout_ptr={:#x}",
                    uaddr as usize,
                    val,
                    current,
                    flags,
                    timeout
                );
                if flags & !(FUTEX_PRIVATE_FLAG) != 0 {
                    warn!(
                        "Unsupported futex WAIT flags: op={:#x} flags={:#x}",
                        op,
                        flags
                    );
                    return Err(ERRNO::EINVAL);
                }
                let timeout_ptr = (!timeout.eq(&0)).then_some(timeout as *const Timespec);
                let ret = futex_wait_addr(uaddr, val, timeout_ptr);
                debug!(
                    "sys_futex WAIT result: uaddr={:#x} expected={} ret={:?}",
                    uaddr as usize,
                    val,
                    ret
                );
                ret
            }
            FUTEX_WAKE => Ok(futex_wake_addr(uaddr as usize, val.max(0) as usize)),
            FUTEX_REQUEUE => {
                let flags = op & !FUTEX_CMD_MASK;
                if uaddr2 == 0 || uaddr2 & (core::mem::align_of::<i32>() - 1) != 0
                {
                    warn!(
                        "Unsupported futex REQUEUE target: op={:#x} uaddr2={:#x}",
                        op,
                        uaddr2
                    );
                    return Err(ERRNO::EINVAL);
                }
                if flags & !FUTEX_PRIVATE_FLAG != 0 {
                    warn!(
                        "Unsupported futex REQUEUE flags: op={:#x} flags={:#x}",
                        op,
                        flags
                    );
                    return Err(ERRNO::EINVAL);
                }
                let src = futex_queue(uaddr as usize);
                let dst = futex_queue(uaddr2);
                Ok(src.wake_and_requeue_with(
                    &dst,
                    val.max(0) as usize,
                    timeout,
                    futex_wait_mark_ready,
                ) as isize)
            }
            FUTEX_WAIT_BITSET => {
                let flags = op & !FUTEX_CMD_MASK;
                if val3 != FUTEX_BITSET_MATCH_ANY {
                    warn!(
                        "Unsupported futex WAIT_BITSET mask: op={:#x} bitset={:#x}",
                        op,
                        val3
                    );
                    return Err(ERRNO::EINVAL);
                }
                if timeout != 0 {
                    warn!(
                        "Unsupported futex WAIT_BITSET timeout: op={:#x} timeout={:#x}",
                        op,
                        timeout
                    );
                    return Err(ERRNO::EINVAL);
                }
                let supported_flags = FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME;
                if flags & !supported_flags != 0 {
                    warn!(
                        "Unsupported futex WAIT_BITSET flags: op={:#x} flags={:#x}",
                        op,
                        flags
                    );
                    return Err(ERRNO::EINVAL);
                }
                futex_wait_addr(uaddr, val, None)
            }
            FUTEX_WAKE_BITSET => {
                let flags = op & !FUTEX_CMD_MASK;
                if val3 != FUTEX_BITSET_MATCH_ANY {
                    warn!(
                        "Unsupported futex WAKE_BITSET mask: op={:#x} bitset={:#x}",
                        op,
                        val3
                    );
                    return Err(ERRNO::EINVAL);
                }
                if flags & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME) != 0 {
                    warn!(
                        "Unsupported futex WAKE_BITSET flags: op={:#x} flags={:#x}",
                        op,
                        flags
                    );
                    return Err(ERRNO::EINVAL);
                }
                Ok(futex_wake_addr(uaddr as usize, val.max(0) as usize))
            }
            _ => {
                warn!(
                    "Unsupported futex op: raw={:#x} cmd={} flags={:#x}",
                    op,
                    op & FUTEX_CMD_MASK,
                    op & !FUTEX_CMD_MASK
                );
                Err(ERRNO::EINVAL)
            },
        }
    })
}

/// Linux-compatible relative `nanosleep(2)` syscall.
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

    syscall_body!({
        let _ = rem;
        let timespec = read_pod_from_user(req)?;
        let current_time = get_time_ns();
        let sleep_ns = (timespec.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(timespec.tv_nsec as u64);
        let expire_ns = current_time.saturating_add(sleep_ns.max(1));
        debug!(
            "nanosleep: current_time_ns = {}, expire_time_ns = {}",
            current_time,
            expire_ns,
        );
        let task = current_task().unwrap();
        add_timer_ns(expire_ns, task);
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
