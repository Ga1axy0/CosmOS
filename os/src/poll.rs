//! Poll wait registry and keyed wakeup helpers.
//!
//! This module provides a fixed-size bitmap-based registry for `ppoll` waits:
//! - kernel-fd rows (max 128)
//! - poll-key columns (max 128)
//! - per-row interest bitmaps for POLLIN/POLLOUT
//!
//! It is intentionally crate-private and shared by syscall/timer/device paths.

use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{TaskControlBlock, WaitQueueKeyed, WaitReason};
#[cfg(feature = "io_perf_counters")]
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
#[cfg(feature = "io_perf_counters")]
use core::fmt::Write;
#[cfg(feature = "io_perf_counters")]
use core::sync::atomic::{AtomicU64, Ordering};
use lazy_static::lazy_static;

/// Readable event bit.
pub(crate) const POLLIN: u16 = 0x001;
/// Writable event bit.
pub(crate) const POLLOUT: u16 = 0x004;
/// Error event bit.
pub(crate) const POLLERR: u16 = 0x008;
/// Hangup event bit.
pub(crate) const POLLHUP: u16 = 0x010;

const MAX_KERNEL_FD: usize = 128;
const MAX_POLL_KEYS: usize = 128;

#[cfg(feature = "io_perf_counters")]
static PERF_SCAN_CALLS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_SCANNED_FDS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_READY_FDS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_FALLBACK_COUNT: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_NOTIFY_CALLS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_REGISTRY_LOCK_ACQUIRES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_REGISTRY_LOCK_WAIT_NS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_REGISTRY_LOCK_WAIT_MAX_NS: AtomicU64 = AtomicU64::new(0);

#[cfg(feature = "io_perf_counters")]
#[inline]
fn perf_inc(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "io_perf_counters")]
#[inline]
fn perf_add(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Ordering::Relaxed);
}

#[cfg(feature = "io_perf_counters")]
#[inline]
fn perf_update_max(counter: &AtomicU64, value: u64) {
    let mut current = counter.load(Ordering::Relaxed);
    while value > current {
        match counter.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(next) => current = next,
        }
    }
}

#[cfg(feature = "io_perf_counters")]
#[inline]
fn record_registry_lock_wait(start_ns: u64) {
    let wait_ns = crate::timer::get_time_ns().saturating_sub(start_ns);
    perf_inc(&PERF_REGISTRY_LOCK_ACQUIRES);
    perf_add(&PERF_REGISTRY_LOCK_WAIT_NS, wait_ns);
    perf_update_max(&PERF_REGISTRY_LOCK_WAIT_MAX_NS, wait_ns);
}

macro_rules! lock_poll_registry {
    () => {{
        #[cfg(feature = "io_perf_counters")]
        let start_ns = crate::timer::get_time_ns();
        let guard = POLL_REGISTRY.lock();
        #[cfg(feature = "io_perf_counters")]
        record_registry_lock_wait(start_ns);
        guard
    }};
}

/// Record one complete readiness scan over a userspace poll set.
#[cfg(feature = "io_perf_counters")]
#[inline]
pub(crate) fn record_scan(scanned_fds: usize, ready_fds: usize) {
    perf_inc(&PERF_SCAN_CALLS);
    perf_add(&PERF_SCANNED_FDS, scanned_fds as u64);
    perf_add(&PERF_READY_FDS, ready_fds as u64);
}

/// Compile out poll scan accounting when I/O counters are disabled.
#[cfg(not(feature = "io_perf_counters"))]
#[inline]
pub(crate) fn record_scan(_scanned_fds: usize, _ready_fds: usize) {}

/// Record one registration-capacity fallback iteration.
#[cfg(feature = "io_perf_counters")]
#[inline]
pub(crate) fn record_fallback() {
    perf_inc(&PERF_FALLBACK_COUNT);
}

/// Compile out fallback accounting when I/O counters are disabled.
#[cfg(not(feature = "io_perf_counters"))]
#[inline]
pub(crate) fn record_fallback() {}

