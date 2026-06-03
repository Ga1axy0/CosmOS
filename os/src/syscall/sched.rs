use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall::utils::read_bytes_from_user;
use crate::syscall::{read_pod_from_user, write_bytes_to_user, write_pod_to_user, Pod};
use crate::syscall_body;
use crate::{
    config::MAX_HARTS,
    hart::hartid,
    mm::{online_mask as online_hart_mask, translated_byte_buffer, translated_ref},
    sched::{
        enqueue_task_on, has_runnable_task_at_or_above, nice_to_weight, pid2process, remove_task,
        request_current_task_resched, resched_hart, suspend_current_and_run_next,
        yield_current_and_run_next, MAX_NICE, MIN_NICE, NICE_0_LOAD,
    },
    task::{
        current_process, current_task, current_user_token, ReschedReason, SchedPolicy,
        SCHED_RT_PRIO_MAX, SCHED_RT_PRIO_MIN,
    },
};

use alloc::{sync::Arc, vec::Vec};
use core::mem::{size_of, MaybeUninit};
use core::slice;

const SCHED_RR: i32 = SchedPolicy::Rr as i32;
const SCHED_FIFO: i32 = SchedPolicy::Fifo as i32;
const SCHED_OTHER: i32 = SchedPolicy::Other as i32;
const SCHED_DEADLINE: i32 = 6;
const PRIO_PROCESS: i32 = 0;
const SCHED_ATTR_SIZE_VER0: usize = 48;
const SCHED_ATTR_SIZE_VER1: usize = 56;

#[repr(C)]
pub struct SchedParam {
    pub sched_priority: i32,
}

