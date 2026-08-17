//! Poll wait registration and source-indexed wakeup helpers.
//!
//! The readiness scan still follows the usual `poll(2)` model: userspace
//! supplies an array of descriptors and the kernel scans it for readiness.
//! The blocking part uses a Linux-like source-indexed wait-list model. Each
//! in-flight poll has one dynamic wait id, and each readiness source keeps a
//! list of the poll waits interested in that source. A source notification
//! therefore visits only its subscribers instead of scanning a fixed global
//! fd-by-key bitmap.
//!
//! This is intentionally crate-private and shared by syscall/timer/device
//! paths. The source lists are a kernel-side equivalent of a driver's wait
//! queue; the existing `File::poll_source_id()` API lets us apply the model
//! without changing every file implementation at once.

use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{TaskControlBlock, WaitQueueKeyed, WaitReason};
use alloc::collections::BTreeMap;
#[cfg(feature = "io_perf_counters")]
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
#[cfg(feature = "io_perf_counters")]
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use lazy_static::lazy_static;

/// Readable event bit.
pub(crate) const POLLIN: u16 = 0x001;
/// Writable event bit.
pub(crate) const POLLOUT: u16 = 0x004;
/// Error event bit.
pub(crate) const POLLERR: u16 = 0x008;
/// Hangup event bit.
pub(crate) const POLLHUP: u16 = 0x010;

const POLL_STATE_ACTIVE: u8 = 0;
const POLL_STATE_READY: u8 = 1;
const POLL_STATE_TIMED_OUT: u8 = 2;
const POLL_STATE_CANCELED: u8 = 3;

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
static PERF_SOURCE_LOOKUPS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_SOURCE_SUBSCRIBERS_SCANNED: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_SOURCE_WAITERS_WOKEN: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_REGISTRY_LOCK_ACQUIRES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_REGISTRY_LOCK_WAIT_NS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_REGISTRY_LOCK_WAIT_MAX_NS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_SOURCE_INDEX_LOCK_ACQUIRES: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_SOURCE_INDEX_LOCK_WAIT_NS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_SOURCE_INDEX_LOCK_WAIT_MAX_NS: AtomicU64 = AtomicU64::new(0);

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

#[cfg(feature = "io_perf_counters")]
#[inline]
fn record_source_index_lock_wait(start_ns: u64) {
    let wait_ns = crate::timer::get_time_ns().saturating_sub(start_ns);
    perf_inc(&PERF_SOURCE_INDEX_LOCK_ACQUIRES);
    perf_add(&PERF_SOURCE_INDEX_LOCK_WAIT_NS, wait_ns);
    perf_update_max(&PERF_SOURCE_INDEX_LOCK_WAIT_MAX_NS, wait_ns);
}

macro_rules! lock_poll_waits {
    () => {{
        #[cfg(feature = "io_perf_counters")]
        let start_ns = crate::timer::get_time_ns();
        let guard = POLL_WAITS.lock();
        #[cfg(feature = "io_perf_counters")]
        record_registry_lock_wait(start_ns);
        guard
    }};
}