/// Reset all poll/ppoll performance counters.
#[cfg(feature = "io_perf_counters")]
pub(crate) fn reset_perf_counters() {
    for counter in [
        &PERF_SCAN_CALLS,
        &PERF_SCANNED_FDS,
        &PERF_READY_FDS,
        &PERF_FALLBACK_COUNT,
        &PERF_NOTIFY_CALLS,
        &PERF_REGISTRY_LOCK_ACQUIRES,
        &PERF_REGISTRY_LOCK_WAIT_NS,
        &PERF_REGISTRY_LOCK_WAIT_MAX_NS,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

/// Render poll/ppoll performance counters for `/proc/io_perf`.
#[cfg(feature = "io_perf_counters")]
pub(crate) fn render_perf_counters() -> String {
    let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    let lock_acquires = load(&PERF_REGISTRY_LOCK_ACQUIRES);
    let lock_wait_ns = load(&PERF_REGISTRY_LOCK_WAIT_NS);
    let lock_wait_avg_ns = if lock_acquires == 0 {
        0
    } else {
        lock_wait_ns / lock_acquires
    };
    let mut out = String::new();
    let _ = writeln!(&mut out, "poll:");
    let _ = writeln!(&mut out, "  scan_calls {}", load(&PERF_SCAN_CALLS));
    let _ = writeln!(&mut out, "  scanned_fds {}", load(&PERF_SCANNED_FDS));
    let _ = writeln!(&mut out, "  ready_fds {}", load(&PERF_READY_FDS));
    let _ = writeln!(&mut out, "  fallback_count {}", load(&PERF_FALLBACK_COUNT));
    let _ = writeln!(&mut out, "  notify_calls {}", load(&PERF_NOTIFY_CALLS));
    let _ = writeln!(&mut out, "  registry_lock_acquires {}", lock_acquires);
    let _ = writeln!(&mut out, "  registry_lock_wait_ns {}", lock_wait_ns);
    let _ = writeln!(&mut out, "  registry_lock_wait_avg_ns {}", lock_wait_avg_ns);
    let _ = writeln!(
        &mut out,
        "  registry_lock_wait_max_ns {}",
        load(&PERF_REGISTRY_LOCK_WAIT_MAX_NS)
    );
    out
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PollKeyState {
    Free,
    Active,
    Ready,
    TimedOut,
}

#[derive(Clone, Copy, Debug)]
struct KernelFdSlot {
    active: bool,
    generation: u16,
    owner_pid: usize,
    owner_fd: usize,
    source_id: usize,
    key_bits: u128,
    key_bits_in: u128,
    key_bits_out: u128,
}

impl Default for KernelFdSlot {
    fn default() -> Self {
        Self {
            active: false,
            generation: 0,
            owner_pid: 0,
            owner_fd: 0,
            source_id: 0,
            key_bits: 0,
            key_bits_in: 0,
            key_bits_out: 0,
        }
    }
}

impl KernelFdSlot {
    const EMPTY: Self = Self {
        active: false,
        generation: 0,
        owner_pid: 0,
        owner_fd: 0,
        source_id: 0,
        key_bits: 0,
        key_bits_in: 0,
        key_bits_out: 0,
    };
}

#[derive(Clone, Copy, Debug)]
struct PollKeySlot {
    generation: u8,
    state: PollKeyState,
    task_ptr: usize,
    owner_pid: usize,
    rows_mask: u128,
}

impl Default for PollKeySlot {
    fn default() -> Self {
        Self {
            generation: 0,
            state: PollKeyState::Free,
            task_ptr: 0,
            owner_pid: 0,
            rows_mask: 0,
        }
    }
}

impl PollKeySlot {
    const EMPTY: Self = Self {
        generation: 0,
        state: PollKeyState::Free,
        task_ptr: 0,
        owner_pid: 0,
        rows_mask: 0,
    };
}

#[derive(Debug)]
struct PollRegistry {
    kernel_slots: [KernelFdSlot; MAX_KERNEL_FD],
    key_slots: [PollKeySlot; MAX_POLL_KEYS],
    next_kernel_fd: usize,
    next_key: usize,
}

impl PollRegistry {
    const fn new() -> Self {
        Self {
            kernel_slots: [KernelFdSlot::EMPTY; MAX_KERNEL_FD],
            key_slots: [PollKeySlot::EMPTY; MAX_POLL_KEYS],
            next_kernel_fd: 0,
            next_key: 0,
        }
    }

    fn alloc_key(&mut self, task_ptr: usize, owner_pid: usize) -> Result<PollWaitHandle, ERRNO> {
        for off in 0..MAX_POLL_KEYS {
            let idx = (self.next_key + off) % MAX_POLL_KEYS;
            if !matches!(self.key_slots[idx].state, PollKeyState::Free) {
                continue;
            }
            let slot = &mut self.key_slots[idx];
            slot.generation = slot.generation.wrapping_add(1);
            slot.state = PollKeyState::Active;
            slot.task_ptr = task_ptr;
            slot.owner_pid = owner_pid;
            slot.rows_mask = 0;
            self.next_key = (idx + 1) % MAX_POLL_KEYS;
            return Ok(PollWaitHandle {
                key_idx: idx as u8,
                key_generation: slot.generation,
            });
        }
        Err(ERRNO::ENOSPC)
    }

    fn find_or_alloc_kernel_fd(
        &mut self,
        pid: usize,
        fd: usize,
        source_id: usize,
    ) -> Result<usize, ERRNO> {
        for (idx, slot) in self.kernel_slots.iter().enumerate() {
            if slot.active
                && slot.owner_pid == pid
                && slot.owner_fd == fd
                && slot.source_id == source_id
            {
                return Ok(idx);
            }
        }

        for off in 0..MAX_KERNEL_FD {
            let idx = (self.next_kernel_fd + off) % MAX_KERNEL_FD;
            if self.kernel_slots[idx].active {
                continue;
            }
            let slot = &mut self.kernel_slots[idx];
            slot.active = true;
            slot.generation = slot.generation.wrapping_add(1);
            slot.owner_pid = pid;
            slot.owner_fd = fd;
            slot.source_id = source_id;
            slot.key_bits = 0;
            slot.key_bits_in = 0;
            slot.key_bits_out = 0;
            self.next_kernel_fd = (idx + 1) % MAX_KERNEL_FD;
            return Ok(idx);
        }

        Err(ERRNO::ENOSPC)
    }

    fn key_valid(&self, handle: PollWaitHandle) -> bool {
        let idx = handle.key_idx as usize;
        let slot = &self.key_slots[idx];
        !matches!(slot.state, PollKeyState::Free) && slot.generation == handle.key_generation
    }

    fn clear_key_rows(&mut self, handle: PollWaitHandle) {
        let key_idx = handle.key_idx as usize;
        let key_bit = key_bit(key_idx);
        let rows_mask = self.key_slots[key_idx].rows_mask;
        for row in 0..MAX_KERNEL_FD {
            let row_mask = row_bit(row);
            if (rows_mask & row_mask) == 0 {
                continue;
            }
            let slot = &mut self.kernel_slots[row];
            slot.key_bits &= !key_bit;
            slot.key_bits_in &= !key_bit;
            slot.key_bits_out &= !key_bit;
            if slot.key_bits == 0 {
                slot.active = false;
            }
        }
        self.key_slots[key_idx].rows_mask = 0;
    }

    fn cleanup_key(&mut self, handle: PollWaitHandle) {
        if !self.key_valid(handle) {
            return;
        }
        self.clear_key_rows(handle);
        let key_idx = handle.key_idx as usize;
        let slot = &mut self.key_slots[key_idx];
        slot.state = PollKeyState::Free;
        slot.task_ptr = 0;
        slot.owner_pid = 0;
    }
}

lazy_static! {
    static ref POLL_WAIT_QUEUE: WaitQueueKeyed<u16> = WaitQueueKeyed::new();
}

// Use const static initialization for registry to avoid a large lazy-init stack
// frame in `spin::once` (can overflow small kernel stacks on early IRQ paths).
static POLL_REGISTRY: SpinNoIrqLock<PollRegistry> = SpinNoIrqLock::new(PollRegistry::new());

#[inline]
fn row_bit(row: usize) -> u128 {
    1u128 << row
}

#[inline]
fn key_bit(key_idx: usize) -> u128 {
    1u128 << key_idx
}

#[inline]
fn encode_wait_key(key_idx: u8, key_generation: u8) -> u16 {
    ((key_generation as u16) << 8) | (key_idx as u16)
}

/// Opaque handle for one in-flight `ppoll` wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PollWaitHandle {
    key_idx: u8,
    key_generation: u8,
}

impl PollWaitHandle {
    /// Encoded wait-queue key (`generation << 8 | index`).
    pub(crate) fn wait_key(self) -> u16 {
        encode_wait_key(self.key_idx, self.key_generation)
    }

    /// Timeout tag consumed by timer path.
    pub(crate) fn timer_tag(self) -> PollTimerTag {
        PollTimerTag {
            key_idx: self.key_idx,
            key_generation: self.key_generation,
        }
    }
}

/// Timeout identity attached to timer heap entries created by `ppoll`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PollTimerTag {
    key_idx: u8,
    key_generation: u8,
}

/// Observable state of a poll wait key after wakeup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PollWakeState {
    /// Triggered by fd readiness notification.
    Ready,
    /// Triggered by timeout.
    TimedOut,
    /// Key no longer valid (already cleaned or generation mismatch).
    Canceled,
}

