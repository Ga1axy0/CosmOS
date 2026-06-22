//! Per-hart interrupt/preemption state used by irq-on kernel paths.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::config::MAX_HARTS;
use crate::hal::hartid;

static HARDIRQ_DEPTH: [AtomicUsize; MAX_HARTS] =
    [const { AtomicUsize::new(0) }; MAX_HARTS];
static NOIRQ_LOCK_DEPTH: [AtomicUsize; MAX_HARTS] =
    [const { AtomicUsize::new(0) }; MAX_HARTS];

#[inline]
fn slot(counters: &[AtomicUsize; MAX_HARTS]) -> &AtomicUsize {
    &counters[hartid().min(MAX_HARTS.saturating_sub(1))]
}

/// Guard for hard interrupt context. Interrupt handlers should not sleep.
pub struct HardIrqGuard;

impl HardIrqGuard {
    /// Enter hard interrupt context for the current hart.
    #[inline]
    pub fn enter() -> Self {
        slot(&HARDIRQ_DEPTH).fetch_add(1, Ordering::Relaxed);
        Self
    }
}

impl Drop for HardIrqGuard {
    #[inline]
    fn drop(&mut self) {
        let prev = slot(&HARDIRQ_DEPTH).fetch_sub(1, Ordering::Relaxed);
        debug_assert!(prev > 0, "hardirq depth underflow");
    }
}

/// Record one held irq-off spin critical section.
#[inline]
pub fn enter_noirq_lock() {
    slot(&NOIRQ_LOCK_DEPTH).fetch_add(1, Ordering::Relaxed);
}

/// Leave one held irq-off spin critical section.
#[inline]
pub fn exit_noirq_lock() {
    let prev = slot(&NOIRQ_LOCK_DEPTH).fetch_sub(1, Ordering::Relaxed);
    debug_assert!(prev > 0, "noirq lock depth underflow");
}

#[inline]
pub fn in_hardirq() -> bool {
    slot(&HARDIRQ_DEPTH).load(Ordering::Relaxed) != 0
}

#[inline]
pub fn noirq_lock_depth() -> usize {
    slot(&NOIRQ_LOCK_DEPTH).load(Ordering::Relaxed)
}

#[inline]
pub fn can_sleep() -> bool {
    !in_hardirq() && noirq_lock_depth() == 0
}

/// Temporarily enable local interrupts while executing ordinary kernel code.
///
/// This guard is intentionally inert in hardirq or irq-off lock context. It is
/// for syscall/page-fault style code after the kernel trap entry is installed;
/// hardirq handlers themselves remain non-nested in this first phase.
pub struct KernelIrqEnableGuard {
    enabled: bool,
}

impl KernelIrqEnableGuard {
    /// Enable local interrupts until the returned guard is dropped.
    #[inline]
    pub fn new() -> Self {
        if can_sleep() && !crate::hal::local_irqs_enabled() {
            unsafe { crate::hal::enable_local_irqs() };
            Self { enabled: true }
        } else {
            Self { enabled: false }
        }
    }
}

impl Drop for KernelIrqEnableGuard {
    #[inline]
    fn drop(&mut self) {
        if self.enabled {
            unsafe { crate::hal::disable_local_irqs() };
        }
    }
}
