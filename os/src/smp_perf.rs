//! Low-overhead SMP scheduling and lock diagnostics.
//!
//! Counters are disabled by default.  Enable them through `/proc/smp_perf_enable`
//! only for a diagnostic run, then disable them before reading `/proc/smp_perf`
//! so the procfs reader does not become part of the measured interval.

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::config::{CLOCK_FREQ, MAX_HARTS};
use crate::hal::hartid;

/// Generic spin-lock acquisitions that do not have a more specific class.
pub(crate) const LOCK_CLASS_OTHER: usize = 0;
/// Process PCB lock acquisitions.
pub(crate) const LOCK_CLASS_PROCESS: usize = 1;
/// Task TCB lock acquisitions.
pub(crate) const LOCK_CLASS_TASK: usize = 2;
/// Per-hart runnable queue locks.
pub(crate) const LOCK_CLASS_RUNQUEUE: usize = 3;
/// Per-hart processor/current-task locks.
pub(crate) const LOCK_CLASS_PROCESSOR: usize = 4;
/// Per-hart timer heap and periodic-deadline locks.
pub(crate) const LOCK_CLASS_TIMER: usize = 5;
/// Scheduler-global PID-to-process map lock.
pub(crate) const LOCK_CLASS_SCHED_GLOBAL: usize = 6;
/// Kernel address-space and frame allocator locks.
pub(crate) const LOCK_CLASS_MEMORY: usize = 7;
/// Page-cache manager lock.
pub(crate) const LOCK_CLASS_PAGE_CACHE: usize = 8;
/// Generic networking locks, including the global network stack.
pub(crate) const LOCK_CLASS_NETWORK: usize = 9;
/// WaitQueue and keyed-wait-queue state locks.
pub(crate) const LOCK_CLASS_WAIT_QUEUE: usize = 10;
/// Global task/thread registries and task-id allocators.
pub(crate) const LOCK_CLASS_TASK_REGISTRY: usize = 11;
/// Direct ProcessControlBlock inner-lock acquisitions not using the helper.
pub(crate) const LOCK_CLASS_PROCESS_DIRECT: usize = 12;
/// Kernel heap allocator and heap page-table locks.
pub(crate) const LOCK_CLASS_HEAP: usize = 13;
/// Device and driver state locks.
pub(crate) const LOCK_CLASS_DRIVER: usize = 14;
/// TLB shootdown serialization locks.
pub(crate) const LOCK_CLASS_TLB: usize = 15;
/// Plain SpinLock acquisitions without a more specific class.
pub(crate) const LOCK_CLASS_SPIN: usize = 16;
/// Futex registry and queue locks.
pub(crate) const LOCK_CLASS_FUTEX: usize = 17;
/// Filesystem/epoll registry locks.
pub(crate) const LOCK_CLASS_FS: usize = 18;
/// Process address-space token lookups, especially the user-trap return path.
pub(crate) const LOCK_CLASS_PROCESS_USER_TOKEN: usize = 19;
const LOCK_CLASS_COUNT: usize = 20;

const LOCK_CLASS_NAMES: [&str; LOCK_CLASS_COUNT] = [
    "other",
    "process",
    "task",
    "runqueue",
    "processor",
    "timer",
    "scheduler_global",
    "memory",
    "page_cache",
    "network",
    "wait_queue",
    "task_registry",
    "process_direct",
    "heap",
    "driver",
    "tlb",
    "spin",
    "futex",
    "fs",
    "process_user_token",
];

static ENABLED: AtomicBool = AtomicBool::new(false);

static USER_TRAPS: [AtomicUsize; MAX_HARTS] = [const { AtomicUsize::new(0) }; MAX_HARTS];
static USER_TRAP_TICKS: [AtomicUsize; MAX_HARTS] = [const { AtomicUsize::new(0) }; MAX_HARTS];
static USER_TRAP_START: [AtomicUsize; MAX_HARTS] = [const { AtomicUsize::new(0) }; MAX_HARTS];
static TIMER_IRQS: [AtomicUsize; MAX_HARTS] = [const { AtomicUsize::new(0) }; MAX_HARTS];
static PERIODIC_TICKS: [AtomicUsize; MAX_HARTS] = [const { AtomicUsize::new(0) }; MAX_HARTS];
static SOFTWARE_IRQS: [AtomicUsize; MAX_HARTS] = [const { AtomicUsize::new(0) }; MAX_HARTS];
static CONTEXT_SWITCHES: [AtomicUsize; MAX_HARTS] = [const { AtomicUsize::new(0) }; MAX_HARTS];

