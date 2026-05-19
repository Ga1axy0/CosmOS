use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall::{write_bytes_to_user, write_pod_to_user, Pod};
use crate::syscall_body;
use crate::{
    config::MAX_HARTS,
    hart::hartid,
    mm::{online_mask as online_hart_mask, translated_byte_buffer, translated_ref},
    sched::{
        enqueue_task_on, has_runnable_task_at_or_above, nice_to_weight, pid2process, remove_task,
        request_current_task_resched, resched_hart, suspend_current_and_run_next,
        yield_current_and_run_next, MAX_NICE, MIN_NICE,
    },
    task::{
        current_process, current_task, current_user_token, ReschedReason, SchedPolicy,
        SCHED_RT_PRIO_MAX, SCHED_RT_PRIO_MIN,
    },
};

use alloc::{sync::Arc, vec::Vec};
use core::mem::size_of;

const SCHED_RR: i32 = SchedPolicy::Rr as i32;
const SCHED_FIFO: i32 = SchedPolicy::Fifo as i32;
const SCHED_OTHER: i32 = SchedPolicy::Other as i32;
const PRIO_PROCESS: i32 = 0;

#[repr(C)]
pub struct SchedParam {
    pub sched_priority: i32,
}

impl Pod for SchedParam {}

fn task_by_pid_or_local_tid(pid: usize) -> Option<Arc<crate::task::TaskControlBlock>> {
    if let Some(process) = pid2process(pid) {
        return process
            .inner_exclusive_access()
            .tasks
            .first()
            .and_then(|task| task.as_ref())
            .cloned();
    }
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    process_inner
        .tasks
        .get(pid)
        .and_then(|task| task.as_ref())
        .cloned()
}

fn affinity_mask_bytes_len() -> usize {
    MAX_HARTS.div_ceil(8).max(1)
}

fn online_cpu_mask() -> usize {
    let online = online_hart_mask();
    if online != 0 {
        online
    } else {
        1usize << hartid().min(usize::BITS.saturating_sub(1) as usize)
    }
}

fn first_cpu_in_mask(mask: usize) -> usize {
    (mask & online_cpu_mask()).trailing_zeros() as usize
}

fn read_cpu_affinity_mask(
    token: usize,
    mask_ptr: *const u8,
    cpusetsize: usize,
) -> Result<usize, ERRNO> {
    if mask_ptr.is_null() || cpusetsize == 0 {
        return Err(ERRNO::EINVAL);
    }
    let user_bytes = translated_byte_buffer(token, mask_ptr, cpusetsize).or_errno(ERRNO::EFAULT)?;
    let mut raw_mask = 0usize;
    let max_bytes = cpusetsize.min(size_of::<usize>());
    let mut copied = 0usize;
    for chunk in user_bytes {
        for &byte in chunk.iter() {
            if copied < max_bytes {
                raw_mask |= (byte as usize) << (copied * 8);
            }
            copied += 1;
        }
    }
    let effective_mask = raw_mask & online_cpu_mask();
    if effective_mask == 0 {
        return Err(ERRNO::EINVAL);
    }
    Ok(effective_mask)
}

/// yield syscall
pub fn sys_yield() -> isize {
    let (policy, rt_priority) = {
        let task = current_task().unwrap();
        let task_inner = task.inner_exclusive_access();
        (task_inner.sched.policy, task_inner.sched.rt_priority)
    };
    match policy {
        SchedPolicy::Fifo | SchedPolicy::Rr => {
            if has_runnable_task_at_or_above(hartid(), rt_priority) {
                request_current_task_resched(ReschedReason::Yield);
                suspend_current_and_run_next();
            }
        }
        SchedPolicy::Other => yield_current_and_run_next(),
        SchedPolicy::Idle => {}
    }
    0
}

fn resched_task_if_running(task: &Arc<crate::task::TaskControlBlock>, is_current: bool) {
    let target_is_current =
        is_current || current_task().is_some_and(|current| Arc::ptr_eq(&current, task));
    let running_hart = {
        let task_inner = task.inner_exclusive_access();
        if !task_inner.sched.on_cpu {
            return;
        }
        if target_is_current {
            None
        } else {
            Some(task_inner.sched.last_cpu)
        }
    };
    if let Some(hart) = running_hart {
        resched_hart(hart);
    } else {
        request_current_task_resched(ReschedReason::Migration);
    }
}

