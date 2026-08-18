//! Barrier-Aware Interference Spreading (BAIS).
//!
//! BAIS is a native CosmOS policy for the competition demo. An AI process
//! registers its fork-join phase through a small hint syscall. AI workers are
//! spread stably across the allowed harts while ordinary work is placed on the
//! hart with the smallest predicted AI completion time. Pending placements are
//! reserved immediately so concurrent wakeups do not herd onto one hart.

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crate::config::MAX_HARTS;
use crate::mm::online_mask;
use crate::task::{SchedPolicy, TaskControlBlock};
use crate::timer::get_time_ns;

const PROGRESS_SCALE: usize = 1000;
const DEFAULT_NON_AI_COST_NS: u64 = 100_000;
const MIN_NON_AI_COST_NS: u64 = 20_000;
const MAX_NON_AI_COST_NS: u64 = 500_000;
const UNKNOWN_REMAINING_NS: u64 = u64::MAX / 8;

pub const HINT_REGISTER: usize = 1;
pub const HINT_PHASE_START: usize = 2;
pub const HINT_PROGRESS: usize = 3;
pub const HINT_ARRIVED: usize = 4;
pub const HINT_UNREGISTER: usize = 5;

static ENABLED: AtomicBool = AtomicBool::new(false);
static AI_PID: AtomicUsize = AtomicUsize::new(0);
static AI_WORKERS: AtomicUsize = AtomicUsize::new(0);
static PHASE: AtomicUsize = AtomicUsize::new(0);
static PHASE_START_NS: AtomicU64 = AtomicU64::new(0);
static ARRIVED_MASK: AtomicUsize = AtomicUsize::new(0);

static SELECT_AI: AtomicU64 = AtomicU64::new(0);
static SELECT_NON_AI: AtomicU64 = AtomicU64::new(0);
static FALLBACKS: AtomicU64 = AtomicU64::new(0);
static HINTS: AtomicU64 = AtomicU64::new(0);
static BLOCK_IRQS: AtomicU64 = AtomicU64::new(0);
static NET_IRQS: AtomicU64 = AtomicU64::new(0);
static BLOCK_DEFERRED_RUNS: AtomicU64 = AtomicU64::new(0);
static NET_DEFERRED_RUNS: AtomicU64 = AtomicU64::new(0);
static BLOCK_DEFERRED_MIGRATIONS: AtomicU64 = AtomicU64::new(0);
static NET_DEFERRED_MIGRATIONS: AtomicU64 = AtomicU64::new(0);
static LAST_BLOCK_IRQ_CPU: AtomicUsize = AtomicUsize::new(MAX_HARTS);
static LAST_NET_IRQ_CPU: AtomicUsize = AtomicUsize::new(MAX_HARTS);

static RUN_START_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static RUN_IS_AI: [AtomicU8; MAX_HARTS] = [const { AtomicU8::new(0) }; MAX_HARTS];
static AI_RUNTIME_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static NON_AI_RUNTIME_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static IRQ_RUNTIME_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static BLOCK_DEFERRED_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static NET_DEFERRED_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static PHASE_AI_RUNTIME_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static PHASE_NON_AI_RUNTIME_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static PHASE_IRQ_RUNTIME_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static WORKER_PROGRESS: [AtomicUsize; MAX_HARTS] = [const { AtomicUsize::new(0) }; MAX_HARTS];
static PLACEMENT_RESERVATIONS: [AtomicUsize; MAX_HARTS] =
    [const { AtomicUsize::new(0) }; MAX_HARTS];
static NON_AI_COST_EWMA_NS: AtomicU64 = AtomicU64::new(DEFAULT_NON_AI_COST_NS);
static AI_PAGE_FAULT_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static AI_PAGE_FAULTS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static AI_MEMORY_CONTROL_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static AI_MEMORY_CONTROLS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static AI_DEVICE_CONTROL_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static AI_DEVICE_CONTROLS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static AI_OTHER_SYSCALL_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static AI_OTHER_SYSCALLS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static PHASE_AI_PAGE_FAULT_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];
static PHASE_AI_MEMORY_CONTROL_NS: [AtomicU64; MAX_HARTS] =
    [const { AtomicU64::new(0) }; MAX_HARTS];
