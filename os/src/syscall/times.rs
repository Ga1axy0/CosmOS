use crate::syscall::errno::ERRNO;
use crate::syscall_body;
use crate::syscall::{read_pod_from_user, write_pod_to_user, Pod};
use crate::timer::set_realtime_offset_from_time_ns;
use crate::{
    config::CLOCK_FREQ,
    sched::block_current_and_run_next,
    task::{current_process, current_task, TaskStatus, WaitReason},
    timer::{
        add_timer_ns, get_realtime_ns, get_time, get_time_ns, get_time_ticks, time_to_ticks,
        TICKS_PER_SEC,
    },
};

/// Linux 兼容的 `clockid_t` 类型。
pub type ClockId = i32;

/// Linux 兼容的实时时钟 ID。
pub const CLOCK_REALTIME: ClockId = 0;
/// Linux 兼容的单调时钟 ID。
pub const CLOCK_MONOTONIC: ClockId = 1;
/// Linux compatible `CLOCK_MONOTONIC_RAW`.
pub const CLOCK_MONOTONIC_RAW: ClockId = 4;
/// Linux 兼容的 `CLOCK_REALTIME_COARSE`。
pub const CLOCK_REALTIME_COARSE: ClockId = 5;
/// Linux 兼容的 `CLOCK_MONOTONIC_COARSE`。
pub const CLOCK_MONOTONIC_COARSE: ClockId = 6;
/// `clock_nanosleep(2)` absolute-deadline flag.
pub const TIMER_ABSTIME: i32 = 1;

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

const ADJ_OFFSET: u32 = 0x0001;
const ADJ_FREQUENCY: u32 = 0x0002;
const ADJ_MAXERROR: u32 = 0x0004;
const ADJ_ESTERROR: u32 = 0x0008;
const ADJ_STATUS: u32 = 0x0010;
const ADJ_TIMECONST: u32 = 0x0020;
const ADJ_TAI: u32 = 0x0080;
const ADJ_SETOFFSET: u32 = 0x0100;
const ADJ_MICRO: u32 = 0x1000;
const ADJ_NANO: u32 = 0x2000;
const ADJ_TICK: u32 = 0x4000;
const ADJ_OFFSET_SINGLESHOT: u32 = 0x8001;
const ADJ_OFFSET_SS_READ: u32 = 0xa001;

const TIME_OK: isize = 0;

const ADJTIMEX_ALLOWED_MODES: u32 = ADJ_OFFSET
    | ADJ_FREQUENCY
    | ADJ_MAXERROR
    | ADJ_ESTERROR
    | ADJ_STATUS
    | ADJ_TIMECONST
    | ADJ_TAI
    | ADJ_SETOFFSET
    | ADJ_MICRO
    | ADJ_NANO
    | ADJ_TICK;

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
    /// second
    pub tv_sec: usize,
    /// nanosecond
    pub tv_nsec: usize,
}

impl Pod for Timespec {}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct TimexTimeVal {
    pub tv_sec: i64,
    pub tv_usec: i64,
}

impl Pod for TimexTimeVal {}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Timex {
    pub modes: u32,
    pub offset: i64,
    pub freq: i64,
    pub maxerror: i64,
    pub esterror: i64,
    pub status: i32,
    pub constant: i64,
    pub precision: i64,
    pub tolerance: i64,
    pub time: TimexTimeVal,
    pub tick: i64,
    pub ppsfreq: i64,
    pub jitter: i64,
    pub shift: i32,
    pub stabil: i64,
    pub jitcnt: i64,
    pub calcnt: i64,
    pub errcnt: i64,
    pub stbcnt: i64,
    pub tai: i32,
    pub reserved: [i32; 11],
}

impl Pod for Timex {}

/// 32-bit timespec used by legacy *_time32 syscalls (tv_sec/tv_nsec are signed 32-bit)
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct OldTimespec32 {
    pub tv_sec: i32,
    pub tv_nsec: i32,
}

impl Pod for OldTimespec32 {}

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
        CLOCK_REALTIME
        | CLOCK_MONOTONIC
        | CLOCK_MONOTONIC_RAW
        | CLOCK_REALTIME_COARSE
        | CLOCK_MONOTONIC_COARSE => {
            // Expose the timer ABI as high-resolution so Linux RT userland
            // enables hrtimer paths such as cyclictest.
            Ok(Timespec {
                tv_sec: 0,
                tv_nsec: 1,
            })
        }
        // TODO：后续按 Linux 语义继续补充其它 clock id。
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

/// 将 `timespec` 转为纳秒时间长度。
fn timespec_to_ns(ts: &Timespec) -> Result<u64, ERRNO> {
    if ts.tv_nsec >= 1_000_000_000 {
        return Err(ERRNO::EINVAL);
    }
    let sec_ns = (ts.tv_sec as u128) * 1_000_000_000u128;
    let nsec = ts.tv_nsec as u128;
    let total = sec_ns.saturating_add(nsec);
    if total > u64::MAX as u128 {
        return Err(ERRNO::EINVAL);
    }
    Ok(total as u64)
}

fn adjtimex_modes_are_valid(modes: u32) -> bool {
    matches!(modes, ADJ_OFFSET_SINGLESHOT | ADJ_OFFSET_SS_READ)
        || (modes & !ADJTIMEX_ALLOWED_MODES) == 0
}

fn adjtimex_tick_bounds() -> (i64, i64) {
    (
        (900_000usize / TICKS_PER_SEC) as i64,
        (1_100_000usize / TICKS_PER_SEC) as i64,
    )
}

