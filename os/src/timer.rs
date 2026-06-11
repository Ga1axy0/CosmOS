//! RISC-V timer-related functionality

use core::cmp::Ordering;
use core::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};

use crate::config::CLOCK_FREQ;
use crate::config::MAX_HARTS;
use crate::hal::hartid;
use crate::hal::Plat;
use crate::hal::traits::Timer as _;
use crate::platform::rtc;
use crate::poll::{self, PollTimerTag};
use crate::net::{handle_socket_wait_timeout, SocketTimerTag};
use crate::signal::{handle_signal_wait_timeout, SignalTimerTag};
use crate::sync::{FutexTimerTag, SpinNoIrqLock, handle_futex_wait_timeout};
use crate::task::{current_task, wakeup_task, TaskControlBlock};
use alloc::collections::BinaryHeap;
use alloc::sync::Arc;
use core::array;
use lazy_static::*;
/// The number of ticks per second
pub const TICKS_PER_SEC: usize = 100;
/// The number of milliseconds per second
const MSEC_PER_SEC: usize = 1000;
/// The number of microseconds per second
#[allow(dead_code)]
const MICRO_PER_SEC: usize = 1_000_000;
/// 每秒对应的纳秒数。
const NSEC_PER_SEC: u64 = 1_000_000_000;
/// Periodic scheduler/accounting tick interval expressed in raw timer ticks.
const PERIODIC_TICK_INTERVAL: u64 = (CLOCK_FREQ / TICKS_PER_SEC) as u64;

/// `CLOCK_REALTIME` 相对单调时钟的偏移，单位为纳秒。
/// 全局唯一
static REALTIME_OFFSET_NS: AtomicI64 = AtomicI64::new(0);

/// Get the current time in ticks
pub fn get_time() -> usize {
    Plat::read_time()
}

/// Get the current time in milliseconds
pub fn get_time_ms() -> usize {
    get_time() * MSEC_PER_SEC / CLOCK_FREQ
}

/// get current time in microseconds
pub fn get_time_us() -> usize {
    get_time() * MICRO_PER_SEC / CLOCK_FREQ
}

/// 获取当前单调时间，单位为纳秒。
pub fn get_time_ns() -> u64 {
    ((get_time() as u128) * (NSEC_PER_SEC as u128) / (CLOCK_FREQ as u128)) as u64
}

/// 使用“当前单调时间 + 实时时钟偏移”得到 `CLOCK_REALTIME`，单位为纳秒。
pub fn get_realtime_ns() -> u64 {
    let monotonic_ns = get_time_ns() as i128;
    let offset_ns = REALTIME_OFFSET_NS.load(AtomicOrdering::Acquire) as i128;
    let realtime_ns = monotonic_ns + offset_ns;
    if realtime_ns <= 0 {
        // TODO：若后续支持更复杂的校时语义，需要明确处理负时间场景。
        0
    } else {
        realtime_ns as u64
    }
}

/// 根据当前单调时间重新设置 `CLOCK_REALTIME` 偏移。
pub fn set_realtime_offset_from_time_ns(realtime_ns: u64) {
    // 这里把“真实时间 = 当前单调时间 + 偏移”固化下来，后续查询无需再访问 RTC。
    let monotonic_ns = get_time_ns() as i128;
    let offset_ns = (realtime_ns as i128) - monotonic_ns;
    REALTIME_OFFSET_NS.store(offset_ns as i64, AtomicOrdering::Release);
}

/// 从 RTC 读取一次当前真实时间，并初始化内核维护的 realtime offset。
pub fn init_realtime_offset_from_rtc() {
    if rtc::rtc_ready() {
        set_realtime_offset_from_time_ns(rtc::read_time_ns());
    } else {
        // TODO：若未来支持可插拔 RTC/DTB 延迟探测，需要定义未就绪时的兜底策略。
        error!("rtc is not ready, realtime offset init failed");
    }
}

/// Get current time in clock ticks used by times(2).
pub fn get_time_ticks() -> usize {
    get_time() * TICKS_PER_SEC / CLOCK_FREQ
}

/// Convert a raw timer counter delta into clock ticks used by times(2).
pub fn time_to_ticks(time: usize) -> usize {
    time.saturating_mul(TICKS_PER_SEC) / CLOCK_FREQ
}

/// Set the next timer interrupt
pub fn set_next_trigger() {
    program_next_trigger_for_hart(hartid(), get_time());
}

