//! Scheduling policy definitions and defaults.

/// Linux default fair-task load weight for nice 0.
pub const NICE_0_LOAD: u64 = 1024;
/// Minimum Linux nice value.
pub const MIN_NICE: i32 = -20;
/// Maximum Linux nice value.
pub const MAX_NICE: i32 = 19;
/// Linux-like RT priority range supported by this kernel.
pub const SCHED_RT_PRIO_MIN: u8 = 1;
/// Linux-like RT priority range supported by this kernel.
pub const SCHED_RT_PRIO_MAX: u8 = 99;
/// Default time slice for SCHED_RR tasks, in timer ticks.
pub const DEFAULT_TIME_SLICE_TICKS: u32 = 10;
/// CFS target latency, in nanoseconds.
pub const CFS_TARGET_LATENCY_NS: u64 = 24_000_000;
/// CFS minimum scheduling granularity, in nanoseconds.
pub const CFS_MIN_GRANULARITY_NS: u64 = 3_000_000;
/// CFS wakeup preemption granularity, in nanoseconds.
pub const CFS_WAKEUP_GRANULARITY_NS: u64 = 1_000_000;
/// Penalty applied to SCHED_OTHER tasks that voluntarily yield.
pub const CFS_YIELD_PENALTY_NS: u64 = CFS_MIN_GRANULARITY_NS;

/// Linux `prio_to_weight` table for nice -20..=19.
pub const PRIO_TO_WEIGHT: [u64; 40] = [
    88761, 71755, 56483, 46273, 36291, 29154, 23254, 18705, 14949, 11916, 9548, 7620, 6100, 4904,
    3906, 3121, 2501, 1991, 1586, 1277, 1024, 820, 655, 526, 423, 335, 272, 215, 172, 137, 110, 87,
    70, 56, 45, 36, 29, 23, 18, 15,
];

/// Clamp a nice value into the Linux nice range.
pub const fn clamp_nice(nice: i32) -> i32 {
    if nice < MIN_NICE {
        MIN_NICE
    } else if nice > MAX_NICE {
        MAX_NICE
    } else {
        nice
    }
}

/// Return the Linux CFS weight for a nice value.
pub const fn nice_to_weight(nice: i32) -> u64 {
    PRIO_TO_WEIGHT[(clamp_nice(nice) - MIN_NICE) as usize]
}

/// Supported scheduling policies.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(i32)]
pub enum SchedPolicy {
    /// Internal idle context, not exposed as a normal runnable task policy.
    Idle = -1,
    /// Linux-like regular fair scheduling (SCHED_OTHER/SCHED_NORMAL).
    Other = 0,
    /// Linux-like real-time first-in, first-out scheduling.
    Fifo = 1,
    /// Linux-like real-time round-robin scheduling.
    Rr = 2,
}

impl SchedPolicy {
    /// Return whether this policy belongs to the Linux real-time scheduling class.
    pub const fn is_rt(self) -> bool {
        matches!(self, Self::Fifo | Self::Rr)
    }
}

/// Why the current task should leave the CPU at the next safe point.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ReschedReason {
    /// A runnable RT task with higher static priority should preempt the current task.
    HigherRtPriority,
    /// The current RR task exhausted its round-robin quantum.
    RrTimesliceExpired,
    /// The current task voluntarily called `sched_yield`.
    Yield,
    /// A fair task should be preempted by CFS placement/runtime rules.
    CfsPreempt,
    /// The current task must reschedule because its CPU placement changed.
    Migration,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
/// In-kernel scheduling attributes inherited by newly created tasks.
pub struct SchedAttr {
    /// Scheduling policy.
    pub policy: SchedPolicy,
    /// Real-time priority in the range `1..=99`.
    pub rt_priority: u8,
    /// Round-robin time slice length in timer ticks.
    pub time_slice_ticks: u32,
    /// Linux nice value for CFS tasks.
    pub nice: i32,
    /// CFS load weight derived from nice.
    pub weight: u64,
}

impl SchedAttr {
    /// Create a regular fair scheduling attribute set with the given nice value.
    pub const fn other(nice: i32) -> Self {
        Self {
            policy: SchedPolicy::Other,
            rt_priority: 0,
            time_slice_ticks: DEFAULT_TIME_SLICE_TICKS,
            nice: clamp_nice(nice),
            weight: nice_to_weight(nice),
        }
    }

    /// Create a round-robin scheduling attribute set with the given RT priority.
    pub const fn rr(rt_priority: u8) -> Self {
        Self {
            policy: SchedPolicy::Rr,
            rt_priority,
            time_slice_ticks: DEFAULT_TIME_SLICE_TICKS,
            nice: 0,
            weight: NICE_0_LOAD,
        }
    }

    /// Create a FIFO real-time scheduling attribute set with the given RT priority.
    pub const fn fifo(rt_priority: u8) -> Self {
        Self {
            policy: SchedPolicy::Fifo,
            rt_priority,
            time_slice_ticks: DEFAULT_TIME_SLICE_TICKS,
            nice: 0,
            weight: NICE_0_LOAD,
        }
    }
}

impl Default for SchedAttr {
    fn default() -> Self {
        Self::other(0)
    }
}
