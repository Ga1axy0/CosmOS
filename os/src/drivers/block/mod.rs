//! virtio_blk device driver

mod virtio_blk;

pub use virtio_blk::VirtIOBlock;

use crate::sync::SpinNoIrqLock;
use crate::task::{yield_current_and_run_next, SchedAttr, WaitQueue, WaitReason};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicBool, Ordering};
use fs::BlockDevice;
use lazy_static::*;
use core::ptr::NonNull;
use virtio_drivers::transport::{
    DeviceType, SomeTransport,
    mmio::{MmioTransport, VirtIOHeader},
};
use crate::platform::{
    VIRTIO_MMIO_BASE, VIRTIO_MMIO_IRQ_BASE, VIRTIO_MMIO_SLOTS, VIRTIO_MMIO_STRIDE,
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
}

static BLOCK_WORKER_STARTED: AtomicBool = AtomicBool::new(false);

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

fn block_worker_has_work() -> bool {
    block_devices_snapshot()
        .into_iter()
        .any(|dev| dev.has_pending_requests() || dev.has_used_completions())
}

fn block_worker_pump_once() -> (bool, bool) {
    let mut completed_any = false;
    let mut pending_any = false;
    for dev in block_devices_snapshot() {
        completed_any |= dev.pump_completions();
        pending_any |= dev.has_pending_requests();
    }
    (completed_any, pending_any)
}

fn block_io_worker_main() -> ! {
    warn!("[virtio_blk] worker started");
    loop {
        let (completed_any, pending_any) = block_worker_pump_once();
        if completed_any {
            continue;
        }
        // Yield-based polling
        // if pending_any {
        //     yield_current_and_run_next();
        //     continue;
        // }
        BLOCK_WORKER_WAIT.wait_with_reason_or_skip(WaitReason::Unknown, block_worker_has_work);
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
    crate::task::spawn_kernel_thread(block_io_worker_main, SchedAttr::other(0));
}

pub(crate) fn wake_worker() {
    BLOCK_WORKER_WAIT.wake_all();
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