impl Pod for SchedParam {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxSchedAttr {
    pub size: u32,
    pub sched_policy: u32,
    pub sched_flags: u64,
    pub sched_nice: i32,
    pub sched_priority: u32,
    pub sched_runtime: u64,
    pub sched_deadline: u64,
    pub sched_period: u64,
    pub sched_util_min: u32,
    pub sched_util_max: u32,
}

impl Pod for LinuxSchedAttr {}

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

fn write_kernel_sched_attr_size(ptr: *mut LinuxSchedAttr) {
    let kernel_size = size_of::<LinuxSchedAttr>() as u32;
    let _ = write_pod_to_user(ptr as *mut u32, &kernel_size);
}

fn read_linux_sched_attr(ptr: *const LinuxSchedAttr) -> Result<LinuxSchedAttr, ERRNO> {
    if ptr.is_null() {
        return Err(ERRNO::EFAULT);
    }
    let user_size = read_pod_from_user(ptr as *const u32)? as usize;
    if user_size < SCHED_ATTR_SIZE_VER0 {
        return Err(ERRNO::EINVAL);
    }
    if user_size > 4096 {
        write_kernel_sched_attr_size(ptr as *mut LinuxSchedAttr);
        return Err(ERRNO::E2BIG);
    }

    let kernel_size = size_of::<LinuxSchedAttr>();
    if user_size > kernel_size {
        let extra = read_bytes_from_user(
            (ptr as *const u8).wrapping_add(kernel_size),
            user_size - kernel_size,
        )?;
        if extra.iter().any(|&byte| byte != 0) {
            write_kernel_sched_attr_size(ptr as *mut LinuxSchedAttr);
            return Err(ERRNO::E2BIG);
        }
    }

    let copy_len = user_size.min(kernel_size);
    let bytes = read_bytes_from_user(ptr as *const u8, copy_len)?;
    let mut value = MaybeUninit::<LinuxSchedAttr>::zeroed();
    let value_bytes =
        unsafe { slice::from_raw_parts_mut(value.as_mut_ptr() as *mut u8, kernel_size) };
    value_bytes[..copy_len].copy_from_slice(&bytes);
    Ok(unsafe { value.assume_init() })
}

fn write_linux_sched_attr(
    ptr: *mut LinuxSchedAttr,
    user_size: usize,
    value: &LinuxSchedAttr,
) -> Result<(), ERRNO> {
    if ptr.is_null() {
        return Err(ERRNO::EFAULT);
    }
    if user_size < SCHED_ATTR_SIZE_VER0 {
        return Err(ERRNO::EINVAL);
    }
    let value_bytes =
        unsafe { slice::from_raw_parts(value as *const LinuxSchedAttr as *const u8, size_of::<LinuxSchedAttr>()) };
    write_bytes_to_user(ptr as *mut u8, &value_bytes[..user_size.min(value_bytes.len())])
}

fn validate_sched_attr(attr: &LinuxSchedAttr) -> Result<(SchedPolicy, u8, i32, u64), ERRNO> {
    if attr.sched_flags != 0 {
        return Err(ERRNO::EINVAL);
    }
    match attr.sched_policy as i32 {
        SCHED_OTHER => {
            if attr.sched_priority != 0 || attr.sched_nice < MIN_NICE || attr.sched_nice > MAX_NICE {
                return Err(ERRNO::EINVAL);
            }
            Ok((SchedPolicy::Other, 0, attr.sched_nice, nice_to_weight(attr.sched_nice)))
        }
        SCHED_FIFO | SCHED_RR => {
            let priority = attr.sched_priority as i32;
            if priority < SCHED_RT_PRIO_MIN as i32 || priority > SCHED_RT_PRIO_MAX as i32 {
                return Err(ERRNO::EINVAL);
            }
            let policy = if attr.sched_policy as i32 == SCHED_FIFO {
                SchedPolicy::Fifo
            } else {
                SchedPolicy::Rr
            };
            Ok((policy, priority as u8, 0, NICE_0_LOAD))
        }
        SCHED_DEADLINE => {
            if attr.sched_priority != 0
                || attr.sched_runtime == 0
                || attr.sched_deadline == 0
                || attr.sched_period == 0
                || attr.sched_runtime > attr.sched_deadline
                || attr.sched_deadline > attr.sched_period
            {
                return Err(ERRNO::EINVAL);
            }
            // Until a real EDF/CBS scheduler exists, keep execution under the
            // fair class while preserving Linux-visible deadline attributes.
            Ok((SchedPolicy::Other, 0, 0, NICE_0_LOAD))
        }
        _ => Err(ERRNO::EINVAL),
    }
}

fn apply_sched_attr_to_task(
    task: Arc<crate::task::TaskControlBlock>,
    linux_attr: &LinuxSchedAttr,
    is_current: bool,
) -> Result<(), ERRNO> {
    let (new_policy, new_priority, new_nice, new_weight) = validate_sched_attr(linux_attr)?;
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
    let enqueue_at_head =
        old_policy.is_rt() && new_policy.is_rt() && new_priority < old_priority;
    if was_on_rq {
        remove_task(task.clone());
    }
    {
        let mut task_inner = task.inner_exclusive_access();
        task_inner.sched.policy = new_policy;
        task_inner.sched.linux_policy = linux_attr.sched_policy as i32;
        task_inner.sched.rt_priority = new_priority;
        task_inner.sched.nice = new_nice;
        task_inner.sched.weight = new_weight;
        task_inner.sched.sched_flags = linux_attr.sched_flags;
        task_inner.sched.sched_runtime = linux_attr.sched_runtime;
        task_inner.sched.sched_deadline = linux_attr.sched_deadline;
        task_inner.sched.sched_period = linux_attr.sched_period;
        task_inner.sched.sched_util_min = linux_attr.sched_util_min;
        task_inner.sched.sched_util_max = linux_attr.sched_util_max;
        match new_policy {
            SchedPolicy::Rr => {
                task_inner.reset_time_slice();
                task_inner.sched.cfs_rq_key = None;
                task_inner.sched.rt_enqueue_head = enqueue_at_head;
            }
            SchedPolicy::Fifo => {
                task_inner.sched.cfs_rq_key = None;
                task_inner.sched.rt_enqueue_head = enqueue_at_head;
            }
            SchedPolicy::Other => {
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
        resched_task_if_running(&task, is_current);
    }
    Ok(())
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
                    task_inner.sched.linux_policy = SCHED_RR;
                    task_inner.sched.rt_priority = new_priority;
                    task_inner.reset_time_slice();
                    task_inner.sched.cfs_rq_key = None;
                    task_inner.sched.rt_enqueue_head = enqueue_at_head;
                }
                SchedPolicy::Fifo => {
                    task_inner.sched.policy = SchedPolicy::Fifo;
                    task_inner.sched.linux_policy = SCHED_FIFO;
                    task_inner.sched.rt_priority = new_priority;
                    task_inner.sched.cfs_rq_key = None;
                    task_inner.sched.rt_enqueue_head = enqueue_at_head;
                }
                SchedPolicy::Other => {
                    task_inner.sched.policy = SchedPolicy::Other;
                    task_inner.sched.linux_policy = SCHED_OTHER;
                    task_inner.sched.rt_priority = 0;
                    task_inner.sched.cfs_initialized = false;
                    task_inner.sched.exec_start_ns = 0;
                    task_inner.sched.cfs_slice_start_ns = 0;
                    task_inner.sched.rt_enqueue_head = false;
                }
                SchedPolicy::Idle => unreachable!(),
            }
            task_inner.sched.sched_flags = 0;
            task_inner.sched.sched_runtime = 0;
            task_inner.sched.sched_deadline = 0;
            task_inner.sched.sched_period = 0;
            task_inner.sched.sched_util_min = 0;
            task_inner.sched.sched_util_max = 0;
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
        Ok(task_inner.sched.linux_policy as isize)
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

pub fn sys_sched_setattr(
    pid: isize,
    attr: *const LinuxSchedAttr,
    flags: u32,
) -> isize {
    syscall_body!({
        if pid < 0 || flags != 0 {
            return Err(ERRNO::EINVAL);
        }
        let attr = read_linux_sched_attr(attr)?;
        let task = if pid == 0 {
            current_task().unwrap()
        } else {
            task_by_pid_or_local_tid(pid as usize).ok_or(ERRNO::ESRCH)?
        };
        apply_sched_attr_to_task(task, &attr, pid == 0)?;
        Ok(0)
    })
}

pub fn sys_sched_getattr(
    pid: isize,
    attr: *mut LinuxSchedAttr,
    size: u32,
    flags: u32,
) -> isize {
    syscall_body!({
        if pid < 0 || flags != 0 {
            return Err(ERRNO::EINVAL);
        }
        let user_size = size as usize;
        if user_size < SCHED_ATTR_SIZE_VER0 {
            return Err(ERRNO::EINVAL);
        }
        let task = if pid == 0 {
            current_task().unwrap()
        } else {
            task_by_pid_or_local_tid(pid as usize).ok_or(ERRNO::ESRCH)?
        };
        let value = {
            let task_inner = task.inner_exclusive_access();
            let sched = &task_inner.sched;
            let linux_policy = sched.linux_policy;
            let (sched_nice, sched_priority) = match linux_policy {
                SCHED_FIFO | SCHED_RR => (0, sched.rt_priority as u32),
                SCHED_DEADLINE => (0, 0),
                _ => (sched.nice, 0),
            };
            LinuxSchedAttr {
                size: SCHED_ATTR_SIZE_VER1 as u32,
                sched_policy: linux_policy as u32,
                sched_flags: sched.sched_flags,
                sched_nice,
                sched_priority,
                sched_runtime: sched.sched_runtime,
                sched_deadline: sched.sched_deadline,
                sched_period: sched.sched_period,
                sched_util_min: sched.sched_util_min,
                sched_util_max: sched.sched_util_max,
            }
        };
        write_linux_sched_attr(attr, user_size, &value)?;
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
