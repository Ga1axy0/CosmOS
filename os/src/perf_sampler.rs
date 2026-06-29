//! Periodic memory-pressure / system-state sampler.
//!
//! The kernel's memory state drifts over a long run — most importantly the
//! kernel heap ([`KERNEL_HEAP_BYTES`]) is a high-water-mark allocator that never
//! returns pages, so it can only grow. As it eats into the page-cache budget the
//! reclaim path runs more often (and more expensively), and the whole system
//! slows down. This module captures that *trajectory* rather than a point
//! snapshot: on the timer-tick path we snapshot a handful of cross-subsystem
//! counters once per second into a bounded FIFO ring buffer, and expose the
//! history through `/proc/mm_perf` (see `fs::procfs`). Reading the file is all
//! that's needed to watch the trend:
//!
//! ```sh
//! cat /proc/mm_perf          # dump the buffered trajectory
//! echo 1 > /proc/mm_perf     # clear history and start fresh
//! ```
//!
//! Everything here is gated behind the `mm_perf_counters` cargo feature. With it
//! disabled the tick hook compiles away to nothing.

use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

use alloc::string::String;

use lazy_static::lazy_static;

use crate::config::CLOCK_FREQ;
use crate::fs::{
    dentry_perf_counters, getdents_perf_counters, inode_perf_counters, page_cache_stats,
};
use crate::mm::{frame_allocator_stats, KERNEL_HEAP_BYTES, KERNEL_HEAP_USED_BYTES};
use crate::net;
use crate::sched::list_pids;
use crate::sync::SpinNoIrqLock;

/// How many timer ticks elapse between samples. [`TICKS_PER_SEC`](crate::timer::TICKS_PER_SEC)
/// is 100, so 100 ticks ≈ one sample per second.
const SAMPLE_INTERVAL_TICKS: usize = 100;

/// Number of historical samples retained in the FIFO ring (oldest evicted
/// first). 2048 samples at 1 Hz ≈ 34 minutes of history.
const HISTORY_LEN: usize = 2048;

/// One point-in-time snapshot of the counters that drift over a long run.
/// Every field is a raw machine value (bytes / pages / counts) so the rendered
/// table stays trivially `awk`-parseable.
#[derive(Clone, Copy, Default)]
pub struct MmSample {
    /// Monotonic uptime in raw timer ticks (== `get_time()`).
    pub tick: usize,
    /// `KERNEL_HEAP_BYTES` — the high-water mark of the kernel heap. Never
    /// decreases; if this climbs while [`heap_used_bytes`](MmSample::heap_used_bytes)
    /// stays low, the heap is bloating / leaking.
    pub heap_total_bytes: usize,
    /// `KERNEL_HEAP_USED_BYTES` — live demand on the heap.
    pub heap_used_bytes: usize,
    /// `heap_total_bytes - heap_used_bytes` — heap capacity not currently used.
    pub heap_slack_bytes: usize,
    /// Free physical frames in the buddy allocator.
    pub free_pages: usize,
    /// Allocated physical frames.
    pub allocated_pages: usize,
    /// Total managed physical frames.
    pub total_pages: usize,
    /// Monotonic OOM event counter.
    pub oom_count: usize,
    /// Pages currently held in the page cache.
    pub cached_pages: usize,
    /// Page-cache reclaim start threshold (pages).
    pub high_watermark: usize,
    /// Page-cache reclaim stop threshold (pages).
    pub low_watermark: usize,
    /// Number of entries in the page-cache inactive queue.
    pub page_cache_inactive: usize,
    /// Number of live page-cache mappings (roughly, cached-file working set).
    pub page_cache_mappings: usize,
    /// Number of live dentry-cache entries.
    pub dentry_entries: usize,
    /// Number of queued dentry-cache inactive entries.
    pub dentry_inactive: usize,
    /// Number of live inode-cache entries.
    pub inode_entries: usize,
    /// Number of queued inode-cache inactive entries.
    pub inode_inactive: usize,
    /// Number of live processes (`PID2PCB` size).
    pub nproc: usize,
    /// Number of live TCP socket states.
    pub ntcp: usize,
    /// Number of live UDP socket states.
    pub nudp: usize,
    /// Cumulative `getdents64` calls since boot.
    pub getdents_calls: usize,
    /// Cumulative bytes returned by `getdents64`.
    pub getdents_bytes: usize,
    /// Cumulative `getdents64` kernel time in microseconds.
    pub getdents_total_us: usize,
    /// Number of directory snapshot rebuilds (`inode.ls()` full walks).
    pub dir_snapshot_calls: usize,
    /// Total directory entries observed across those rebuilds.
    pub dir_snapshot_entries: usize,
    /// Cumulative directory snapshot rebuild time in microseconds.
    pub dir_snapshot_total_us: usize,
}

struct Ring {
    buf: [MmSample; HISTORY_LEN],
    head: usize, // next slot to write (== oldest slot once full)
    len: usize,  // number of valid samples, capped at HISTORY_LEN
}

impl Ring {
    fn push(&mut self, s: MmSample) {
        self.buf[self.head] = s;
        self.head = (self.head + 1) % HISTORY_LEN;
        if self.len < HISTORY_LEN {
            self.len += 1;
        }
    }
}