/// Register one poll wait key and attach interested `(fd, source_id, events)` rows.
///
/// `pid` + `fd` disambiguates per-process fd namespace;
/// `source_id` identifies the underlying readiness source.
pub(crate) fn register_poll_wait(
    pid: usize,
    task: &Arc<TaskControlBlock>,
    interests: &[(usize, usize, u16)],
) -> Result<PollWaitHandle, ERRNO> {
    let task_ptr = Arc::as_ptr(task) as usize;
    let mut registry = lock_poll_registry!();
    let handle = registry.alloc_key(task_ptr, pid)?;
    let key_idx = handle.key_idx as usize;
    let key_bit = key_bit(key_idx);

    // debug!("register_poll_wait: pid={}, handle={:?}, interests={:?}", pid, handle, interests);

    for &(fd, source_id, events) in interests {
        let row = match registry.find_or_alloc_kernel_fd(pid, fd, source_id) {
            Ok(row) => row,
            Err(e) => {
                registry.cleanup_key(handle);
                return Err(e);
            }
        };
        let row_slot = &mut registry.kernel_slots[row];
        row_slot.key_bits |= key_bit;
        if (events & POLLIN) != 0 {
            row_slot.key_bits_in |= key_bit;
        }
        if (events & POLLOUT) != 0 {
            row_slot.key_bits_out |= key_bit;
        }
        registry.key_slots[key_idx].rows_mask |= row_bit(row);
    }

    Ok(handle)
}