static PHASE_AI_DEVICE_CONTROL_NS: [AtomicU64; MAX_HARTS] =
    [const { AtomicU64::new(0) }; MAX_HARTS];
static PHASE_AI_OTHER_SYSCALL_NS: [AtomicU64; MAX_HARTS] = [const { AtomicU64::new(0) }; MAX_HARTS];

#[inline]
/// Return whether a BAIS placement policy is active.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

fn task_identity(task: &TaskControlBlock) -> (usize, usize) {
    let pid = task
        .process
        .upgrade()
        .map(|process| process.getpid())
        .unwrap_or(0);
    let tid = task
        .inner_exclusive_access()
        .res
        .as_ref()
        .map(|res| res.tid)
        .unwrap_or(pid);
    (pid, tid)
}

#[inline]
pub(crate) fn is_ai_task(task: &TaskControlBlock) -> bool {
    let ai_pid = AI_PID.load(Ordering::Acquire);
    ai_pid != 0
        && task
            .process
            .upgrade()
            .is_some_and(|process| process.getpid() == ai_pid)
}

fn nth_cpu(mask: usize, index: usize) -> Option<usize> {
    let count = mask.count_ones() as usize;
    if count == 0 {
        return None;
    }
    let mut remaining = index % count;
    for cpu in 0..MAX_HARTS {
        if mask & (1usize << cpu) == 0 {
            continue;
        }
        if remaining == 0 {
            return Some(cpu);
        }
        remaining -= 1;
    }
    None
}

fn phase_interference_debt_ns(cpu: usize) -> u64 {
    PHASE_NON_AI_RUNTIME_NS[cpu]
        .load(Ordering::Relaxed)
        .saturating_add(PHASE_IRQ_RUNTIME_NS[cpu].load(Ordering::Relaxed))
        .saturating_add(PHASE_AI_PAGE_FAULT_NS[cpu].load(Ordering::Relaxed))
        .saturating_add(PHASE_AI_MEMORY_CONTROL_NS[cpu].load(Ordering::Relaxed))
        .saturating_add(PHASE_AI_DEVICE_CONTROL_NS[cpu].load(Ordering::Relaxed))
        .saturating_add(PHASE_AI_OTHER_SYSCALL_NS[cpu].load(Ordering::Relaxed))
}

fn estimate_remaining_ns(ai_runtime_ns: u64, progress: usize) -> u64 {
    if progress >= PROGRESS_SCALE {
        return 0;
    }
    if progress == 0 || ai_runtime_ns == 0 {
        return UNKNOWN_REMAINING_NS;
    }
    let remaining = (ai_runtime_ns as u128).saturating_mul((PROGRESS_SCALE - progress) as u128)
        / progress as u128;
    remaining.min(UNKNOWN_REMAINING_NS as u128) as u64
}

#[inline]
fn effective_phase_ai_runtime_ns(cpu: usize, now: u64) -> u64 {
    let accounted = PHASE_AI_RUNTIME_NS[cpu].load(Ordering::Relaxed);
    if RUN_IS_AI[cpu].load(Ordering::Acquire) == 0 {
        return accounted;
    }
    let run_start = RUN_START_NS[cpu].load(Ordering::Acquire);
    let phase_start = PHASE_START_NS.load(Ordering::Acquire);
    accounted.saturating_add(now.saturating_sub(run_start.max(phase_start)))
}

#[inline]
fn consume_placement_reservation(cpu: usize) {
    let _ = PLACEMENT_RESERVATIONS[cpu].fetch_update(
        Ordering::AcqRel,
        Ordering::Relaxed,
        |reservations| Some(reservations.saturating_sub(1)),
    );
}

