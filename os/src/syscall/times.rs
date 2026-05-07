use crate::mm::translated_ref;
use crate::syscall::errno::{ERRNO, OrErrno};
use crate::syscall_body;
use crate::syscall::{write_pod_to_user, Pod};
use crate::task::current_user_token;
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

#[repr(C)]
#[derive(Debug, Default)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

impl Pod for TimeVal {}

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
        let timeval = translated_ref(current_user_token(), tv).or_errno(ERRNO::EFAULT)?;
        let time_us = timeval.sec * 1_000_000 + timeval.usec;
        set_realtime_offset_from_time_ns((time_us * 1_000) as u64);
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
                let timespec = translated_ref(current_user_token(), _tp).or_errno(ERRNO::EFAULT)?;
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
