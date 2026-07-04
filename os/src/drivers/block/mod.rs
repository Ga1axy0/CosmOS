//! virtio_blk device driver

mod virtio_blk;

pub use virtio_blk::VirtIOBlock;

use crate::platform::{
    VIRTIO_MMIO_BASE, VIRTIO_MMIO_IRQ_BASE, VIRTIO_MMIO_SLOTS, VIRTIO_MMIO_STRIDE,
};
use crate::sync::SpinNoIrqLock;
use crate::task::{ReschedReason, SchedAttr, TaskControlBlock, TaskStatus, WaitQueue, WaitReason};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use core::convert::TryFrom;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use fs::BlockDevice;
use lazy_static::*;
use virtio_drivers::transport::{
    mmio::{MmioTransport, VirtIOHeader},
    DeviceType, SomeTransport,
};

/// Reset block driver performance counters.
#[cfg(feature = "io_perf_counters")]
pub fn reset_perf_counters() {
    virtio_blk::reset_perf_counters();
}

/// Render block driver performance counters.
#[cfg(feature = "io_perf_counters")]
pub fn render_perf_counters() -> String {
    virtio_blk::render_perf_counters()
}

/// Return the discovered block-device name for `idx`.
///
/// Default naming follows Linux-style partition numbering used by this tree:
/// `vda`, `vda2`, `vda3`, ...
/// With `legacy-vdb-names`, the names remain `vda`, `vdb`, `vdc`, ...
#[cfg(feature = "legacy-vdb-names")]
pub(crate) fn block_device_name(idx: usize) -> String {
    alloc::format!("vd{}", (b'a' + idx as u8) as char)
}

#[cfg(not(feature = "legacy-vdb-names"))]
pub(crate) fn block_device_name(idx: usize) -> String {
    if idx == 0 {
        String::from("vda")
    } else {
        alloc::format!("vda{}", idx + 1)
    }
}

#[inline]
fn mmio_slot_device_type(header: NonNull<VirtIOHeader>) -> Option<DeviceType> {
    // VirtIO MMIO register layout: magic(0x00), version(0x04), device_id(0x08).
    const MAGIC_VALUE: u32 = 0x7472_6976;
    const LEGACY_VERSION: u32 = 1;
    const MODERN_VERSION: u32 = 2;

    let base = header.as_ptr() as *const u32;
    // SAFETY: caller passes an MMIO header address on the virt bus.
    let magic = unsafe { core::ptr::read_volatile(base) };
    if magic != MAGIC_VALUE {
        return None;
    }
    // SAFETY: MMIO header word reads are volatile.
    let version = unsafe { core::ptr::read_volatile(base.add(1)) };
    if version != LEGACY_VERSION && version != MODERN_VERSION {
        return None;
    }
    // SAFETY: MMIO header word reads are volatile.
    let device_id = unsafe { core::ptr::read_volatile(base.add(2)) };
    DeviceType::try_from(device_id).ok()
}

lazy_static! {
    /// Registry of all discovered block devices, keyed by name (`"vda"`, `"vda2"`, … by default).
    ///
    /// Must be populated by [`probe_block_devices`] before any FS initialisation.
    pub static ref BLOCK_DEVICES: SpinNoIrqLock<BTreeMap<String, Arc<dyn BlockDevice>>> =
        SpinNoIrqLock::new(BTreeMap::new());

        /// VirtIO MMIO IRQ to block device mapping.
        pub static ref BLOCK_DEVICES_BY_IRQ: SpinNoIrqLock<BTreeMap<u32, Arc<VirtIOBlock>>> =
        SpinNoIrqLock::new(BTreeMap::new());
        static ref BLOCK_WORKER_WAIT: WaitQueue = WaitQueue::new();
        static ref BLOCK_WORKER_TASK: SpinNoIrqLock<Option<Arc<TaskControlBlock>>> =
        SpinNoIrqLock::new(None);
}

static BLOCK_WORKER_STARTED: AtomicBool = AtomicBool::new(false);
static BLOCK_COMPLETION_WORK_PENDING: AtomicBool = AtomicBool::new(false);
static BLOCK_WORKER_SLEEPS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_WORKER_WAKES: AtomicUsize = AtomicUsize::new(0);
static BLOCK_COMPLETION_EVENTS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_WORKER_LOOPS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_WORKER_PUMP_CALLS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_WORKER_PUMP_COMPLETED: AtomicUsize = AtomicUsize::new(0);
static BLOCK_WORKER_LAST_LOOP_NS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_WORKER_LAST_PUMP_NS: AtomicUsize = AtomicUsize::new(0);
static BLOCK_WORKER_IN_PUMP: AtomicBool = AtomicBool::new(false);
static BLOCK_WORKER_SELF_HEAL_COUNT: AtomicUsize = AtomicUsize::new(0);
static BLOCK_IRQ_SELF_HEAL_COUNT: AtomicUsize = AtomicUsize::new(0);