fn update_non_ai_cost(elapsed_ns: u64) {
    let sample = elapsed_ns.clamp(MIN_NON_AI_COST_NS, MAX_NON_AI_COST_NS);
    let mut old = NON_AI_COST_EWMA_NS.load(Ordering::Relaxed);
    loop {
        // A slow EWMA makes the reservation price stable across short bursts.
        let next = old.saturating_mul(7).saturating_add(sample) / 8;
        match NON_AI_COST_EWMA_NS.compare_exchange_weak(
            old,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(current) => old = current,
        }
    }
}

fn slack_cpu(mask: usize) -> Option<usize> {
    let now = get_time_ns();
    let arrived = ARRIVED_MASK.load(Ordering::Acquire);
    let reservation_cost = NON_AI_COST_EWMA_NS.load(Ordering::Relaxed);
    let mut best: Option<(usize, u64, u64)> = None;
    for cpu in 0..MAX_HARTS {
        if mask & (1usize << cpu) == 0 {
            continue;
        }
        let progress = WORKER_PROGRESS[cpu].load(Ordering::Relaxed);
        let remaining = if arrived & (1usize << cpu) != 0 {
            0
        } else {
            estimate_remaining_ns(effective_phase_ai_runtime_ns(cpu, now), progress)
        };
        let reservations = PLACEMENT_RESERVATIONS[cpu].load(Ordering::Relaxed) as u64;
        let predicted_finish =
            remaining.saturating_add(reservations.saturating_mul(reservation_cost));
        let debt = phase_interference_debt_ns(cpu);
        if best.is_none_or(|(_, best_finish, best_debt)| {
            predicted_finish < best_finish || (predicted_finish == best_finish && debt < best_debt)
        }) {
            best = Some((cpu, predicted_finish, debt));
        }
    }
    best.map(|(cpu, _, _)| {
        // Reserve before returning: another simultaneous wakeup observes this
        // cost even though the selected task has not begun executing yet.
        PLACEMENT_RESERVATIONS[cpu].fetch_add(1, Ordering::AcqRel);
        cpu
    })
}

/// Select a hart for an ordinary CFS task.
///
/// AI workers retain a stable one-worker-per-hart mapping. Other tasks use
/// the selected interference-spreading policy while an AI process is active.
pub(crate) fn select_task_cpu(
    task: &TaskControlBlock,
    allowed_cpus: usize,
    sched_policy: SchedPolicy,
) -> Option<usize> {
    if !enabled()
        || AI_PID.load(Ordering::Acquire) == 0
        || !matches!(sched_policy, SchedPolicy::Other)
    {
        return None;
    }

    let (pid, tid) = task_identity(task);
    let selected = if pid == AI_PID.load(Ordering::Acquire) {
        SELECT_AI.fetch_add(1, Ordering::Relaxed);
        nth_cpu(allowed_cpus, tid)
    } else {
        SELECT_NON_AI.fetch_add(1, Ordering::Relaxed);
        slack_cpu(allowed_cpus)
    };
    if selected.is_none() {
        FALLBACKS.fetch_add(1, Ordering::Relaxed);
    }
    selected
}

/// Keep a registered AI worker on its stable BAIS hart during idle stealing.
pub(crate) fn allow_steal(task: &TaskControlBlock, target_cpu: usize) -> bool {
    if !enabled() || !is_ai_task(task) {
        return true;
    }
    let (_, tid) = task_identity(task);
    let allowed = task.inner_exclusive_access().sched.cpu_affinity_mask & online_mask();
    nth_cpu(allowed, tid).is_none_or(|home| home == target_cpu)
}