pub fn sys_sched_setscheduler(pid: isize, policy: i32, param: *const SchedParam) -> isize {
    syscall_body!({
        if pid < 0 || param.is_null() {
            return Err(ERRNO::EINVAL);
        }
        if policy != SCHED_RR && policy != SCHED_FIFO && policy != SCHED_OTHER {
            return Err(ERRNO::EINVAL);
        }
        let token = current_user_token();
        let param = translated_ref(token, param).or_errno(ERRNO::EFAULT)?;
        let priority = param.sched_priority;
        match policy {
            SCHED_RR | SCHED_FIFO => {
                if priority < SCHED_RT_PRIO_MIN as i32 || priority > SCHED_RT_PRIO_MAX as i32 {
                    return Err(ERRNO::EINVAL);
                }
            }
            SCHED_OTHER => {
                if priority != 0 {
                    return Err(ERRNO::EINVAL);
                }
            }
            _ => unreachable!(),
        }
        let new_policy = match policy {
            SCHED_RR => SchedPolicy::Rr,
            SCHED_FIFO => SchedPolicy::Fifo,
            SCHED_OTHER => SchedPolicy::Other,
            _ => unreachable!(),
        };
        let new_priority = if new_policy.is_rt() {
            priority as u8
        } else {
            0
        };
        let task = if pid == 0 {
            current_task().unwrap()
        } else {
            task_by_pid_or_local_tid(pid as usize).ok_or(ERRNO::ESRCH)?
        };
        let (was_on_rq, was_on_cpu, last_cpu, old_policy, old_priority) = {
            let task_inner = task.inner_exclusive_access();
            (
                task_inner.sched.on_rq,
                task_inner.sched.on_cpu,
                task_inner.sched.last_cpu,
                task_inner.sched.policy,
                task_inner.sched.rt_priority,
            )
        };
        if was_on_rq && old_policy == new_policy && old_priority == new_priority {
            return Ok(0);
        }
        let enqueue_at_head =
            old_policy.is_rt() && new_policy.is_rt() && new_priority < old_priority;
        if was_on_rq {
            remove_task(task.clone());
        }
        {
            let mut task_inner = task.inner_exclusive_access();
            match new_policy {
                SchedPolicy::Rr => {
                    task_inner.sched.policy = SchedPolicy::Rr;
                    task_inner.sched.rt_priority = new_priority;
                    task_inner.reset_time_slice();
                    task_inner.sched.cfs_rq_key = None;
                    task_inner.sched.rt_enqueue_head = enqueue_at_head;
                }
                SchedPolicy::Fifo => {
                    task_inner.sched.policy = SchedPolicy::Fifo;
                    task_inner.sched.rt_priority = new_priority;
                    task_inner.sched.cfs_rq_key = None;
                    task_inner.sched.rt_enqueue_head = enqueue_at_head;
                }
                SchedPolicy::Other => {
                    task_inner.sched.policy = SchedPolicy::Other;
                    task_inner.sched.rt_priority = 0;
                    task_inner.sched.cfs_initialized = false;
                    task_inner.sched.exec_start_ns = 0;
                    task_inner.sched.cfs_slice_start_ns = 0;
                    task_inner.sched.rt_enqueue_head = false;
                }
                SchedPolicy::Idle => unreachable!(),
            }
        }
        if was_on_rq {
            enqueue_task_on(task, last_cpu);
        } else if was_on_cpu {
            if enqueue_at_head {
                task.inner_exclusive_access().sched.rt_enqueue_head = true;
            }
            resched_task_if_running(&task, pid == 0);
        }
        Ok(0)
    })
}

pub fn sys_sched_getscheduler(pid: isize) -> isize {
    syscall_body!({
        if pid < 0 {
            return Err(ERRNO::EINVAL);
        }
        let task = if pid == 0 {
            current_task().unwrap()
        } else {
            task_by_pid_or_local_tid(pid as usize).ok_or(ERRNO::ESRCH)?
        };
        let task_inner = task.inner_exclusive_access();
        Ok(task_inner.sched.policy as isize)
    })
}

pub fn sys_sched_getparam(pid: isize, param: *mut SchedParam) -> isize {
    syscall_body!({
        if pid < 0 || param.is_null() {
            return Err(ERRNO::EINVAL);
        }
        let task = if pid == 0 {
            current_task().unwrap()
        } else {
            task_by_pid_or_local_tid(pid as usize).ok_or(ERRNO::ESRCH)?
        };
        let sched_priority = {
            let task_inner = task.inner_exclusive_access();
            if task_inner.sched.policy.is_rt() {
                task_inner.sched.rt_priority as i32
            } else {
                0
            }
        };
        write_pod_to_user(param, &SchedParam { sched_priority }).or_errno(ERRNO::EFAULT)?;
        Ok(0)
    })
}

fn normalize_nice(prio: i32) -> i32 {
    prio.clamp(MIN_NICE, MAX_NICE)
}