/// Remove all bitmap registrations bound to this wait key and free the key slot.
pub(crate) fn cleanup_poll_wait(handle: PollWaitHandle) {
    lock_poll_registry!().cleanup_key(handle);
}

/// Check whether wait should be skipped because key has already been triggered.
pub(crate) fn poll_wait_should_skip(handle: PollWaitHandle) -> bool {
    let registry = lock_poll_registry!();
    if !registry.key_valid(handle) {
        return true;
    }
    let state = registry.key_slots[handle.key_idx as usize].state;
    !matches!(state, PollKeyState::Active)
}

/// Query current wake state for this wait key.
pub(crate) fn poll_wait_state(handle: PollWaitHandle) -> PollWakeState {
    let registry = lock_poll_registry!();
    if !registry.key_valid(handle) {
        return PollWakeState::Canceled;
    }
    match registry.key_slots[handle.key_idx as usize].state {
        PollKeyState::TimedOut => PollWakeState::TimedOut,
        PollKeyState::Ready => PollWakeState::Ready,
        PollKeyState::Active => PollWakeState::Canceled,
        PollKeyState::Free => PollWakeState::Canceled,
    }
}

/// Block on global poll wait queue with race-safe skip recheck.
pub(crate) fn wait_poll_key(handle: PollWaitHandle) {
    let wait_key = handle.wait_key();
    POLL_WAIT_QUEUE.wait_selected_with_reason_or_skip(wait_key, WaitReason::Poll, || {
        poll_wait_should_skip(handle)
    });
}

