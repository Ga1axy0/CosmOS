use crate::syscall::errno::ERRNO;
use crate::syscall_body;
use crate::syscall::{read_pod_from_user, write_pod_to_user, Pod};
use crate::timer::set_realtime_offset_from_time_ns;
use crate::{
    config::CLOCK_FREQ,
    task::{current_process, current_task},
    timer::{get_realtime_ns, get_time, get_time_ns, get_time_ticks, get_time_us, time_to_ticks},
};

/// Linux 兼容的 `clockid_t` 类型。
pub type ClockId = i32;

/// Linux 兼容的实时时钟 ID。
pub const CLOCK_REALTIME: ClockId = 0;
/// Linux 兼容的单调时钟 ID。
pub const CLOCK_MONOTONIC: ClockId = 1;

/// Linux `getrusage(2)` 的当前进程选择器。
pub const RUSAGE_SELF: i32 = 0;
/// Linux `getrusage(2)` 的子进程选择器。
pub const RUSAGE_CHILDREN: i32 = -1;
/// Linux `getrusage(2)` 的当前线程选择器。
pub const RUSAGE_THREAD: i32 = 1;

/// Linux `getitimer(2)/setitimer(2)` 的 `which`：实时时钟。
pub const ITIMER_REAL: i32 = 0;
/// Linux `getitimer(2)/setitimer(2)` 的 `which`：用户态 CPU 时间。
pub const ITIMER_VIRTUAL: i32 = 1;
/// Linux `getitimer(2)/setitimer(2)` 的 `which`：用户态 + 内核态 CPU 时间。
pub const ITIMER_PROF: i32 = 2;

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

impl Pod for TimeVal {}

/// Linux 兼容的旧版 `itimerval` 结构。
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct OldItimerval {
    pub it_interval: TimeVal,
    pub it_value: TimeVal,
}

impl Pod for OldItimerval {}

/// Linux 风格的 `rusage` 结构。
#[repr(C)]
#[derive(Debug, Default)]
pub struct RUsage {
    pub ru_utime: TimeVal,
    pub ru_stime: TimeVal,
    pub ru_maxrss: isize,
    pub ru_ixrss: isize,
    pub ru_idrss: isize,
    pub ru_isrss: isize,
    pub ru_minflt: isize,
    pub ru_majflt: isize,
    pub ru_nswap: isize,
    pub ru_inblock: isize,
    pub ru_oublock: isize,
    pub ru_msgsnd: isize,
    pub ru_msgrcv: isize,
    pub ru_nsignals: isize,
    pub ru_nvcsw: isize,
    pub ru_nivcsw: isize,
}

impl Pod for RUsage {}

/// Linux 风格的 `timespec` 结构。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Timespec {
    pub tv_sec: usize,
    pub tv_nsec: usize,
}

impl Pod for Timespec {}

#[repr(C)]
pub struct Tms {
    pub tms_utime: usize,
    pub tms_stime: usize,
    pub tms_cutime: usize,
    pub tms_cstime: usize,
}

impl Pod for Tms {}

/// 将纳秒时间戳拆分为 `timespec`。
fn timespec_from_ns(time_ns: u64) -> Timespec {
    Timespec {
        tv_sec: (time_ns / 1_000_000_000) as usize,
        tv_nsec: (time_ns % 1_000_000_000) as usize,
    }
}

/// 将内核 CPU 账户的原始时间计数转换为 `timeval`。
fn timeval_from_raw_time(raw_time: usize) -> TimeVal {
    let raw_time = raw_time as u128;
    let freq = CLOCK_FREQ as u128;
    TimeVal {
        sec: (raw_time / freq) as usize,
        usec: ((raw_time % freq) * 1_000_000 / freq) as usize,  
    }
}
/// 返回当前内核可提供的时钟分辨率。
fn clock_resolution(clockid: ClockId) -> Result<Timespec, ERRNO> {
    match clockid {
        CLOCK_REALTIME | CLOCK_MONOTONIC => {
            let resolution_ns = 1_000_000_000u64.div_ceil(CLOCK_FREQ as u64);
            Ok(timespec_from_ns(resolution_ns))
        }
        // TODO：后续按 Linux 语义继续补充 CLOCK_MONOTONIC_RAW、
        // CLOCK_REALTIME_COARSE 等其它 clock id。
        _ => Err(ERRNO::EINVAL),
    }
}