pub(super) struct BlockWorkerDebugSnapshot {
    task_ptr: usize,
    status: Option<TaskStatus>,
    wait: Option<WaitReason>,
    on_cpu: bool,
    on_rq: bool,
    last_cpu: usize,
    has_wq: bool,
    pending: u64,
    mask: u64,
    resched: Option<ReschedReason>,
    loops: usize,
    pump_calls: usize,
    pump_completed: usize,
    in_pump: bool,
    last_loop_age_ms: Option<usize>,
    last_pump_age_ms: Option<usize>,
    last_sched_op: crate::task::LastSchedOp,
}

/// Scan the VirtIO MMIO bus slots and register every block device found.
///
/// QEMU's `virt` machine maps up to 8 VirtIO devices starting at `0x1000_1000`,
/// each occupying `0x1000` bytes.  Devices are named `vda`, `vda2`, … by
/// default, or `vda`, `vdb`, … with `legacy-vdb-names`.
///
/// Must be called **before** `fs::init_rootfs` and `fs::init_dev`.
pub fn probe_block_devices() {
    let mut map = BLOCK_DEVICES.lock();
    let mut irq_map = BLOCK_DEVICES_BY_IRQ.lock();
    let mut idx = 0usize;
    for slot in 0..VIRTIO_MMIO_SLOTS {
        let addr = VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_STRIDE;

        let Some(header) = NonNull::new(addr as *mut VirtIOHeader) else {
            continue;
        };
        let device_type = mmio_slot_device_type(header);
        if device_type != Some(DeviceType::Block) {
            if let Some(kind) = device_type {
                debug!("[kernel] VirtIO slot {} is {:?}, skipping", slot, kind);
            }
            continue;
        }

        let transport = match unsafe { MmioTransport::new(header, VIRTIO_MMIO_STRIDE) } {
            Ok(t) => t,
            Err(_) => continue,
        };

        if let Some(dev) = VirtIOBlock::try_new(SomeTransport::from(transport)) {
            let dev = Arc::new(dev);
            let name = block_device_name(idx);
            debug!("[kernel] block device {} idx {} at {:#x}", name, idx, addr);
            map.insert(name, dev.clone());
            irq_map.insert(VIRTIO_MMIO_IRQ_BASE + slot as u32, dev);
            idx += 1;
        }
    }
    if idx == 0 {
        panic!("[kernel] no VirtIO block devices found");
    }
}

/// Handle one IRQ for a registered block device.
pub fn handle_irq(irq: u32) -> bool {
    if let Some(dev) = BLOCK_DEVICES_BY_IRQ.lock().get(&irq).cloned() {
        dev.handle_irq();
        true
    } else {
        false
    }
}

fn block_devices_snapshot() -> alloc::vec::Vec<Arc<VirtIOBlock>> {
    BLOCK_DEVICES_BY_IRQ.lock().values().cloned().collect()
}

fn block_devices_with_irq_snapshot() -> alloc::vec::Vec<(u32, Arc<VirtIOBlock>)> {
    BLOCK_DEVICES_BY_IRQ
        .lock()
        .iter()
        .map(|(irq, dev)| (*irq, Arc::clone(dev)))
        .collect()
}

fn block_worker_has_completions() -> bool {
    if BLOCK_COMPLETION_WORK_PENDING.load(Ordering::Acquire) {
        return true;
    }
    block_devices_snapshot()
        .into_iter()
        .any(|dev| dev.has_used_completions())
}

fn block_worker_pump_once() -> bool {
    let mut completed_any = false;
    for dev in block_devices_snapshot() {
        completed_any |= dev.pump_completions();
    }
    completed_any
}

fn age_ms_since(now_ns: usize, then_ns: usize) -> Option<usize> {
    if then_ns == 0 {
        None
    } else {
        Some(now_ns.saturating_sub(then_ns) / 1_000_000)
    }
}