macro_rules! lock_source_index {
    () => {{
        #[cfg(feature = "io_perf_counters")]
        let start_ns = crate::timer::get_time_ns();
        let guard = SOURCE_WAITERS.lock();
        #[cfg(feature = "io_perf_counters")]
        record_source_index_lock_wait(start_ns);
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
        &PERF_SOURCE_LOOKUPS,
        &PERF_SOURCE_SUBSCRIBERS_SCANNED,
        &PERF_SOURCE_WAITERS_WOKEN,
        &PERF_REGISTRY_LOCK_ACQUIRES,
        &PERF_REGISTRY_LOCK_WAIT_NS,
        &PERF_REGISTRY_LOCK_WAIT_MAX_NS,
        &PERF_SOURCE_INDEX_LOCK_ACQUIRES,
        &PERF_SOURCE_INDEX_LOCK_WAIT_NS,
        &PERF_SOURCE_INDEX_LOCK_WAIT_MAX_NS,
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
    let source_index_lock_acquires = load(&PERF_SOURCE_INDEX_LOCK_ACQUIRES);
    let source_index_lock_wait_ns = load(&PERF_SOURCE_INDEX_LOCK_WAIT_NS);
    let source_index_lock_wait_avg_ns = if source_index_lock_acquires == 0 {
        0
    } else {
        source_index_lock_wait_ns / source_index_lock_acquires
    };
    let mut out = String::new();
    let _ = writeln!(&mut out, "poll:");
    let _ = writeln!(&mut out, "  scan_calls {}", load(&PERF_SCAN_CALLS));
    let _ = writeln!(&mut out, "  scanned_fds {}", load(&PERF_SCANNED_FDS));
    let _ = writeln!(&mut out, "  ready_fds {}", load(&PERF_READY_FDS));
    let _ = writeln!(&mut out, "  fallback_count {}", load(&PERF_FALLBACK_COUNT));
    let _ = writeln!(&mut out, "  notify_calls {}", load(&PERF_NOTIFY_CALLS));
    let _ = writeln!(&mut out, "  source_lookups {}", load(&PERF_SOURCE_LOOKUPS));
    let _ = writeln!(
        &mut out,
        "  source_subscribers_scanned {}",
        load(&PERF_SOURCE_SUBSCRIBERS_SCANNED)
    );
    let _ = writeln!(
        &mut out,
        "  source_waiters_woken {}",
        load(&PERF_SOURCE_WAITERS_WOKEN)
    );
    // Keep the old field names for procfs consumers. They now measure the
    // dynamic in-flight wait map rather than a fixed bitmap registry.
    let _ = writeln!(&mut out, "  registry_lock_acquires {}", lock_acquires);
    let _ = writeln!(&mut out, "  registry_lock_wait_ns {}", lock_wait_ns);
    let _ = writeln!(&mut out, "  registry_lock_wait_avg_ns {}", lock_wait_avg_ns);
    let _ = writeln!(
        &mut out,
        "  registry_lock_wait_max_ns {}",
        load(&PERF_REGISTRY_LOCK_WAIT_MAX_NS)
    );
    let _ = writeln!(
        &mut out,
        "  source_index_lock_acquires {}",
        source_index_lock_acquires
    );
    let _ = writeln!(
        &mut out,
        "  source_index_lock_wait_ns {}",
        source_index_lock_wait_ns
    );
    let _ = writeln!(
        &mut out,
        "  source_index_lock_wait_avg_ns {}",
        source_index_lock_wait_avg_ns
    );
    let _ = writeln!(
        &mut out,
        "  source_index_lock_wait_max_ns {}",
        load(&PERF_SOURCE_INDEX_LOCK_WAIT_MAX_NS)
    );
    out
}

/// State stored once per in-flight `ppoll` call.
struct PollWait {
    id: u64,
    task_ptr: usize,
    owner_pid: usize,
    state: AtomicU8,
    sources: Vec<usize>,
}

impl PollWait {
    #[inline]
    fn state(&self) -> u8 {
        self.state.load(Ordering::Acquire)
    }

    #[inline]
    fn is_active(&self) -> bool {
        self.state() == POLL_STATE_ACTIVE
    }

    #[inline]
    fn mark_ready(&self) -> bool {
        self.state
            .compare_exchange(
                POLL_STATE_ACTIVE,
                POLL_STATE_READY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    #[inline]
    fn mark_timed_out(&self) -> bool {
        self.state
            .compare_exchange(
                POLL_STATE_ACTIVE,
                POLL_STATE_TIMED_OUT,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

struct PollSubscription {
    wait: Weak<PollWait>,
    events: u16,
}

struct SourceWaiters {
    subscribers: SpinNoIrqLock<Vec<PollSubscription>>,
}

impl SourceWaiters {
    fn new() -> Self {
        Self {
            subscribers: SpinNoIrqLock::new(Vec::new()),
        }
    }
}

lazy_static! {
    /// The keyed queue only stores tasks that are actually sleeping. Its
    /// u64 key has no fixed 128-waiter capacity.
    static ref POLL_WAIT_QUEUE: WaitQueueKeyed<u64> = WaitQueueKeyed::new();
    /// Dynamic lifetime/index map for in-flight waits. It is not traversed by
    /// ordinary source notifications; it only validates handles and handles
    /// task/timer cleanup.
    static ref POLL_WAITS: SpinNoIrqLock<BTreeMap<u64, Arc<PollWait>>> =
        SpinNoIrqLock::new(BTreeMap::new());
    /// Source id -> subscribers, analogous to a driver's wait queue indexed
    /// by the readiness source.
    static ref SOURCE_WAITERS: SpinNoIrqLock<BTreeMap<usize, Arc<SourceWaiters>>> =
        SpinNoIrqLock::new(BTreeMap::new());
}

static NEXT_POLL_WAIT_ID: AtomicU64 = AtomicU64::new(1);

#[inline]
fn next_poll_wait_id() -> u64 {
    loop {
        let id = NEXT_POLL_WAIT_ID.fetch_add(1, Ordering::Relaxed);
        if id != 0 {
            return id;
        }
    }
}

fn source_bucket(source_id: usize) -> Arc<SourceWaiters> {
    let mut buckets = lock_source_index!();
    buckets
        .entry(source_id)
        .or_insert_with(|| Arc::new(SourceWaiters::new()))
        .clone()
}

fn existing_source_bucket(source_id: usize) -> Option<Arc<SourceWaiters>> {
    lock_source_index!().get(&source_id).cloned()
}

/// Drop empty source buckets when no concurrent user still holds the bucket.
/// The strong-count check prevents removing a bucket that another registrar
/// already cloned but has not yet appended its subscription to.
fn maybe_remove_source_bucket(source_id: usize, bucket: &Arc<SourceWaiters>) {
    let mut buckets = lock_source_index!();
    let Some(current) = buckets.get(&source_id) else {
        return;
    };
    if !Arc::ptr_eq(current, bucket) || Arc::strong_count(bucket) != 2 {
        return;
    }
    if !bucket.subscribers.lock().is_empty() {
        return;
    }
    buckets.remove(&source_id);
}

fn detach_wait_from_sources(wait: &PollWait) {
    for &source_id in &wait.sources {
        let Some(bucket) = existing_source_bucket(source_id) else {
            continue;
        };
        let mut subscribers = bucket.subscribers.lock();
        subscribers.retain(|subscription| {
            subscription
                .wait
                .upgrade()
                .is_some_and(|other| other.id != wait.id)
        });
        let empty = subscribers.is_empty();
        drop(subscribers);
        if empty {
            maybe_remove_source_bucket(source_id, &bucket);
        }
    }
}

#[inline]
fn source_matches(events: u16, ready_mask: u16) -> bool {
    ((ready_mask & POLLIN) != 0 && (events & POLLIN) != 0)
        || ((ready_mask & POLLOUT) != 0 && (events & POLLOUT) != 0)
        // Keep the existing behavior: error/hangup wakes a registered fd
        // even when its requested event mask is empty.
        || (ready_mask & (POLLERR | POLLHUP)) != 0
}

/// Opaque handle for one in-flight `ppoll` wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PollWaitHandle {
    wait_id: u64,
}

impl PollWaitHandle {
    /// Dynamic wait-queue key.
    pub(crate) fn wait_key(self) -> u64 {
        self.wait_id
    }

    /// Timeout tag consumed by timer path.
    pub(crate) fn timer_tag(self) -> PollTimerTag {
        PollTimerTag {
            wait_id: self.wait_id,
        }
    }
}

/// Timeout identity attached to timer heap entries created by `ppoll`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PollTimerTag {
    wait_id: u64,
}

/// Observable state of a poll wait key after wakeup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PollWakeState {
    /// Triggered by fd readiness notification.
    Ready,
    /// Triggered by timeout.
    TimedOut,
    /// Key no longer valid (already cleaned or unknown).
    Canceled,
}

/// Register one poll wait and attach interested `(fd, source_id, events)`
/// subscriptions. The syscall keeps the fd in its own readiness scan and the
/// transient wakeup index is keyed by `source_id`.
pub(crate) fn register_poll_wait(
    pid: usize,
    task: &Arc<TaskControlBlock>,
    interests: &[(usize, usize, u16)],
) -> Result<PollWaitHandle, ERRNO> {
    let mut sources = Vec::new();
    sources
        .try_reserve(interests.len())
        .map_err(|_| ERRNO::ENOMEM)?;
    for &(_, source_id, _) in interests {
        sources.push(source_id);
    }
    sources.sort_unstable();
    sources.dedup();

    let wait_id = next_poll_wait_id();
    let wait = Arc::new(PollWait {
        id: wait_id,
        task_ptr: Arc::as_ptr(task) as usize,
        owner_pid: pid,
        state: AtomicU8::new(POLL_STATE_ACTIVE),
        sources,
    });

    // Publish weak source subscriptions before publishing the lifetime map.
    // A notification in this small interval marks the wait READY; the
    // syscall's post-registration scan then observes the same readiness.
    for &(_, source_id, events) in interests {
        source_bucket(source_id)
            .subscribers
            .lock()
            .push(PollSubscription {
                wait: Arc::downgrade(&wait),
                events,
            });
    }
    lock_poll_waits!().insert(wait_id, wait);

    Ok(PollWaitHandle { wait_id })
}

/// Remove all source subscriptions bound to this wait id and free its state.
pub(crate) fn cleanup_poll_wait(handle: PollWaitHandle) {
    let wait = lock_poll_waits!().remove(&handle.wait_id);
    let Some(wait) = wait else {
        return;
    };
    wait.state.store(POLL_STATE_CANCELED, Ordering::Release);
    detach_wait_from_sources(&wait);
    // Normally the syscall has already returned from the keyed queue. This
    // also removes a waiter if an exit path races with the normal cleanup.
    POLL_WAIT_QUEUE.wake_selected(handle.wait_key());
}

/// Remove every transient poll registration owned by `task`.
pub(crate) fn cleanup_poll_wait_for_task(task: &Arc<TaskControlBlock>) {
    let task_ptr = Arc::as_ptr(task) as usize;
    let waits = {
        let waits = lock_poll_waits!();
        waits
            .values()
            .filter(|wait| wait.task_ptr == task_ptr)
            .cloned()
            .collect::<Vec<_>>()
    };
    for wait in waits {
        cleanup_poll_wait(PollWaitHandle { wait_id: wait.id });
    }
}

/// Check whether wait should be skipped because it has already triggered.
pub(crate) fn poll_wait_should_skip(handle: PollWaitHandle) -> bool {
    let wait = lock_poll_waits!().get(&handle.wait_id).cloned();
    wait.is_none_or(|wait| wait.state() != POLL_STATE_ACTIVE)
}

/// Query current wake state for this wait.
pub(crate) fn poll_wait_state(handle: PollWaitHandle) -> PollWakeState {
    let Some(wait) = lock_poll_waits!().get(&handle.wait_id).cloned() else {
        return PollWakeState::Canceled;
    };
    match wait.state() {
        POLL_STATE_TIMED_OUT => PollWakeState::TimedOut,
        POLL_STATE_READY => PollWakeState::Ready,
        POLL_STATE_ACTIVE | POLL_STATE_CANCELED => PollWakeState::Canceled,
        _ => PollWakeState::Canceled,
    }
}

/// Block on the keyed poll wait queue with race-safe skip recheck.
pub(crate) fn wait_poll_key(handle: PollWaitHandle) {
    POLL_WAIT_QUEUE.wait_selected_with_reason_or_skip(handle.wait_key(), WaitReason::Poll, || {
        poll_wait_should_skip(handle)
    });
}

/// Notify readiness for a source id and wake only its interested waiters.
pub(crate) fn notify_poll_source(source_id: usize, ready_mask: u16) {
    #[cfg(feature = "io_perf_counters")]
    perf_inc(&PERF_NOTIFY_CALLS);

    let bucket = {
        #[cfg(feature = "io_perf_counters")]
        perf_inc(&PERF_SOURCE_LOOKUPS);
        lock_source_index!().get(&source_id).cloned()
    };
    if let Some(bucket) = bucket {
        let mut wait_keys = Vec::new();
        let mut subscribers = bucket.subscribers.lock();
        #[cfg(feature = "io_perf_counters")]
        perf_add(&PERF_SOURCE_SUBSCRIBERS_SCANNED, subscribers.len() as u64);
        subscribers.retain(|subscription| {
            let Some(wait) = subscription.wait.upgrade() else {
                return false;
            };
            if !wait.is_active() {
                return false;
            }
            if source_matches(subscription.events, ready_mask) && wait.mark_ready() {
                wait_keys.push(wait.id);
            }
            true
        });
        let empty = subscribers.is_empty();
        drop(subscribers);
        if empty {
            maybe_remove_source_bucket(source_id, &bucket);
        }

        #[cfg(feature = "io_perf_counters")]
        perf_add(&PERF_SOURCE_WAITERS_WOKEN, wait_keys.len() as u64);
        for wait_key in wait_keys {
            POLL_WAIT_QUEUE.wake_selected(wait_key);
        }
    }

    // Persistent epoll subscriptions are indexed by source id and remain
    // separate from this transient poll wait list.
    crate::fs::epoll::notify_source(source_id, ready_mask);
}

/// Notify pending signal delivery for a process and wake all active poll waits.
pub(crate) fn notify_poll_signal_pid(pid: usize) {
    debug!("notify_poll_signal_pid: pid={}", pid);
    let mut wait_keys = Vec::new();
    {
        let waits = lock_poll_waits!();
        for wait in waits.values() {
            if wait.owner_pid == pid && wait.mark_ready() {
                wait_keys.push(wait.id);
            }
        }
    }
    for wait_key in wait_keys {
        POLL_WAIT_QUEUE.wake_selected(wait_key);
    }
}

/// Check whether a task currently has an in-flight keyed poll wait entry.
pub(crate) fn task_has_inflight_keyed_poll_wait(task: &Arc<TaskControlBlock>) -> bool {
    let task_ptr = Arc::as_ptr(task) as usize;
    let waits = lock_poll_waits!();
    waits
        .values()
        .any(|wait| wait.task_ptr == task_ptr && wait.state() != POLL_STATE_CANCELED)
}

/// Timer callback for poll timeout entries.
///
/// Returns `true` when the timer entry should be popped from heap.
pub(crate) fn handle_poll_timeout(tag: PollTimerTag, task: &Arc<TaskControlBlock>) -> bool {
    let handle = PollWaitHandle {
        wait_id: tag.wait_id,
    };
    let Some(wait) = lock_poll_waits!().get(&handle.wait_id).cloned() else {
        return true;
    };
    if wait.task_ptr != Arc::as_ptr(task) as usize || !wait.mark_timed_out() {
        return true;
    }

    detach_wait_from_sources(&wait);
    POLL_WAIT_QUEUE.wake_selected(handle.wait_key());
    true
}
