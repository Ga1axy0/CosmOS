//! Scheduling policy definitions and defaults.

/// Linux-like RT priority range supported by this kernel.
pub const SCHED_RT_PRIO_MIN: u8 = 1;
/// Linux-like RT priority range supported by this kernel.
pub const SCHED_RT_PRIO_MAX: u8 = 99;
/// Default time slice for SCHED_RR tasks, in timer ticks.
pub const DEFAULT_TIME_SLICE_TICKS: u32 = 10;

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
/// Supported scheduling policies for phase 1.
pub enum SchedPolicy {
    /// Internal idle context, not exposed as a normal runnable task policy.
    Idle = 0,
    /// Linux-like real-time round-robin scheduling.
    Rr = 2,
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
}

impl SchedAttr {
    /// Create a round-robin scheduling attribute set with the given RT priority.
    pub const fn rr(rt_priority: u8) -> Self {
        Self {
            policy: SchedPolicy::Rr,
            rt_priority,
            time_slice_ticks: DEFAULT_TIME_SLICE_TICKS,
        }
    }
}

impl Default for SchedAttr {
    fn default() -> Self {
        Self::rr(SCHED_RT_PRIO_MIN)
    }
}