fn block_worker_debug_snapshot(now_ns: usize) -> BlockWorkerDebugSnapshot {
    let task = BLOCK_WORKER_TASK.lock().as_ref().cloned();
    let (
        task_ptr,
        status,
        wait,
        on_cpu,
        on_rq,
        last_cpu,
        has_wq,
        pending,
        mask,
        resched,
        last_sched_op,
    ) = if let Some(task) = task {
        let task_ptr = Arc::as_ptr(&task) as usize;
        let task_inner = task.inner_exclusive_access();
        // Read `on_cpu` UNDER the task-inner lock. Every writer of `on_cpu`
        // (dequeue/steal/finish_pending/pick_run/block-abort/cancel/exit) holds
        // this same lock, so reading it here yields a value consistent with
        // status/on_rq/last_sched_op. Reading it before the lock (as before)
        // produced torn snapshots during the worker's rapid enqueue/dequeue/run
        // churn — e.g. on_cpu observed false (post-finish) while status observed
        // Runnable (just dequeued) — which falsely tripped the orphan self-heal.
        let on_cpu = task.on_cpu.load(Ordering::Relaxed);
        (
            task_ptr,
            Some(task_inner.task_status),
            task_inner.wait_reason,
            on_cpu,
            task_inner.sched.on_rq,
            task_inner.sched.last_cpu,
            task_inner.current_wq_handle.is_some(),
            task_inner.pending_signals.bits(),
            task_inner.signal_mask.bits(),
            task_inner.sched.resched_reason,
            task_inner.last_sched_op,
        )
    } else {
        (
            0,
            None,
            None,
            false,
            false,
            0,
            false,
            0,
            0,
            None,
            crate::task::LastSchedOp::default(),
        )
    };

    BlockWorkerDebugSnapshot {
        task_ptr,
        status,
        wait,
        on_cpu,
        on_rq,
        last_cpu,
        has_wq,
        pending,
        mask,
        resched,
        loops: BLOCK_WORKER_LOOPS.load(Ordering::Relaxed),
        pump_calls: BLOCK_WORKER_PUMP_CALLS.load(Ordering::Relaxed),
        pump_completed: BLOCK_WORKER_PUMP_COMPLETED.load(Ordering::Relaxed),
        in_pump: BLOCK_WORKER_IN_PUMP.load(Ordering::Acquire),
        last_loop_age_ms: age_ms_since(
            now_ns,
            BLOCK_WORKER_LAST_LOOP_NS.load(Ordering::Acquire),
        ),
        last_pump_age_ms: age_ms_since(
            now_ns,
            BLOCK_WORKER_LAST_PUMP_NS.load(Ordering::Acquire),
        ),
        last_sched_op,
    }
}

fn block_io_worker_main() -> ! {
    warn!("[virtio_blk] worker started");
    loop {
        BLOCK_WORKER_LOOPS.fetch_add(1, Ordering::Relaxed);
        BLOCK_WORKER_LAST_LOOP_NS.store(crate::timer::get_time_ns() as usize, Ordering::Release);
        let had_completion_event = BLOCK_COMPLETION_WORK_PENDING.swap(false, Ordering::AcqRel);
        BLOCK_WORKER_PUMP_CALLS.fetch_add(1, Ordering::Relaxed);
        BLOCK_WORKER_LAST_PUMP_NS.store(crate::timer::get_time_ns() as usize, Ordering::Release);
        BLOCK_WORKER_IN_PUMP.store(true, Ordering::Release);
        let completed_any = block_worker_pump_once();
        BLOCK_WORKER_IN_PUMP.store(false, Ordering::Release);
        if completed_any {
            BLOCK_WORKER_PUMP_COMPLETED.fetch_add(1, Ordering::Relaxed);
        }
        if completed_any || had_completion_event {
            continue;
        }
        BLOCK_WORKER_SLEEPS.fetch_add(1, Ordering::Relaxed);
        BLOCK_WORKER_WAIT
            .wait_with_reason_or_skip(WaitReason::BlockDeviceIo, block_worker_has_completions);
    }
}

/// Start the global block I/O completion worker.
pub fn start_workers() {
    if BLOCK_WORKER_STARTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let task = crate::task::spawn_kernel_thread(block_io_worker_main, SchedAttr::other(0));
    warn!(
        "[virtio_blk][worker] spawned task={:#x}",
        Arc::as_ptr(&task) as usize
    );
    *BLOCK_WORKER_TASK.lock() = Some(task);
}

pub(crate) fn wake_worker() {
    BLOCK_WORKER_WAKES.fetch_add(1, Ordering::Relaxed);
    BLOCK_WORKER_WAIT.wake_all();
}

pub(crate) fn schedule_completion_work() {
    BLOCK_COMPLETION_EVENTS.fetch_add(1, Ordering::Relaxed);
    BLOCK_COMPLETION_WORK_PENDING.store(true, Ordering::Release);
    BLOCK_WORKER_WAIT.wake_all();
}