#[inline]
pub(crate) fn on_task_running(task: &TaskControlBlock, cpu: usize) {
    if !enabled() || cpu >= MAX_HARTS {
        return;
    }
    let is_ai = is_ai_task(task);
    if !is_ai {
        consume_placement_reservation(cpu);
    }
    RUN_IS_AI[cpu].store(is_ai as u8, Ordering::Relaxed);
    RUN_START_NS[cpu].store(get_time_ns(), Ordering::Release);
}

#[inline]
pub(crate) fn on_task_stopping(_task: &TaskControlBlock, cpu: usize) {
    if !enabled() || cpu >= MAX_HARTS {
        return;
    }
    let start = RUN_START_NS[cpu].swap(0, Ordering::AcqRel);
    if start == 0 {
        return;
    }
    let now = get_time_ns();
    let elapsed = now.saturating_sub(start);
    let phase_elapsed = now.saturating_sub(start.max(PHASE_START_NS.load(Ordering::Acquire)));
    if RUN_IS_AI[cpu].swap(0, Ordering::Relaxed) != 0 {
        AI_RUNTIME_NS[cpu].fetch_add(elapsed, Ordering::Relaxed);
        PHASE_AI_RUNTIME_NS[cpu].fetch_add(phase_elapsed, Ordering::Relaxed);
    } else {
        NON_AI_RUNTIME_NS[cpu].fetch_add(elapsed, Ordering::Relaxed);
        PHASE_NON_AI_RUNTIME_NS[cpu].fetch_add(phase_elapsed, Ordering::Relaxed);
        update_non_ai_cost(elapsed);
    }
}

/// Attribute hard-IRQ work to the hart that interrupted an AI phase.
pub fn account_irq(cpu: usize, elapsed_ns: u64) {
    if enabled() && cpu < MAX_HARTS {
        IRQ_RUNTIME_NS[cpu].fetch_add(elapsed_ns, Ordering::Relaxed);
        PHASE_IRQ_RUNTIME_NS[cpu].fetch_add(elapsed_ns, Ordering::Relaxed);
    }
}

#[inline]
fn current_is_registered_ai() -> bool {
    crate::task::current_task()
        .as_deref()
        .is_some_and(is_ai_task)
}

fn phase_service_overlap_ns(start_ns: u64, elapsed_ns: u64) -> u64 {
    let end_ns = start_ns.saturating_add(elapsed_ns);
    end_ns.saturating_sub(start_ns.max(PHASE_START_NS.load(Ordering::Acquire)))
}

/// Account a synchronous user page-fault stall on the registered AI task.
pub fn account_ai_page_fault(cpu: usize, start_ns: u64, elapsed_ns: u64) {
    if !enabled() || cpu >= MAX_HARTS || !current_is_registered_ai() {
        return;
    }
    AI_PAGE_FAULTS[cpu].fetch_add(1, Ordering::Relaxed);
    AI_PAGE_FAULT_NS[cpu].fetch_add(elapsed_ns, Ordering::Relaxed);
    PHASE_AI_PAGE_FAULT_NS[cpu].fetch_add(
        phase_service_overlap_ns(start_ns, elapsed_ns),
        Ordering::Relaxed,
    );
}

/// Account mmap/munmap/brk and related memory-control work performed for AI.
pub fn account_ai_memory_control(cpu: usize, start_ns: u64, elapsed_ns: u64) {
    if !enabled() || cpu >= MAX_HARTS || !current_is_registered_ai() {
        return;
    }
    AI_MEMORY_CONTROLS[cpu].fetch_add(1, Ordering::Relaxed);
    AI_MEMORY_CONTROL_NS[cpu].fetch_add(elapsed_ns, Ordering::Relaxed);
    PHASE_AI_MEMORY_CONTROL_NS[cpu].fetch_add(
        phase_service_overlap_ns(start_ns, elapsed_ns),
        Ordering::Relaxed,
    );
}