lazy_static! {
    static ref RING: SpinNoIrqLock<Ring> = SpinNoIrqLock::new(Ring {
        buf: [MmSample::default(); HISTORY_LEN],
        head: 0,
        len: 0,
    });
}

/// Ticks accumulated since the last sample. Only ever touched by hart 0 (see
/// [`on_tick`]'s hart guard), so a single relaxed atomic is sufficient.
static TICKS_SINCE_SAMPLE: AtomicUsize = AtomicUsize::new(0);

/// Read every counter into a fresh [`MmSample`].
///
/// Each lock here is taken and released independently (no cross-subsystem lock
/// nesting), so this is safe to call from the hard-IRQ timer context just like
/// `net::poll()`.
fn collect_sample(now_tick: usize) -> MmSample {
    let stats = frame_allocator_stats();
    let (ntcp, nudp) = net::socket_state_counts();
    let heap_total_bytes = KERNEL_HEAP_BYTES.load(Ordering::Relaxed);
    let heap_used_bytes = KERNEL_HEAP_USED_BYTES.load(Ordering::Relaxed);
    let page_cache = page_cache_stats();
    let dentry = dentry_perf_counters();
    let inode = inode_perf_counters();
    let getdents = getdents_perf_counters();
    MmSample {
        tick: now_tick,
        heap_total_bytes,
        heap_used_bytes,
        heap_slack_bytes: heap_total_bytes.saturating_sub(heap_used_bytes),
        free_pages: stats.free_pages,
        allocated_pages: stats.allocated_pages,
        total_pages: stats.total_pages,
        oom_count: stats.oom_count,
        cached_pages: page_cache.cached_pages,
        high_watermark: page_cache.high_watermark,
        low_watermark: page_cache.low_watermark,
        page_cache_inactive: page_cache.inactive_entries,
        page_cache_mappings: page_cache.mappings,
        dentry_entries: dentry.entries,
        dentry_inactive: dentry.inactive_entries,
        inode_entries: inode.entries,
        inode_inactive: inode.inactive_entries,
        nproc: list_pids().len(),
        ntcp,
        nudp,
        getdents_calls: getdents.calls,
        getdents_bytes: getdents.bytes,
        getdents_total_us: getdents.total_us,
        dir_snapshot_calls: getdents.dir_snapshots,
        dir_snapshot_entries: getdents.dir_snapshot_entries,
        dir_snapshot_total_us: getdents.dir_snapshot_us,
    }
}

/// Hook invoked from the timer-interrupt path.
///
/// Cheap in the common case: one relaxed atomic increment per tick, and a full
/// snapshot only once per [`SAMPLE_INTERVAL_TICKS`]. Sampling is pinned to hart 0
/// (mirroring `check_itimers_of_all_processes`) so there is a single writer to
/// the ring buffer. `now_tick` is the current `get_time()` value passed in by the
/// caller.
pub fn on_tick(now_tick: usize) {
    if crate::hal::hartid() != 0 {
        return;
    }
    let prev = TICKS_SINCE_SAMPLE.fetch_add(1, Ordering::Relaxed);
    if prev + 1 < SAMPLE_INTERVAL_TICKS {
        return;
    }
    TICKS_SINCE_SAMPLE.store(0, Ordering::Relaxed);

    let sample = collect_sample(now_tick);
    RING.lock().push(sample);
}

/// Render the buffered history as a whitespace-separated table, oldest sample
/// first. The leading `#` line is a human-readable header (skip with
/// `grep -v '^#'`). `uptime` is in seconds.
pub fn render() -> String {
    let ring = RING.lock();
    let mut out = String::with_capacity(ring.len * 128 + 128);
    let _ = writeln!(
        &mut out,
        "# uptime_s heap_tot heap_used heap_slack free alloc total oom cached hi_wm lo_wm \
         pc_inact pc_map dent dent_inact inode inode_inact nproc ntcp nudp gd_calls gd_bytes \
         gd_us ls_calls ls_ent ls_us (heap in bytes; page/cache counts in 4KiB pages or items; \
         *_us in microseconds)"
    );
    let start = if ring.len < HISTORY_LEN { 0 } else { ring.head };
    for i in 0..ring.len {
        let s = &ring.buf[(start + i) % HISTORY_LEN];
        let _ = writeln!(
            &mut out,
            "{} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
            s.tick / CLOCK_FREQ,
            s.heap_total_bytes,
            s.heap_used_bytes,
            s.heap_slack_bytes,
            s.free_pages,
            s.allocated_pages,
            s.total_pages,
            s.oom_count,
            s.cached_pages,
            s.high_watermark,
            s.low_watermark,
            s.page_cache_inactive,
            s.page_cache_mappings,
            s.dentry_entries,
            s.dentry_inactive,
            s.inode_entries,
            s.inode_inactive,
            s.nproc,
            s.ntcp,
            s.nudp,
            s.getdents_calls,
            s.getdents_bytes,
            s.getdents_total_us,
            s.dir_snapshot_calls,
            s.dir_snapshot_entries,
            s.dir_snapshot_total_us,
        );
    }
    out
}

/// Drop all buffered samples (the next sample starts a fresh trajectory).
pub fn reset() {
    let mut ring = RING.lock();
    ring.head = 0;
    ring.len = 0;
}