static LOCK_WAIT_EVENTS: [[AtomicUsize; MAX_HARTS]; LOCK_CLASS_COUNT] =
    [const { [const { AtomicUsize::new(0) }; MAX_HARTS] }; LOCK_CLASS_COUNT];
static LOCK_WAIT_SPINS: [[AtomicUsize; MAX_HARTS]; LOCK_CLASS_COUNT] =
    [const { [const { AtomicUsize::new(0) }; MAX_HARTS] }; LOCK_CLASS_COUNT];
static LOCK_WAIT_TICKS: [[AtomicUsize; MAX_HARTS]; LOCK_CLASS_COUNT] =
    [const { [const { AtomicUsize::new(0) }; MAX_HARTS] }; LOCK_CLASS_COUNT];
static LOCK_MAX_SPINS: [[AtomicUsize; MAX_HARTS]; LOCK_CLASS_COUNT] =
    [const { [const { AtomicUsize::new(0) }; MAX_HARTS] }; LOCK_CLASS_COUNT];
static LOCK_ACQUIRES: [[AtomicUsize; MAX_HARTS]; LOCK_CLASS_COUNT] =
    [const { [const { AtomicUsize::new(0) }; MAX_HARTS] }; LOCK_CLASS_COUNT];
static LOCK_CONTENDED: [[AtomicUsize; MAX_HARTS]; LOCK_CLASS_COUNT] =
    [const { [const { AtomicUsize::new(0) }; MAX_HARTS] }; LOCK_CLASS_COUNT];
static LOCK_HOLD_TICKS: [[AtomicUsize; MAX_HARTS]; LOCK_CLASS_COUNT] =
    [const { [const { AtomicUsize::new(0) }; MAX_HARTS] }; LOCK_CLASS_COUNT];
static LOCK_MAX_HOLD_TICKS: [[AtomicUsize; MAX_HARTS]; LOCK_CLASS_COUNT] =
    [const { [const { AtomicUsize::new(0) }; MAX_HARTS] }; LOCK_CLASS_COUNT];

#[inline]
fn normalized_hart() -> usize {
    hartid().min(MAX_HARTS.saturating_sub(1))
}

/// Return whether diagnostic counters are currently enabled.
#[inline]
pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Enable or disable diagnostic counters.
pub(crate) fn set_enabled(value: bool) {
    ENABLED.store(value, Ordering::Release);
}

/// Record entry into a user-originated trap.
pub(crate) fn user_trap_enter(now: usize) {
    if !enabled() {
        return;
    }
    let hart = normalized_hart();
    USER_TRAPS[hart].fetch_add(1, Ordering::Relaxed);
    USER_TRAP_START[hart].store(now, Ordering::Relaxed);
}

/// Record return from a user-originated trap.
pub(crate) fn user_trap_return(now: usize) {
    if !enabled() {
        return;
    }
    let hart = normalized_hart();
    let start = USER_TRAP_START[hart].swap(0, Ordering::Relaxed);
    if start != 0 {
        USER_TRAP_TICKS[hart].fetch_add(now.saturating_sub(start), Ordering::Relaxed);
    }
}

/// Record one supervisor timer interrupt, regardless of current privilege path.
pub(crate) fn timer_irq() {
    if enabled() {
        TIMER_IRQS[normalized_hart()].fetch_add(1, Ordering::Relaxed);
    }
}

/// Record one periodic scheduler tick.
pub(crate) fn periodic_tick() {
    if enabled() {
        PERIODIC_TICKS[normalized_hart()].fetch_add(1, Ordering::Relaxed);
    }
}

/// Record one software interrupt/reschedule IPI.
pub(crate) fn software_irq() {
    if enabled() {
        SOFTWARE_IRQS[normalized_hart()].fetch_add(1, Ordering::Relaxed);
    }
}

/// Record one scheduler context switch on the current hart.
pub(crate) fn context_switch() {
    if enabled() {
        CONTEXT_SWITCHES[normalized_hart()].fetch_add(1, Ordering::Relaxed);
    }
}