/// 初始化当前 hart 的时钟中断状态。
///
/// 该函数需要每个 hart 各自执行一次：先开启 supervisor timer interrupt，
/// 再设置当前 hart 的下一次 timer 触发时间。
pub fn init_hart() {
    crate::trap::enable_timer_interrupt();
    let hart = normalize_hart(hartid());
    let now = get_time() as u64;
    *PER_CPU_NEXT_PERIODIC_TICK[hart].lock() = now.saturating_add(PERIODIC_TICK_INTERVAL.max(1));
    set_next_trigger();
    info!("hart {} timer init done", hartid());
}

/// condvar for timer
pub struct TimerCondVar {
    /// The absolute monotonic deadline when the timer expires, in nanoseconds.
    pub expire_ns: u64,
    /// The task to be woken up when the timer expires
    pub task: Arc<TaskControlBlock>,
    /// Optional timeout identity for specialized timer wakeup paths.
    pub(crate) timer_tag: Option<TimerTagKind>,
}

/// Specialized timeout identity stored in timer heap entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimerTagKind {
    Poll(PollTimerTag),
    Signal(SignalTimerTag),
    Futex(FutexTimerTag),
    Socket(SocketTimerTag),
}

impl PartialEq for TimerCondVar {
    fn eq(&self, other: &Self) -> bool {
        self.expire_ns == other.expire_ns
    }
}
impl Eq for TimerCondVar {}
impl PartialOrd for TimerCondVar {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(other.expire_ns.cmp(&self.expire_ns))
    }
}

impl Ord for TimerCondVar {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap()
    }
}

lazy_static! {
    /// Per-hart timer bases. Each hart scans only its own heap on timer tick.
    static ref PER_CPU_TIMERS: [SpinNoIrqLock<BinaryHeap<TimerCondVar>>; MAX_HARTS] =
        array::from_fn(|_| SpinNoIrqLock::new(BinaryHeap::<TimerCondVar>::new()));
    /// Per-hart next periodic scheduler/accounting tick, in raw timer ticks.
    static ref PER_CPU_NEXT_PERIODIC_TICK: [SpinNoIrqLock<u64>; MAX_HARTS] =
        array::from_fn(|_| SpinNoIrqLock::new(0));
}

fn normalize_hart(hart: usize) -> usize {
    hart.min(MAX_HARTS.saturating_sub(1))
}

fn timer_hart_for_task(task: &Arc<TaskControlBlock>) -> usize {
    let task_inner = task.inner_exclusive_access();
    normalize_hart(task_inner.sched.last_cpu)
}

/// Add a timer with an absolute monotonic deadline in nanoseconds.
pub fn add_timer_ns(expire_ns: u64, task: Arc<TaskControlBlock>) {
    add_timer_with_tag(expire_ns, task, None);
}

/// Add a timer with an optional poll timeout identity.
pub(crate) fn add_timer_with_poll_tag(
    expire_ns: u64,
    task: Arc<TaskControlBlock>,
    poll_tag: Option<PollTimerTag>,
) {
    add_timer_with_tag(expire_ns, task, poll_tag.map(TimerTagKind::Poll));
}

/// Add a timer with an optional sigtimedwait timeout identity.
pub(crate) fn add_timer_with_signal_tag(
    expire_ns: u64,
    task: Arc<TaskControlBlock>,
    signal_tag: Option<SignalTimerTag>,
) {
    add_timer_with_tag(expire_ns, task, signal_tag.map(TimerTagKind::Signal));
}

/// Add a timer with an optional futex wait timeout identity.
pub(crate) fn add_timer_with_futex_tag(
    expire_ns: u64,
    task: Arc<TaskControlBlock>,
    futex_tag: Option<FutexTimerTag>,
) {
    add_timer_with_tag(expire_ns, task, futex_tag.map(TimerTagKind::Futex));
}

/// Add a timer with an optional socket wait timeout identity.
pub(crate) fn add_timer_with_socket_tag(
    expire_ns: u64,
    task: Arc<TaskControlBlock>,
    socket_tag: Option<SocketTimerTag>,
) {
    add_timer_with_tag(expire_ns, task, socket_tag.map(TimerTagKind::Socket));
}