/// Account read/write/ioctl/fsync-style device-control work performed for AI.
pub fn account_ai_device_control(cpu: usize, start_ns: u64, elapsed_ns: u64) {
    if !enabled() || cpu >= MAX_HARTS || !current_is_registered_ai() {
        return;
    }
    AI_DEVICE_CONTROLS[cpu].fetch_add(1, Ordering::Relaxed);
    AI_DEVICE_CONTROL_NS[cpu].fetch_add(elapsed_ns, Ordering::Relaxed);
    PHASE_AI_DEVICE_CONTROL_NS[cpu].fetch_add(
        phase_service_overlap_ns(start_ns, elapsed_ns),
        Ordering::Relaxed,
    );
}

/// Account other synchronous syscalls made by the registered AI process.
pub fn account_ai_other_syscall(cpu: usize, start_ns: u64, elapsed_ns: u64) {
    if !enabled() || cpu >= MAX_HARTS || !current_is_registered_ai() {
        return;
    }
    AI_OTHER_SYSCALLS[cpu].fetch_add(1, Ordering::Relaxed);
    AI_OTHER_SYSCALL_NS[cpu].fetch_add(elapsed_ns, Ordering::Relaxed);
    PHASE_AI_OTHER_SYSCALL_NS[cpu].fetch_add(
        phase_service_overlap_ns(start_ns, elapsed_ns),
        Ordering::Relaxed,
    );
}

/// Record the hart on which a VirtIO block interrupt requested bottom-half work.
pub fn note_block_irq(cpu: usize) {
    if enabled() && cpu < MAX_HARTS {
        BLOCK_IRQS.fetch_add(1, Ordering::Relaxed);
        LAST_BLOCK_IRQ_CPU.store(cpu, Ordering::Release);
    }
}

