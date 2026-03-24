use crate::syscall::errno::ERRNO;
use crate::syscall_body;
use crate::syscall::{write_pod_to_user, Pod};
use crate::{
    task::{current_process, current_task},
    timer::{get_realtime_ns, get_time, get_time_ns, get_time_ticks, get_time_us, time_to_ticks},
};

/// Linux 兼容的 `clockid_t` 类型。
pub type ClockId = i32;

/// Linux 兼容的实时时钟 ID。
pub const CLOCK_REALTIME: ClockId = 0;
/// Linux 兼容的单调时钟 ID。
pub const CLOCK_MONOTONIC: ClockId = 1;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

impl Pod for TimeVal {}

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

/// get_time syscall
pub fn sys_get_time(_ts: *mut TimeVal, _tz: usize) -> isize {
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
        write_pod_to_user(_ts, &timeval)?;
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
        write_pod_to_user(tp, &timespec)?;
        Ok(0)
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