/// 将纳秒时间长度转换为 `timeval`。
fn timeval_from_ns(ns: u64) -> TimeVal {
    TimeVal {
        sec: (ns / 1_000_000_000) as usize,
        usec: ((ns % 1_000_000_000) / 1_000) as usize,
    }
}

/// 将 `timeval` 转为纳秒时间长度。
fn timeval_to_ns(tv: &TimeVal) -> Result<u64, ERRNO> {
    if tv.usec >= 1_000_000 {
        return Err(ERRNO::EINVAL);
    }
    let sec_ns = (tv.sec as u128) * 1_000_000_000u128;
    let usec_ns = (tv.usec as u128) * 1_000u128;
    let total = sec_ns.saturating_add(usec_ns);
    if total > u64::MAX as u128 {
        return Err(ERRNO::EINVAL);
    }
    Ok(total as u64)
}

/// get_time syscall
pub fn sys_get_time_of_day(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_get_time",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let time_us = get_time_us();
        let timeval = TimeVal {
            sec: time_us / 1_000_000,
            usec: time_us % 1_000_000,
        };
        write_pod_to_user(ts, &timeval)?;
        Ok(0)
    })
}

pub fn sys_set_time_of_day(tv: *const TimeVal, _tz: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_set_time_of_day",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let timeval = read_pod_from_user(tv)?;
        let time_us = timeval.sec * 1_000_000 + timeval.usec;
        set_realtime_offset_from_time_ns((time_us * 1_000) as u64);
        Ok(0)
    })
}

/// `getitimer(2)` 系统调用。
pub fn sys_getitimer(which: i32, value: *mut OldItimerval) -> isize {
    trace!(
        "kernel:pid[{}] sys_getitimer which={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        which
    );
    syscall_body!({
        match which {
            ITIMER_REAL | ITIMER_VIRTUAL | ITIMER_PROF => {}
            _ => return Err(ERRNO::EINVAL),
        }
        let process = current_process();
        let now_raw = get_time();
        let now_realtime_ns = get_realtime_ns();
        let (value_ns, interval_ns) = process.get_itimer_state(which, now_raw, now_realtime_ns)?;
        let out = OldItimerval {
            it_interval: timeval_from_ns(interval_ns),
            it_value: timeval_from_ns(value_ns),
        };
        write_pod_to_user(value, &out)?;
        Ok(0)
    })
}

/// `setitimer(2)` 系统调用。
pub fn sys_setitimer(
    which: i32,
    value: *const OldItimerval,
    ovalue: *mut OldItimerval,
) -> isize {
    trace!(
        "kernel:pid[{}] sys_setitimer which={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        which
    );
    syscall_body!({
        match which {
            ITIMER_REAL | ITIMER_VIRTUAL | ITIMER_PROF => {}
            _ => return Err(ERRNO::EINVAL),
        }

        let new_value = if value.is_null() {
            None
        } else {
            let new_timer = read_pod_from_user(value)?;
            let value_ns = timeval_to_ns(&new_timer.it_value)?;
            let interval_ns = timeval_to_ns(&new_timer.it_interval)?;
            Some((value_ns, interval_ns))
        };

        let process = current_process();
        let now_raw = get_time();
        let now_realtime_ns = get_realtime_ns();
        let (old_value_ns, old_interval_ns) =
            process.set_itimer_state(which, now_raw, now_realtime_ns, new_value)?;

        if !ovalue.is_null() {
            let old = OldItimerval {
                it_interval: timeval_from_ns(old_interval_ns),
                it_value: timeval_from_ns(old_value_ns),
            };
            write_pod_to_user(ovalue, &old)?;
        }
        Ok(0)
    })
}

