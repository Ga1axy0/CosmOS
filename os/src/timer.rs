//! RISC-V timer-related functionality

use core::cmp::Ordering;
use core::sync::atomic::{AtomicI64, Ordering as AtomicOrdering};

use crate::config::CLOCK_FREQ;
use crate::drivers::rtc;
use crate::hart::hartid;
use crate::poll::{self, PollTimerTag};
use crate::sbi::set_timer;
use crate::sync::SpinNoIrqLock;
use crate::task::{current_task, wakeup_task, TaskControlBlock};
use alloc::collections::BinaryHeap;
use alloc::sync::Arc;
use lazy_static::*;
use riscv::register::time;
/// The number of ticks per second
pub const TICKS_PER_SEC: usize = 100;
/// The number of milliseconds per second
const MSEC_PER_SEC: usize = 1000;
/// The number of microseconds per second
#[allow(dead_code)]
const MICRO_PER_SEC: usize = 1_000_000;
/// 每秒对应的纳秒数。
const NSEC_PER_SEC: u64 = 1_000_000_000;

/// `CLOCK_REALTIME` 相对单调时钟的偏移，单位为纳秒。
/// 全局唯一
static REALTIME_OFFSET_NS: AtomicI64 = AtomicI64::new(0);

/// Get the current time in ticks
pub fn get_time() -> usize {
    time::read()
}

/// Get the current time in milliseconds
pub fn get_time_ms() -> usize {
    time::read() * MSEC_PER_SEC / CLOCK_FREQ
}

/// get current time in microseconds
pub fn get_time_us() -> usize {
    time::read() * MICRO_PER_SEC / CLOCK_FREQ
}

/// 获取当前单调时间，单位为纳秒。
pub fn get_time_ns() -> u64 {
    ((time::read() as u128) * (NSEC_PER_SEC as u128) / (CLOCK_FREQ as u128)) as u64
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
    time::read() * TICKS_PER_SEC / CLOCK_FREQ
}

/// Convert a raw timer counter delta into clock ticks used by times(2).
pub fn time_to_ticks(time: usize) -> usize {
    time.saturating_mul(TICKS_PER_SEC) / CLOCK_FREQ
}

/// Set the next timer interrupt
pub fn set_next_trigger() {
    set_timer(get_time() + CLOCK_FREQ / TICKS_PER_SEC);
}

/// 初始化当前 hart 的时钟中断状态。
///
/// 该函数需要每个 hart 各自执行一次：先开启 supervisor timer interrupt，
/// 再设置当前 hart 的下一次 timer 触发时间。
pub fn init_hart() {
    crate::trap::enable_timer_interrupt();
    set_next_trigger();
    info!("hart {} timer init done", hartid());
}

/// condvar for timer
pub struct TimerCondVar {
    /// The time when the timer expires, in milliseconds
    pub expire_ms: usize,
    /// The task to be woken up when the timer expires
    pub task: Arc<TaskControlBlock>,
    /// Optional poll timeout identity used to validate stale timer entries.
    pub(crate) poll_tag: Option<PollTimerTag>,
}

impl PartialEq for TimerCondVar {
    fn eq(&self, other: &Self) -> bool {
        self.expire_ms == other.expire_ms
    }
}
impl Eq for TimerCondVar {}
impl PartialOrd for TimerCondVar {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let a = -(self.expire_ms as isize);
        let b = -(other.expire_ms as isize);
        Some(a.cmp(&b))
    }
}

impl Ord for TimerCondVar {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap()
    }
}

lazy_static! {
    /// TIMERS: global instance: set of timer condvars
    static ref TIMERS: SpinNoIrqLock<BinaryHeap<TimerCondVar>> =
        unsafe { SpinNoIrqLock::new(BinaryHeap::<TimerCondVar>::new()) };
}

/// Add a timer
pub fn add_timer(expire_ms: usize, task: Arc<TaskControlBlock>) {
    add_timer_with_poll_tag(expire_ms, task, None);
}

/// Add a timer with an optional poll timeout identity.
pub(crate) fn add_timer_with_poll_tag(
    expire_ms: usize,
    task: Arc<TaskControlBlock>,
    poll_tag: Option<PollTimerTag>,
) {
    trace!(
        "kernel:pid[{}] add_timer",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let mut timers = TIMERS.lock();
    timers.push(TimerCondVar {
        expire_ms,
        task,
        poll_tag,
    });
}

/// Remove a timer
pub fn remove_timer(task: Arc<TaskControlBlock>) {
    //trace!("kernel:pid[{}] remove_timer", current_task().unwrap().process.upgrade().unwrap().getpid());
    trace!("kernel: remove_timer");
    let mut timers = TIMERS.lock();
    let mut temp = BinaryHeap::<TimerCondVar>::new();
    for condvar in timers.drain() {
        if Arc::as_ptr(&task) != Arc::as_ptr(&condvar.task) {
            temp.push(condvar);
        }
    }
    timers.clear();
    timers.append(&mut temp);
    trace!("kernel: remove_timer END");
}

/// Check if the timer has expired
pub fn check_timer() {
    // trace!("kernel: check_timer");
    let current_ms = get_time_ms();
    let mut timers = TIMERS.lock();
    while let Some(timer) = timers.peek() {
        if timer.expire_ms <= current_ms {
            if let Some(tag) = timer.poll_tag {
                if poll::handle_poll_timeout(tag, &timer.task) {
                    timers.pop();
                }
                continue;
            }
            // hart A 将任务入堆但还没标为 Blocked (即目前是 Running)，此时 timer 已经过期，被 hart B 检查。 
            // 此时唤醒不成功，但仍然要保留 timer。
            // 这种情形应该较少，且最多损失一个时间片，是可以接受的。
            if wakeup_task(Arc::clone(&timer.task)) {
                timers.pop();
            }
        } else {
            break;
        }
    }
}