/// Timer-driven WARN diagnostics for stuck block I/O.
pub fn warn_if_stalled(now_ns: usize) {
    let worker_waiters = BLOCK_WORKER_WAIT.debug_waiter_count();
    let worker_work_pending = BLOCK_COMPLETION_WORK_PENDING.load(Ordering::Acquire);
    let worker_sleeps = BLOCK_WORKER_SLEEPS.load(Ordering::Relaxed);
    let worker_wakes = BLOCK_WORKER_WAKES.load(Ordering::Relaxed);
    let completion_events = BLOCK_COMPLETION_EVENTS.load(Ordering::Relaxed);
    let worker = block_worker_debug_snapshot(now_ns);

    for (irq, dev) in block_devices_with_irq_snapshot() {
        dev.warn_if_stalled(
            irq,
            now_ns,
            worker_waiters,
            worker_work_pending,
            worker_sleeps,
            worker_wakes,
            completion_events,
            &worker,
        );
    }

    // Self-heal: if the worker is a lost-runnable orphan (Runnable, on no
    // runqueue, on no CPU, and not in its wait queue), `wake_worker()` cannot
    // rescue it — `wake_all` finds the WQ empty and never calls `wakeup_task`,
    // so the repair path in `enqueue_wakeup_task` is never reached. Re-enqueue
    // it directly. This is a safety net while the underlying wake/block race is
    // closed elsewhere; it downgrades a permanent stall to a brief blip.
    //
    // Require the worker to have NOT looped for at least 200ms: a Runnable task
    // is briefly off-rq/off-CPU during normal transitions too (e.g. inside
    // `finish_pending_task_release`, between clearing on_cpu and `add_task`
    // setting on_rq). Healing on that transient would be a spurious no-op and
    // log noise. A genuine orphan's loop counter stops advancing, so gating on
    // staleness fires only for real orphans.
    let worker_stalled = worker
        .last_loop_age_ms
        .is_some_and(|age_ms| age_ms >= 200);
    if worker.status == Some(TaskStatus::Runnable)
        && !worker.on_rq
        && !worker.on_cpu
        && worker_waiters == 0
        && worker.task_ptr != 0
        && worker_stalled
    {
        let count = BLOCK_WORKER_SELF_HEAL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 16 || count.is_power_of_two() {
            error!(
                "[virtio_blk] orphan self-heal count={} task={:#x} loops={} \
                 last_sched_op={:?} — re-enqueueing lost-runnable worker",
                count, worker.task_ptr, worker.loops, worker.last_sched_op
            );
        }
        if let Some(wtask) = BLOCK_WORKER_TASK.lock().as_ref().cloned() {
            crate::task::wakeup_task(wtask);
        }
    }

    // Self-heal (lost completion IRQ): a completion is sitting in some device's
    // used ring (`block_worker_has_completions()` true) but the worker has not
    // looped in >=200ms — meaning its completion IRQ never reached `handle_irq`,
    // so `schedule_completion_work` was never called and neither the worker nor
    // the blocked waiter will wake to pump the used ring. Re-arm the completion
    // work from this timer tick; the worker then pumps, completes the request,
    // and wakes the waiter. This recovers regardless of *why* the IRQ was lost
    // (EXTIOI/PCH-PIC race, virtio MMIO ISR-vs-used-ring-DMA visibility, edge
    // coalescing). Gated on the same staleness criterion as the orphan heal.
    if worker_stalled && block_worker_has_completions() {
        let count = BLOCK_IRQ_SELF_HEAL_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= 16 || count.is_power_of_two() {
            error!(
                "[virtio_blk] lost-irq self-heal count={} work_pending={} \
                 worker_loops={} worker_status={:?} — re-arming completion work",
                count, worker_work_pending, worker.loops, worker.status
            );
        }
        schedule_completion_work();
    }
}

lazy_static! {
    /// The primary block device (`vda`), provided for backward compatibility.
    ///
    /// [`probe_block_devices`] must be called before this is first accessed.
    pub static ref BLOCK_DEVICE: Arc<dyn BlockDevice> = BLOCK_DEVICES
        .lock()
        .get(&block_device_name(0))
        .cloned()
        .expect("[kernel] BLOCK_DEVICE: vda not found");
}

#[allow(unused)]
/// Test the block device
pub fn block_device_test() {
    let block_device = BLOCK_DEVICE.clone();
    let mut write_buffer = [0u8; 512];
    let mut read_buffer = [0u8; 512];
    for i in 0..512 {
        for byte in write_buffer.iter_mut() {
            *byte = i as u8;
        }
        block_device.write_block(i as usize, &write_buffer);
        block_device.read_block(i as usize, &mut read_buffer);
        assert_eq!(write_buffer, read_buffer);
    }
    println!("block device test passed!");
}