fn add_timer_with_tag(
    expire_ns: u64,
    task: Arc<TaskControlBlock>,
    timer_tag: Option<TimerTagKind>,
) {
    trace!(
        "kernel:pid[{}] add_timer",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let target_hart = timer_hart_for_task(&task);
    let mut timers = PER_CPU_TIMERS[target_hart].lock();
    timers.push(TimerCondVar {
        expire_ns,
        task,
        timer_tag,
    });
    drop(timers);
    if target_hart == normalize_hart(hartid()) {
        program_next_trigger_for_hart(target_hart, get_time());
    }
}

/// Remove a timer
pub fn remove_timer(task: Arc<TaskControlBlock>) {
    //trace!("kernel:pid[{}] remove_timer", current_task().unwrap().process.upgrade().unwrap().getpid());
    trace!("kernel: remove_timer");
    for timers in PER_CPU_TIMERS.iter() {
        let mut timers = timers.lock();
        let mut temp = BinaryHeap::<TimerCondVar>::new();
        for condvar in timers.drain() {
            if Arc::as_ptr(&task) != Arc::as_ptr(&condvar.task) {
                temp.push(condvar);
            }
        }
        timers.clear();
        timers.append(&mut temp);
    }
    trace!("kernel: remove_timer END");
}

fn next_timer_deadline_ns_for_hart(hart: usize) -> Option<u64> {
    let timers = PER_CPU_TIMERS[hart].lock();
    timers.peek().map(|timer| timer.expire_ns)
}

fn ns_to_raw_ticks_ceil(ns: u64) -> u64 {
    if ns == 0 {
        return 0;
    }
    ((ns as u128)
        .saturating_mul(CLOCK_FREQ as u128)
        .saturating_add((NSEC_PER_SEC - 1) as u128)
        / (NSEC_PER_SEC as u128)) as u64
}

fn program_next_trigger_for_hart(hart: usize, now_raw: usize) {
    let periodic_raw = *PER_CPU_NEXT_PERIODIC_TICK[hart].lock();
    let timer_raw = next_timer_deadline_ns_for_hart(hart).map(ns_to_raw_ticks_ceil);
    let next_raw = match timer_raw {
        Some(timer_raw) => periodic_raw.min(timer_raw),
        None => periodic_raw,
    };
    let now_raw = now_raw as u64;
    Plat::set_next(next_raw.max(now_raw.saturating_add(1)) as usize);
}

/// Check if the timer has expired for the current hart.
pub fn check_timer() {
    check_timer_expired(get_time_ns());
}

fn check_timer_expired(current_ns: u64) {
    let mut timers = PER_CPU_TIMERS[normalize_hart(hartid())].lock();
    while let Some(timer) = timers.peek() {
        if timer.expire_ns <= current_ns {
            if let Some(tag) = timer.timer_tag {
                match tag {
                    TimerTagKind::Poll(poll_tag) => {
                        if poll::handle_poll_timeout(poll_tag, &timer.task) {
                            timers.pop();
                        }
                    }
                    TimerTagKind::Signal(signal_tag) => {
                        if handle_signal_wait_timeout(signal_tag, &timer.task) {
                            timers.pop();
                        }
                    }
                    TimerTagKind::Futex(futex_tag) => {
                        if handle_futex_wait_timeout(futex_tag, &timer.task) {
                            timers.pop();
                        }
                    }
                    TimerTagKind::Socket(socket_tag) => {
                        if handle_socket_wait_timeout(socket_tag, &timer.task) {
                            timers.pop();
                        }
                    }
                }
                continue;
            }
            // 如果任务还没真正进入睡眠态（例如仍处于 Running），跨 hart 的提前检查可能先看到它。
            // 此时唤醒不成功，但仍然要保留 timer，等下一次 tick 再尝试。
            // 这种情形应该较少，且最多损失一个时间片，是可以接受的。
            if wakeup_task(Arc::clone(&timer.task)) {
                timers.pop();
            }
        } else {
            break;
        }
    }
}

/// Handle one supervisor timer interrupt on the current hart.
///
/// Returns whether the periodic scheduler/accounting tick fired as part of
/// this interrupt.
pub fn handle_timer_interrupt() -> bool {
    let hart = normalize_hart(hartid());
    let now_raw = get_time() as u64;
    let current_ns = get_time_ns();
    check_timer_expired(current_ns);

    let periodic_fired = {
        let mut next_periodic = PER_CPU_NEXT_PERIODIC_TICK[hart].lock();
        if now_raw < *next_periodic {
            false
        } else {
            let interval = PERIODIC_TICK_INTERVAL.max(1);
            while now_raw >= *next_periodic {
                *next_periodic = next_periodic.saturating_add(interval);
            }
            true
        }
    };

    program_next_trigger_for_hart(hart, now_raw as usize);
    periodic_fired
}