/// `clock_gettime(2)` 系统调用。
pub fn sys_clock_gettime(clockid: ClockId, tp: *mut Timespec) -> isize {
    trace!(
        "kernel:pid[{}] sys_clock_gettime clockid={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        clockid
    );
    syscall_body!({
        let timespec = match clockid {
            CLOCK_REALTIME => timespec_from_ns(get_realtime_ns()),
            CLOCK_MONOTONIC => timespec_from_ns(get_time_ns()),
            // TODO：后续按 Linux 语义继续补充 CLOCK_MONOTONIC_RAW、
            // CLOCK_REALTIME_COARSE 等其它 clock id。
            _ => return Err(ERRNO::EINVAL),
        };
        // debug!("sys_clock_gettime: clockid={}, timespec={:?}", clockid, timespec);
        write_pod_to_user(tp, &timespec)?;
        Ok(0)
    })
}

/// `clock_getres(2)` 系统调用。
pub fn sys_clock_getres(clockid: ClockId, tp: *mut Timespec) -> isize {
    trace!(
        "kernel:pid[{}] sys_clock_getres clockid={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        clockid
    );
    syscall_body!({
        let resolution = clock_resolution(clockid)?;
        if !tp.is_null() {
            write_pod_to_user(tp, &resolution)?;
        }
        Ok(0)
    })
}

pub fn sys_clock_settime(clockid: ClockId, _tp: *const Timespec) -> isize {
    trace!(
        "kernel:pid[{}] sys_clock_settime clockid={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        clockid
    );
    syscall_body!({
        // TODO：后续按 Linux 语义继续补充 CLOCK_MONOTONIC_RAW、
        // CLOCK_REALTIME_COARSE 等其它 clock id 的设置。
        match clockid {
            CLOCK_REALTIME => {
                let timespec = read_pod_from_user(_tp)?;
                let time_ns = (timespec.tv_sec as u64) * 1_000_000_000 + (timespec.tv_nsec as u64);
                set_realtime_offset_from_time_ns(time_ns);
                Ok(0)
            }
            _ => Err(ERRNO::EINVAL),
        }
    })
}

pub fn sys_times(buf: *mut Tms) -> isize {
    trace!(
        "kernel:pid[{}] sys_times",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let process = current_process();
        let (utime, stime, cutime, cstime) = process.times_snapshot(get_time());
        let tms = Tms {
            tms_utime: time_to_ticks(utime),
            tms_stime: time_to_ticks(stime),
            tms_cutime: time_to_ticks(cutime),
            tms_cstime: time_to_ticks(cstime),
        };
        write_pod_to_user(buf, &tms)?;
        Ok(get_time_ticks() as isize)
    })
}

/// `getrusage(2)` 系统调用。
pub fn sys_getrusage(who: i32, usage: *mut RUsage) -> isize {
    trace!(
        "kernel:pid[{}] sys_getrusage who={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        who
    );
    syscall_body!({
        let process = current_process();
        let (utime_raw, stime_raw) = match who {
            RUSAGE_SELF | RUSAGE_THREAD => {
                let (utime, stime, _, _) = process.times_snapshot(get_time());
                (utime, stime)
            }
            RUSAGE_CHILDREN => {
                let (_, _, cutime, cstime) = process.times_snapshot(get_time());
                (cutime, cstime)
            }
            _ => return Err(ERRNO::EINVAL),
        };

        let rusage = RUsage {
            ru_utime: timeval_from_raw_time(utime_raw),
            ru_stime: timeval_from_raw_time(stime_raw),
            ..Default::default()
        };
        write_pod_to_user(usage, &rusage)?;
        Ok(0)
    })
}