fn tasks_for_prio_process(who: usize) -> Result<Vec<Arc<crate::task::TaskControlBlock>>, ERRNO> {
    let process = if who == 0 {
        current_process()
    } else {
        pid2process(who).ok_or(ERRNO::ESRCH)?
    };
    let process_inner = process.inner_exclusive_access();
    let tasks = process_inner
        .tasks
        .iter()
        .filter_map(|task| task.as_ref().cloned())
        .collect::<Vec<_>>();
    if tasks.is_empty() {
        Err(ERRNO::ESRCH)
    } else {
        Ok(tasks)
    }
}

/// Linux-compatible setpriority syscall for PRIO_PROCESS.
pub fn sys_setpriority(which: i32, who: usize, prio: i32) -> isize {
    syscall_body!({
        if which != PRIO_PROCESS {
            return Err(ERRNO::EINVAL);
        }
        let nice = normalize_nice(prio);
        let weight = nice_to_weight(nice);
        let tasks = tasks_for_prio_process(who)?;
        for task in tasks {
            let (was_on_rq, was_on_cpu, last_cpu) = {
                let task_inner = task.inner_exclusive_access();
                (
                    task_inner.sched.on_rq,
                    task_inner.sched.on_cpu,
                    task_inner.sched.last_cpu,
                )
            };
            if was_on_rq {
                remove_task(task.clone());
            }
            {
                let mut task_inner = task.inner_exclusive_access();
                task_inner.sched.nice = nice;
                task_inner.sched.weight = weight;
            }
            if was_on_rq {
                enqueue_task_on(task, last_cpu);
            } else if was_on_cpu {
                resched_task_if_running(&task, who == 0);
            }
        }
        Ok(0)
    })
}

/// Linux raw getpriority syscall for PRIO_PROCESS.
pub fn sys_getpriority(which: i32, who: usize) -> isize {
    syscall_body!({
        if which != PRIO_PROCESS {
            return Err(ERRNO::EINVAL);
        }
        let tasks = tasks_for_prio_process(who)?;
        let best_nice = tasks
            .iter()
            .map(|task| task.inner_exclusive_access().sched.nice)
            .min()
            .ok_or(ERRNO::ESRCH)?;
        Ok((20 - best_nice) as isize)
    })
}

pub fn sys_sched_setaffinity(pid: isize, cpusetsize: usize, mask: *const u8) -> isize {
    syscall_body!({
        if pid < 0 {
            return Err(ERRNO::EINVAL);
        }
        let token = current_user_token();
        let affinity_mask = read_cpu_affinity_mask(token, mask, cpusetsize)?;
        let task = if pid == 0 {
            current_task().unwrap()
        } else {
            task_by_pid_or_local_tid(pid as usize).ok_or(ERRNO::ESRCH)?
        };
        let (was_on_rq, was_on_cpu, current_hart, needs_migration) = {
            let task_inner = task.inner_exclusive_access();
            (
                task_inner.sched.on_rq,
                task_inner.sched.on_cpu,
                task_inner.sched.last_cpu,
                affinity_mask
                    & (1usize << task_inner.sched.last_cpu.min(MAX_HARTS.saturating_sub(1)))
                    == 0,
            )
        };
        if was_on_rq {
            remove_task(task.clone());
        }
        {
            let mut task_inner = task.inner_exclusive_access();
            task_inner.sched.cpu_affinity_mask = affinity_mask;
            if needs_migration && !was_on_cpu {
                task_inner.sched.last_cpu = first_cpu_in_mask(affinity_mask);
            }
        }
        if was_on_rq {
            let target_hart = {
                let task_inner = task.inner_exclusive_access();
                task_inner.sched.last_cpu
            };
            enqueue_task_on(task, target_hart);
        } else if needs_migration && was_on_cpu {
            if pid == 0 {
                request_current_task_resched(ReschedReason::Migration);
            } else {
                resched_hart(current_hart);
            }
        }
        Ok(0)
    })
}

pub fn sys_sched_getaffinity(pid: isize, cpusetsize: usize, mask: *mut u8) -> isize {
    syscall_body!({
        if pid < 0 || mask.is_null() {
            return Err(ERRNO::EINVAL);
        }
        let kernel_mask_size = affinity_mask_bytes_len();
        if cpusetsize < kernel_mask_size {
            return Err(ERRNO::EINVAL);
        }
        let task = if pid == 0 {
            current_task().unwrap()
        } else {
            task_by_pid_or_local_tid(pid as usize).ok_or(ERRNO::ESRCH)?
        };
        let affinity_mask =
            task.inner_exclusive_access().sched.cpu_affinity_mask & online_cpu_mask();
        let mut mask_bytes = Vec::new();
        mask_bytes.resize(cpusetsize, 0);
        for (idx, slot) in mask_bytes.iter_mut().take(kernel_mask_size).enumerate() {
            *slot = ((affinity_mask >> (idx * 8)) & 0xff) as u8;
        }
        write_bytes_to_user(mask, mask_bytes.as_slice())?;
        Ok(kernel_mask_size as isize)
    })
}