fn current_adjtimex_snapshot() -> Timex {
    let realtime_us = get_realtime_ns() / 1_000;
    Timex {
        precision: 1,
        time: TimexTimeVal {
            tv_sec: (realtime_us / 1_000_000) as i64,
            tv_usec: (realtime_us % 1_000_000) as i64,
        },
        tick: (1_000_000usize / TICKS_PER_SEC) as i64,
        ..Default::default()
    }
}

fn do_adjtimex(buf: *mut Timex) -> Result<isize, ERRNO> {
    let timex = read_pod_from_user(buf)?;
    if !adjtimex_modes_are_valid(timex.modes) {
        return Err(ERRNO::EINVAL);
    }

    // 与 Linux 一致地允许 modes=0 的“只读查询”给非特权进程使用。
    if timex.modes != 0 && timex.modes != ADJ_OFFSET_SS_READ && current_process().geteuid() != 0 {
        return Err(ERRNO::EPERM);
    }

    if timex.modes & ADJ_TICK != 0 {
        let (tick_min, tick_max) = adjtimex_tick_bounds();
        if timex.tick < tick_min || timex.tick > tick_max {
            return Err(ERRNO::EINVAL);
        }
    }

    let snapshot = current_adjtimex_snapshot();
    write_pod_to_user(buf, &snapshot)?;
    Ok(TIME_OK)
}

/// get_time syscall
pub fn sys_get_time_of_day(ts: *mut TimeVal, _tz: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_get_time",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let time_us = (get_realtime_ns() / 1_000) as usize;
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

/// `adjtimex(2)` 系统调用。
pub fn sys_adjtimex(buf: *mut Timex) -> isize {
    trace!(
        "kernel:pid[{}] sys_adjtimex",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({ do_adjtimex(buf) })
}

/// `clock_adjtime(2)` 系统调用。
pub fn sys_clock_adjtime(clockid: ClockId, buf: *mut Timex) -> isize {
    trace!(
        "kernel:pid[{}] sys_clock_adjtime clockid={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        clockid
    );
    syscall_body!({
        if clockid != CLOCK_REALTIME {
            return Err(ERRNO::EINVAL);
        }
        do_adjtimex(buf)
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
            CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW => timespec_from_ns(get_time_ns()),
            CLOCK_REALTIME_COARSE => timespec_from_ns(get_realtime_ns()),
            CLOCK_MONOTONIC_COARSE => timespec_from_ns(get_time_ns()),
            // TODO：后续按 Linux 语义继续补充其它 clock id。
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

/// `clock_nanosleep(2)` 系统调用。
pub fn sys_clock_nanosleep(
    clockid: ClockId,
    flags: i32,
    req: *const Timespec,
    rem: *mut Timespec,
) -> isize {
    trace!(
        "kernel:pid[{}] sys_clock_nanosleep clockid={} flags={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        clockid,
        flags
    );
    syscall_body!({
        if req.is_null() {
            return Err(ERRNO::EFAULT);
        }
        if flags & !TIMER_ABSTIME != 0 {
            return Err(ERRNO::EINVAL);
        }
        match clockid {
            CLOCK_REALTIME | CLOCK_MONOTONIC => {}
            _ => return Err(ERRNO::EINVAL),
        }

        let req = read_pod_from_user(req)?;
        let req_ns = timespec_to_ns(&req)?;
        // 单调时钟现值：定时器队列（`add_timer_ns` / `check_timer`）只按
        // CLOCK_MONOTONIC 的 `get_time_ns()` 判定到期，因此最终的到期时刻必须
        // 落在单调时间轴上。我们这里把它取一次并复用，避免读两次时钟产生缝隙。
        let monotonic_now_ns = get_time_ns();
        let now_ns = match clockid {
            CLOCK_REALTIME => get_realtime_ns(),
            CLOCK_MONOTONIC => monotonic_now_ns,
            _ => unreachable!(),
        };
        // 先把请求归一化成“还需睡多久”的相对时长：绝对超时按各自时钟基准做差，
        // 相对超时直接采用。这样无论传入的是 realtime 还是 monotonic，得到的都是
        // 一个与时钟无关的纯时长。
        let sleep_ns = if flags & TIMER_ABSTIME != 0 {
            req_ns.saturating_sub(now_ns)
        } else {
            req_ns
        };

        if sleep_ns == 0 {
            if !rem.is_null() {
                write_pod_to_user(rem, &Timespec { tv_sec: 0, tv_nsec: 0 })?;
            }
            return Ok(0);
        }

        // 关键修复：到期时刻一律换算到单调时间轴。此前对 CLOCK_REALTIME 用
        // `now_ns`(=单调+RTC墙钟偏移) 作为基准，得到的 `expire_ns` 大约领先单调时钟
        // 数十年，定时器永远不会触发——glibc 的 `usleep` 走
        // `clock_nanosleep(CLOCK_REALTIME, 0, …)`，于是阻塞至天荒地老（musl 的
        // `usleep` 走 `nanosleep` 用单调时钟，故不受影响）。
        let expire_ns = monotonic_now_ns.saturating_add(sleep_ns.max(1));
        let task = current_task().unwrap();
        // Publish the sleep state before arming the timer so an immediate
        // expiry cannot drop the timer while this task is still marked Running.
        {
            let mut task_inner = task.inner_exclusive_access();
            task_inner.task_status = TaskStatus::Interruptible;
            task_inner.wait_reason = Some(WaitReason::Nanosleep);
        }
        add_timer_ns(expire_ns, task);
        block_current_and_run_next(WaitReason::Nanosleep);

        if !rem.is_null() {
            write_pod_to_user(rem, &Timespec { tv_sec: 0, tv_nsec: 0 })?;
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
        // TODO：后续按 Linux 语义继续补充其它 clock id 的设置。
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