fn update_max(counter: &AtomicUsize, value: usize) {
    let mut old = counter.load(Ordering::Relaxed);
    while value > old {
        match counter.compare_exchange_weak(old, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => old = observed,
        }
    }
}

/// Record a successful lock acquisition.
pub(crate) fn lock_acquired(class: usize, contended: bool) {
    if class >= LOCK_CLASS_COUNT {
        return;
    }
    let hart = normalized_hart();
    LOCK_ACQUIRES[class][hart].fetch_add(1, Ordering::Relaxed);
    if contended {
        LOCK_CONTENDED[class][hart].fetch_add(1, Ordering::Relaxed);
    }
}

/// Record the interval for which a diagnostic-enabled lock guard was held.
pub(crate) fn lock_released(class: usize, hold_ticks: usize) {
    if class >= LOCK_CLASS_COUNT {
        return;
    }
    let hart = normalized_hart();
    LOCK_HOLD_TICKS[class][hart].fetch_add(hold_ticks, Ordering::Relaxed);
    update_max(&LOCK_MAX_HOLD_TICKS[class][hart], hold_ticks);
}

/// Record a contended spin-lock acquisition.
pub(crate) fn lock_wait(class: usize, spins: usize, wait_ticks: usize) {
    if !enabled() || class >= LOCK_CLASS_COUNT || spins == 0 {
        return;
    }
    let hart = normalized_hart();
    LOCK_WAIT_EVENTS[class][hart].fetch_add(1, Ordering::Relaxed);
    LOCK_WAIT_SPINS[class][hart].fetch_add(spins, Ordering::Relaxed);
    LOCK_WAIT_TICKS[class][hart].fetch_add(wait_ticks, Ordering::Relaxed);
    update_max(&LOCK_MAX_SPINS[class][hart], spins);
}

/// Record one failed non-blocking lock probe. The caller may retry and account
/// the elapsed time separately; this function intentionally records only the
/// failed attempt and one spin so try-lock loops are visible in diagnostics.
pub(crate) fn lock_try_failed(class: usize) {
    if !enabled() || class >= LOCK_CLASS_COUNT {
        return;
    }
    let hart = normalized_hart();
    LOCK_WAIT_EVENTS[class][hart].fetch_add(1, Ordering::Relaxed);
    LOCK_WAIT_SPINS[class][hart].fetch_add(1, Ordering::Relaxed);
    update_max(&LOCK_MAX_SPINS[class][hart], 1);
}

fn reset_array(counters: &[AtomicUsize; MAX_HARTS]) {
    for counter in counters {
        counter.store(0, Ordering::Relaxed);
    }
}

/// Reset all diagnostic counters.
pub(crate) fn reset() {
    reset_array(&USER_TRAPS);
    reset_array(&USER_TRAP_TICKS);
    reset_array(&USER_TRAP_START);
    reset_array(&TIMER_IRQS);
    reset_array(&PERIODIC_TICKS);
    reset_array(&SOFTWARE_IRQS);
    reset_array(&CONTEXT_SWITCHES);
    for class in 0..LOCK_CLASS_COUNT {
        reset_array(&LOCK_WAIT_EVENTS[class]);
        reset_array(&LOCK_WAIT_SPINS[class]);
        reset_array(&LOCK_WAIT_TICKS[class]);
        reset_array(&LOCK_MAX_SPINS[class]);
        reset_array(&LOCK_ACQUIRES[class]);
        reset_array(&LOCK_CONTENDED[class]);
        reset_array(&LOCK_HOLD_TICKS[class]);
        reset_array(&LOCK_MAX_HOLD_TICKS[class]);
    }
}