/// Attribute one block completion-worker run to its selected hart.
pub fn account_block_deferred(cpu: usize, elapsed_ns: u64) {
    if !enabled() || cpu >= MAX_HARTS {
        return;
    }
    BLOCK_DEFERRED_RUNS.fetch_add(1, Ordering::Relaxed);
    BLOCK_DEFERRED_NS[cpu].fetch_add(elapsed_ns, Ordering::Relaxed);
    // Consume the origin once: watchdog/extra drain loops are deferred runs,
    // but they are not additional IRQ-to-worker placement decisions.
    let origin = LAST_BLOCK_IRQ_CPU.swap(MAX_HARTS, Ordering::AcqRel);
    if origin < MAX_HARTS && origin != cpu {
        BLOCK_DEFERRED_MIGRATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Record the hart on which a network interrupt requested bottom-half work.
pub fn note_net_irq(cpu: usize) {
    if enabled() && cpu < MAX_HARTS {
        NET_IRQS.fetch_add(1, Ordering::Relaxed);
        LAST_NET_IRQ_CPU.store(cpu, Ordering::Release);
    }
}

/// Attribute one network poll-worker run to its selected hart.
pub fn account_net_deferred(cpu: usize, elapsed_ns: u64) {
    if !enabled() || cpu >= MAX_HARTS {
        return;
    }
    NET_DEFERRED_RUNS.fetch_add(1, Ordering::Relaxed);
    NET_DEFERRED_NS[cpu].fetch_add(elapsed_ns, Ordering::Relaxed);
    // Timer-driven follow-up polls must not inherit an old IRQ origin.
    let origin = LAST_NET_IRQ_CPU.swap(MAX_HARTS, Ordering::AcqRel);
    if origin < MAX_HARTS && origin != cpu {
        NET_DEFERRED_MIGRATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

fn reset_phase(phase: usize) {
    PHASE.store(phase, Ordering::Release);
    PHASE_START_NS.store(get_time_ns(), Ordering::Release);
    ARRIVED_MASK.store(0, Ordering::Release);
    for cpu in 0..MAX_HARTS {
        PHASE_AI_RUNTIME_NS[cpu].store(0, Ordering::Relaxed);
        PHASE_NON_AI_RUNTIME_NS[cpu].store(0, Ordering::Relaxed);
        PHASE_IRQ_RUNTIME_NS[cpu].store(0, Ordering::Relaxed);
        PHASE_AI_PAGE_FAULT_NS[cpu].store(0, Ordering::Relaxed);
        PHASE_AI_MEMORY_CONTROL_NS[cpu].store(0, Ordering::Relaxed);
        PHASE_AI_DEVICE_CONTROL_NS[cpu].store(0, Ordering::Relaxed);
        PHASE_AI_OTHER_SYSCALL_NS[cpu].store(0, Ordering::Relaxed);
        WORKER_PROGRESS[cpu].store(0, Ordering::Relaxed);
        PLACEMENT_RESERVATIONS[cpu].store(0, Ordering::Relaxed);
    }
}

/// Consume an AI-worker hint from the current process.
pub fn hint_current(op: usize, value: usize) -> bool {
    let pid = crate::task::current_process().getpid();
    match op {
        HINT_REGISTER => {
            AI_PID.store(pid, Ordering::Release);
            AI_WORKERS.store(value.min(MAX_HARTS), Ordering::Release);
            reset_phase(0);
            let cpu = crate::hal::hartid();
            if cpu < MAX_HARTS {
                RUN_IS_AI[cpu].store(1, Ordering::Relaxed);
                RUN_START_NS[cpu].store(get_time_ns(), Ordering::Release);
            }
        }
        HINT_PHASE_START if AI_PID.load(Ordering::Acquire) == pid => {
            reset_phase(value);
            let current_cpu = crate::hal::hartid();
            let targets = online_mask() & !(1usize << current_cpu);
            for cpu in 0..MAX_HARTS {
                if targets & (1usize << cpu) != 0 {
                    super::runqueue::resched_hart(cpu);
                }
            }
        }
        HINT_PROGRESS if AI_PID.load(Ordering::Acquire) == pid => {
            let cpu = crate::hal::hartid();
            if cpu < MAX_HARTS {
                WORKER_PROGRESS[cpu].fetch_max(value, Ordering::Relaxed);
            }
        }
        HINT_ARRIVED if AI_PID.load(Ordering::Acquire) == pid => {
            let cpu = crate::hal::hartid();
            if cpu < MAX_HARTS {
                WORKER_PROGRESS[cpu].store(1000, Ordering::Relaxed);
                ARRIVED_MASK.fetch_or(1usize << cpu, Ordering::Release);
            }
        }
        HINT_UNREGISTER if AI_PID.load(Ordering::Acquire) == pid => {
            AI_PID.store(0, Ordering::Release);
            AI_WORKERS.store(0, Ordering::Release);
            ARRIVED_MASK.store(0, Ordering::Release);
            for reservations in &PLACEMENT_RESERVATIONS {
                reservations.store(0, Ordering::Relaxed);
            }
        }
        _ => return false,
    }
    HINTS.fetch_add(1, Ordering::Relaxed);
    true
}

fn reset_stats() {
    SELECT_AI.store(0, Ordering::Relaxed);
    SELECT_NON_AI.store(0, Ordering::Relaxed);
    FALLBACKS.store(0, Ordering::Relaxed);
    HINTS.store(0, Ordering::Relaxed);
    BLOCK_IRQS.store(0, Ordering::Relaxed);
    NET_IRQS.store(0, Ordering::Relaxed);
    BLOCK_DEFERRED_RUNS.store(0, Ordering::Relaxed);
    NET_DEFERRED_RUNS.store(0, Ordering::Relaxed);
    BLOCK_DEFERRED_MIGRATIONS.store(0, Ordering::Relaxed);
    NET_DEFERRED_MIGRATIONS.store(0, Ordering::Relaxed);
    LAST_BLOCK_IRQ_CPU.store(MAX_HARTS, Ordering::Relaxed);
    LAST_NET_IRQ_CPU.store(MAX_HARTS, Ordering::Relaxed);
    NON_AI_COST_EWMA_NS.store(DEFAULT_NON_AI_COST_NS, Ordering::Relaxed);
    for cpu in 0..MAX_HARTS {
        AI_RUNTIME_NS[cpu].store(0, Ordering::Relaxed);
        NON_AI_RUNTIME_NS[cpu].store(0, Ordering::Relaxed);
        IRQ_RUNTIME_NS[cpu].store(0, Ordering::Relaxed);
        BLOCK_DEFERRED_NS[cpu].store(0, Ordering::Relaxed);
        NET_DEFERRED_NS[cpu].store(0, Ordering::Relaxed);
        AI_PAGE_FAULT_NS[cpu].store(0, Ordering::Relaxed);
        AI_PAGE_FAULTS[cpu].store(0, Ordering::Relaxed);
        AI_MEMORY_CONTROL_NS[cpu].store(0, Ordering::Relaxed);
        AI_MEMORY_CONTROLS[cpu].store(0, Ordering::Relaxed);
        AI_DEVICE_CONTROL_NS[cpu].store(0, Ordering::Relaxed);
        AI_DEVICE_CONTROLS[cpu].store(0, Ordering::Relaxed);
        AI_OTHER_SYSCALL_NS[cpu].store(0, Ordering::Relaxed);
        AI_OTHER_SYSCALLS[cpu].store(0, Ordering::Relaxed);
    }
    reset_phase(PHASE.load(Ordering::Relaxed));
}

/// Enable/disable BAIS or reset its counters through /proc/bais.
pub fn apply_control(command: &str) -> bool {
    match command.trim() {
        "off" | "0" => ENABLED.store(false, Ordering::Release),
        "bais" | "on" | "1" => ENABLED.store(true, Ordering::Release),
        "reset" => reset_stats(),
        "" => return true,
        _ => return false,
    }
    true
}

/// Render the current BAIS policy and per-hart accounting for /proc/bais.
pub fn render() -> String {
    let online = online_mask();
    let mut out = String::new();
    let _ = writeln!(
        &mut out,
        "policy {}",
        if enabled() { "bais" } else { "off" }
    );
    let _ = writeln!(
        &mut out,
        "ai_pid {} workers {} phase {} arrived {:#x}",
        AI_PID.load(Ordering::Relaxed),
        AI_WORKERS.load(Ordering::Relaxed),
        PHASE.load(Ordering::Relaxed),
        ARRIVED_MASK.load(Ordering::Relaxed),
    );
    let _ = writeln!(
        &mut out,
        "select_ai {} select_non_ai {} fallbacks {} hints {}",
        SELECT_AI.load(Ordering::Relaxed),
        SELECT_NON_AI.load(Ordering::Relaxed),
        FALLBACKS.load(Ordering::Relaxed),
        HINTS.load(Ordering::Relaxed),
    );
    let _ = writeln!(
        &mut out,
        "block_irqs {} block_runs {} block_migrations {} net_irqs {} net_runs {} net_migrations {} placement_cost_ns {}",
        BLOCK_IRQS.load(Ordering::Relaxed),
        BLOCK_DEFERRED_RUNS.load(Ordering::Relaxed),
        BLOCK_DEFERRED_MIGRATIONS.load(Ordering::Relaxed),
        NET_IRQS.load(Ordering::Relaxed),
        NET_DEFERRED_RUNS.load(Ordering::Relaxed),
        NET_DEFERRED_MIGRATIONS.load(Ordering::Relaxed),
        NON_AI_COST_EWMA_NS.load(Ordering::Relaxed),
    );
    let now = get_time_ns();
    for cpu in 0..MAX_HARTS {
        if online & (1usize << cpu) == 0 {
            continue;
        }
        let _ = writeln!(
            &mut out,
            "cpu {} ai_ns {} non_ai_ns {} irq_ns {} block_deferred_ns {} net_deferred_ns {} phase_ai_ns {} phase_non_ai_ns {} phase_irq_ns {} progress {} arrived {} reservations {} predicted_remaining_ns {}",
            cpu,
            AI_RUNTIME_NS[cpu].load(Ordering::Relaxed),
            NON_AI_RUNTIME_NS[cpu].load(Ordering::Relaxed),
            IRQ_RUNTIME_NS[cpu].load(Ordering::Relaxed),
            BLOCK_DEFERRED_NS[cpu].load(Ordering::Relaxed),
            NET_DEFERRED_NS[cpu].load(Ordering::Relaxed),
            PHASE_AI_RUNTIME_NS[cpu].load(Ordering::Relaxed),
            PHASE_NON_AI_RUNTIME_NS[cpu].load(Ordering::Relaxed),
            PHASE_IRQ_RUNTIME_NS[cpu].load(Ordering::Relaxed),
            WORKER_PROGRESS[cpu].load(Ordering::Relaxed),
            (ARRIVED_MASK.load(Ordering::Relaxed) >> cpu) & 1,
            PLACEMENT_RESERVATIONS[cpu].load(Ordering::Relaxed),
            estimate_remaining_ns(
                effective_phase_ai_runtime_ns(cpu, now),
                WORKER_PROGRESS[cpu].load(Ordering::Relaxed),
            ),
        );
        let _ = writeln!(
            &mut out,
            "cpu_service {} page_faults {} page_fault_ns {} memory_controls {} memory_control_ns {} device_controls {} device_control_ns {} other_syscalls {} other_syscall_ns {} phase_page_fault_ns {} phase_memory_control_ns {} phase_device_control_ns {} phase_other_syscall_ns {} phase_interference_debt_ns {}",
            cpu,
            AI_PAGE_FAULTS[cpu].load(Ordering::Relaxed),
            AI_PAGE_FAULT_NS[cpu].load(Ordering::Relaxed),
            AI_MEMORY_CONTROLS[cpu].load(Ordering::Relaxed),
            AI_MEMORY_CONTROL_NS[cpu].load(Ordering::Relaxed),
            AI_DEVICE_CONTROLS[cpu].load(Ordering::Relaxed),
            AI_DEVICE_CONTROL_NS[cpu].load(Ordering::Relaxed),
            AI_OTHER_SYSCALLS[cpu].load(Ordering::Relaxed),
            AI_OTHER_SYSCALL_NS[cpu].load(Ordering::Relaxed),
            PHASE_AI_PAGE_FAULT_NS[cpu].load(Ordering::Relaxed),
            PHASE_AI_MEMORY_CONTROL_NS[cpu].load(Ordering::Relaxed),
            PHASE_AI_DEVICE_CONTROL_NS[cpu].load(Ordering::Relaxed),
            PHASE_AI_OTHER_SYSCALL_NS[cpu].load(Ordering::Relaxed),
            phase_interference_debt_ns(cpu),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{estimate_remaining_ns, nth_cpu, UNKNOWN_REMAINING_NS};

    #[test]
    fn nth_cpu_respects_sparse_masks() {
        let mask = (1usize << 1) | (1usize << 4) | (1usize << 7);
        assert_eq!(nth_cpu(mask, 0), Some(1));
        assert_eq!(nth_cpu(mask, 1), Some(4));
        assert_eq!(nth_cpu(mask, 2), Some(7));
        assert_eq!(nth_cpu(mask, 3), Some(1));
        assert_eq!(nth_cpu(0, 0), None);
    }

    #[test]
    fn remaining_estimate_tracks_progress() {
        assert_eq!(estimate_remaining_ns(125, 125), 875);
        assert_eq!(estimate_remaining_ns(500, 500), 500);
        assert_eq!(estimate_remaining_ns(1000, 1000), 0);
        assert_eq!(estimate_remaining_ns(0, 0), UNKNOWN_REMAINING_NS);
    }
}
