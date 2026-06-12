//! Minimal autogroup state shared with procfs/sysctl exposure.
//!
//! xxOS does not yet maintain Linux-style autogroup scheduling entities, but
//! LTP expects the global enable knob to exist and be writable through
//! `/proc/sys/kernel/sched_autogroup_enabled`.

use core::sync::atomic::{AtomicBool, Ordering};

/// Global autogroup enable flag exposed via `/proc/sys/kernel/sched_autogroup_enabled`.
static SCHED_AUTOGROUP_ENABLED: AtomicBool = AtomicBool::new(true);

/// Return whether scheduler autogrouping is currently enabled.
pub fn autogroup_enabled() -> bool {
    SCHED_AUTOGROUP_ENABLED.load(Ordering::Relaxed)
}

/// Update the scheduler autogroup enable flag.
pub fn set_autogroup_enabled(enabled: bool) {
    SCHED_AUTOGROUP_ENABLED.store(enabled, Ordering::Relaxed);
}