/// Notify readiness for a source id and wake interested wait keys.
pub(crate) fn notify_poll_source(source_id: usize, ready_mask: u16) {
    // debug!("notify_poll_source: source_id={}, ready_mask={:#x}", source_id, ready_mask);
    #[cfg(feature = "io_perf_counters")]
    perf_inc(&PERF_NOTIFY_CALLS);
    let mut wait_keys = Vec::new();
    {
        let mut registry = lock_poll_registry!();
        let mut wake_bits = 0u128;

        for row in 0..MAX_KERNEL_FD {
            let row_slot = &registry.kernel_slots[row];
            if !row_slot.active || row_slot.source_id != source_id {
                continue;
            }
            let mut row_wake = 0u128;
            if (ready_mask & POLLIN) != 0 {
                row_wake |= row_slot.key_bits_in;
            }
            if (ready_mask & POLLOUT) != 0 {
                row_wake |= row_slot.key_bits_out;
            }
            if (ready_mask & (POLLERR | POLLHUP)) != 0 {
                row_wake |= row_slot.key_bits;
            }
            wake_bits |= row_wake;
        }

        for key_idx in 0..MAX_POLL_KEYS {
            if (wake_bits & key_bit(key_idx)) == 0 {
                continue;
            }
            let slot = &mut registry.key_slots[key_idx];
            if !matches!(slot.state, PollKeyState::Active) {
                continue;
            }
            slot.state = PollKeyState::Ready;
            wait_keys.push(encode_wait_key(key_idx as u8, slot.generation));
        }
    }

    for key in wait_keys {
        POLL_WAIT_QUEUE.wake_selected(key);
    }

    // Persistent epoll subscriptions are indexed by source id and therefore
    // do not participate in the transient poll registry's full row scan.
    crate::fs::epoll::notify_source(source_id, ready_mask);
}

/// Notify pending signal delivery for a process and wake all active poll waiters of that pid.
pub(crate) fn notify_poll_signal_pid(pid: usize) {
    debug!("notify_poll_signal_pid: pid={}", pid);
    let mut wait_keys = Vec::new();
    {
        let mut registry = lock_poll_registry!();
        for key_idx in 0..MAX_POLL_KEYS {
            let slot = &mut registry.key_slots[key_idx];
            if slot.owner_pid != pid || !matches!(slot.state, PollKeyState::Active) {
                continue;
            }
            slot.state = PollKeyState::Ready;
            wait_keys.push(encode_wait_key(key_idx as u8, slot.generation));
        }
    }

    for key in wait_keys {
        POLL_WAIT_QUEUE.wake_selected(key);
    }
}

/// Check whether a task currently has an in-flight keyed poll wait entry.
pub(crate) fn task_has_inflight_keyed_poll_wait(task: &Arc<TaskControlBlock>) -> bool {
    let task_ptr = Arc::as_ptr(task) as usize;
    let registry = lock_poll_registry!();
    registry
        .key_slots
        .iter()
        .any(|slot| slot.task_ptr == task_ptr && !matches!(slot.state, PollKeyState::Free))
}

/// Timer callback for poll timeout entries.
///
/// Returns `true` when the timer entry should be popped from heap.
pub(crate) fn handle_poll_timeout(tag: PollTimerTag, task: &Arc<TaskControlBlock>) -> bool {
    let handle = PollWaitHandle {
        key_idx: tag.key_idx,
        key_generation: tag.key_generation,
    };
    let wait_key = {
        let mut registry = lock_poll_registry!();
        if !registry.key_valid(handle) {
            return true;
        }
        let key_idx = handle.key_idx as usize;
        let generation = {
            let slot = &mut registry.key_slots[key_idx];
            if slot.task_ptr != (Arc::as_ptr(task) as usize) {
                return true;
            }
            if matches!(slot.state, PollKeyState::Ready) {
                return true;
            }
            if !matches!(slot.state, PollKeyState::Active) {
                return true;
            }
            slot.state = PollKeyState::TimedOut;
            slot.generation
        };
        registry.clear_key_rows(handle);
        encode_wait_key(handle.key_idx, generation)
    };

    POLL_WAIT_QUEUE.wake_selected(wait_key);
    true
}