/// Render a procfs-friendly snapshot of all counters.
pub(crate) fn render() -> String {
    let mut out = String::new();
    let _ = writeln!(&mut out, "enabled {}", if enabled() { 1 } else { 0 });
    let _ = write!(
        &mut out,
        "# hart user_traps user_trap_ticks timer_irqs periodic_ticks software_irqs context_switches"
    );
    for name in LOCK_CLASS_NAMES {
        let _ = write!(&mut out, " {}_waits", name);
    }
    let _ = writeln!(&mut out);
    for hart in 0..MAX_HARTS {
        let _ = write!(
            &mut out,
            "hart {} {} {} {} {} {} {}",
            hart,
            USER_TRAPS[hart].load(Ordering::Relaxed),
            USER_TRAP_TICKS[hart].load(Ordering::Relaxed),
            TIMER_IRQS[hart].load(Ordering::Relaxed),
            PERIODIC_TICKS[hart].load(Ordering::Relaxed),
            SOFTWARE_IRQS[hart].load(Ordering::Relaxed),
            CONTEXT_SWITCHES[hart].load(Ordering::Relaxed),
        );
        for class in 0..LOCK_CLASS_COUNT {
            let _ = write!(
                &mut out,
                " {}",
                LOCK_WAIT_EVENTS[class][hart].load(Ordering::Relaxed)
            );
        }
        let _ = writeln!(&mut out);
    }

    let _ = writeln!(
        &mut out,
        "# hart_lock_class hart class acquires contended wait_events wait_spins wait_ticks max_wait_spins hold_ticks max_hold_ticks"
    );
    for hart in 0..MAX_HARTS {
        for (class, name) in LOCK_CLASS_NAMES.iter().enumerate() {
            let acquires = LOCK_ACQUIRES[class][hart].load(Ordering::Relaxed);
            let contended = LOCK_CONTENDED[class][hart].load(Ordering::Relaxed);
            let wait_events = LOCK_WAIT_EVENTS[class][hart].load(Ordering::Relaxed);
            let wait_spins = LOCK_WAIT_SPINS[class][hart].load(Ordering::Relaxed);
            let wait_ticks = LOCK_WAIT_TICKS[class][hart].load(Ordering::Relaxed);
            let max_wait_spins = LOCK_MAX_SPINS[class][hart].load(Ordering::Relaxed);
            let hold_ticks = LOCK_HOLD_TICKS[class][hart].load(Ordering::Relaxed);
            let max_hold_ticks = LOCK_MAX_HOLD_TICKS[class][hart].load(Ordering::Relaxed);
            if acquires == 0
                && contended == 0
                && wait_events == 0
                && hold_ticks == 0
                && max_hold_ticks == 0
            {
                continue;
            }
            let _ = writeln!(
                &mut out,
                "hart_lock {} {} {} {} {} {} {} {} {} {}",
                hart,
                name,
                acquires,
                contended,
                wait_events,
                wait_spins,
                wait_ticks,
                max_wait_spins,
                hold_ticks,
                max_hold_ticks,
            );
        }
    }

    let _ = writeln!(&mut out, "# timer_ticks_per_second {}", CLOCK_FREQ);
    let _ = writeln!(
        &mut out,
        "# lock_class acquires contended wait_events wait_spins wait_ticks max_wait_spins hold_ticks max_hold_ticks"
    );
    for (class, name) in LOCK_CLASS_NAMES.iter().enumerate() {
        let mut acquires = 0usize;
        let mut contended = 0usize;
        let mut events = 0usize;
        let mut spins = 0usize;
        let mut ticks = 0usize;
        let mut max_spins = 0usize;
        let mut hold_ticks = 0usize;
        let mut max_hold_ticks = 0usize;
        for hart in 0..MAX_HARTS {
            acquires = acquires.saturating_add(LOCK_ACQUIRES[class][hart].load(Ordering::Relaxed));
            contended = contended.saturating_add(LOCK_CONTENDED[class][hart].load(Ordering::Relaxed));
            events = events.saturating_add(LOCK_WAIT_EVENTS[class][hart].load(Ordering::Relaxed));
            spins = spins.saturating_add(LOCK_WAIT_SPINS[class][hart].load(Ordering::Relaxed));
            ticks = ticks.saturating_add(LOCK_WAIT_TICKS[class][hart].load(Ordering::Relaxed));
            max_spins = max_spins.max(LOCK_MAX_SPINS[class][hart].load(Ordering::Relaxed));
            hold_ticks = hold_ticks.saturating_add(LOCK_HOLD_TICKS[class][hart].load(Ordering::Relaxed));
            max_hold_ticks = max_hold_ticks.max(LOCK_MAX_HOLD_TICKS[class][hart].load(Ordering::Relaxed));
        }
        let _ = writeln!(
            &mut out,
            "lock {} {} {} {} {} {} {} {} {}",
            name,
            acquires,
            contended,
            events,
            spins,
            ticks,
            max_spins,
            hold_ticks,
            max_hold_ticks,
        );
    }
    out
}
