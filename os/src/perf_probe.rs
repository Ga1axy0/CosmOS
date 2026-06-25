//! Low-overhead named timing probes for local hotspot diagnosis.

#[cfg(feature = "perf_probe")]
mod enabled {
    use alloc::string::String;
    use core::fmt::Write;
    use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use lazy_static::lazy_static;

    use crate::sync::SpinNoIrqLock;
    use crate::timer::get_time_ns;

    const MAX_PROBES: usize = 64;
    const UNREGISTERED: usize = 0;
    const INVALID_SLOT: usize = usize::MAX;

    struct ProbeStats {
        calls: AtomicU64,
        total_ns: AtomicU64,
        max_ns: AtomicU64,
    }

    impl ProbeStats {
        const fn new() -> Self {
            Self {
                calls: AtomicU64::new(0),
                total_ns: AtomicU64::new(0),
                max_ns: AtomicU64::new(0),
            }
        }

        fn reset(&self) {
            self.calls.store(0, Ordering::Relaxed);
            self.total_ns.store(0, Ordering::Relaxed);
            self.max_ns.store(0, Ordering::Relaxed);
        }
    }

    static ENABLED: AtomicBool = AtomicBool::new(false);

    lazy_static! {
        static ref PROBE_NAMES: SpinNoIrqLock<[Option<&'static str>; MAX_PROBES]> =
            SpinNoIrqLock::new([None; MAX_PROBES]);
        static ref PROBES: [ProbeStats; MAX_PROBES] = core::array::from_fn(|_| ProbeStats::new());
    }

    /// RAII scope guard that records elapsed nanoseconds on drop.
    pub(crate) struct ProbeGuard {
        slot: usize,
        start_ns: u64,
    }

    impl Drop for ProbeGuard {
        fn drop(&mut self) {
            if self.slot == INVALID_SLOT {
                return;
            }
            let elapsed_ns = get_time_ns().saturating_sub(self.start_ns);
            record_elapsed(self.slot, elapsed_ns);
        }
    }

    /// Return whether timing probes are currently enabled.
    pub(crate) fn enabled() -> bool {
        ENABLED.load(Ordering::Acquire)
    }

    /// Enable or disable timing probes.
    pub(crate) fn set_enabled(enabled: bool) {
        ENABLED.store(enabled, Ordering::Release);
    }

    /// Reset all probe counters.
    pub(crate) fn reset() {
        for probe in PROBES.iter() {
            probe.reset();
        }
    }

    fn inactive_guard() -> ProbeGuard {
        ProbeGuard {
            slot: INVALID_SLOT,
            start_ns: 0,
        }
    }

    fn cached_slot(cache: &AtomicUsize) -> Option<usize> {
        match cache.load(Ordering::Acquire) {
            UNREGISTERED => None,
            slot_plus_one => Some(slot_plus_one - 1),
        }
    }

    fn register(cache: &'static AtomicUsize, name: &'static str) -> Option<usize> {
        if let Some(slot) = cached_slot(cache) {
            return Some(slot);
        }

        let mut names = PROBE_NAMES.lock();
        for (slot, registered_name) in names.iter().enumerate() {
            if registered_name.is_some_and(|registered_name| registered_name == name) {
                cache.store(slot + 1, Ordering::Release);
                return Some(slot);
            }
        }
        for (slot, registered_name) in names.iter_mut().enumerate() {
            if registered_name.is_none() {
                *registered_name = Some(name);
                cache.store(slot + 1, Ordering::Release);
                return Some(slot);
            }
        }
        None
    }

    /// Start timing one named probe scope.
    pub(crate) fn scope_registered(cache: &'static AtomicUsize, name: &'static str) -> ProbeGuard {
        if !enabled() {
            return inactive_guard();
        }
        let Some(slot) = register(cache, name) else {
            return inactive_guard();
        };
        ProbeGuard {
            slot,
            start_ns: get_time_ns(),
        }
    }

    /// Record an already measured elapsed interval.
    pub(crate) fn record_elapsed(slot: usize, elapsed_ns: u64) {
        if slot >= MAX_PROBES {
            return;
        }
        let probe = &PROBES[slot];
        probe.calls.fetch_add(1, Ordering::Relaxed);
        probe.total_ns.fetch_add(elapsed_ns, Ordering::Relaxed);
        let mut max = probe.max_ns.load(Ordering::Relaxed);
        while elapsed_ns > max {
            match probe.max_ns.compare_exchange_weak(
                max,
                elapsed_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => max = observed,
            }
        }
    }

    /// Render timing probe counters in a procfs-friendly table.
    pub(crate) fn render() -> String {
        let mut out = String::new();
        let _ = writeln!(&mut out, "enabled {}", if enabled() { 1 } else { 0 });
        let _ = writeln!(&mut out, "name calls total_ns avg_ns max_ns");
        let names = PROBE_NAMES.lock();
        for (slot, name) in names.iter().enumerate() {
            let Some(name) = name else {
                continue;
            };
            let probe = &PROBES[slot];
            let calls = probe.calls.load(Ordering::Relaxed);
            let total_ns = probe.total_ns.load(Ordering::Relaxed);
            let avg_ns = if calls == 0 { 0 } else { total_ns / calls };
            let max_ns = probe.max_ns.load(Ordering::Relaxed);
            let _ = writeln!(
                &mut out,
                "{} {} {} {} {}",
                name, calls, total_ns, avg_ns, max_ns
            );
        }
        out
    }
}

#[cfg(feature = "perf_probe")]
pub(crate) use enabled::{enabled, render, reset, scope_registered, set_enabled};

/// Measure the elapsed time of a block under a registered probe name.
///
/// ```ignore
/// crate::probe!({
///     add_timer_inner();
/// }, "timer.add");
/// ```
#[macro_export]
macro_rules! probe {
    ($body:block, $name:expr) => {{
        #[cfg(feature = "perf_probe")]
        {
            static PROBE_SLOT: core::sync::atomic::AtomicUsize =
                core::sync::atomic::AtomicUsize::new(0);
            let _probe_guard = $crate::perf_probe::scope_registered(&PROBE_SLOT, $name);
            $body
        }
        #[cfg(not(feature = "perf_probe"))]
        {
            $body
        }
    }};
}
