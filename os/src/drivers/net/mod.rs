//! VirtIO network device discovery and IRQ dispatch.

mod virtio_net;

use alloc::sync::Arc;
use core::convert::TryFrom;
use core::ptr::NonNull;
use lazy_static::lazy_static;
use virtio_drivers::transport::{
    DeviceType, SomeTransport,
    mmio::{MmioTransport, VirtIOHeader},
};

use crate::platform::{
    VIRTIO_MMIO_BASE, VIRTIO_MMIO_IRQ_BASE, VIRTIO_MMIO_SLOTS, VIRTIO_MMIO_STRIDE,
};
use crate::sync::SpinNoIrqLock;

pub use virtio_net::VirtIONetDevice;

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
    /// Single discovered network device on QEMU virt for now.
    static ref NET_DEVICE: SpinNoIrqLock<Option<Arc<VirtIONetDevice>>> = SpinNoIrqLock::new(None);
}

/// Register a discovered network device (used by the unified probe).
pub fn register_device(dev: Arc<VirtIONetDevice>) {
    *NET_DEVICE.lock() = Some(dev);
}

/// Probe all VirtIO MMIO slots and register the first network device.
pub fn probe_net_devices() {
    for slot in 0..VIRTIO_MMIO_SLOTS {
        let addr = VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_STRIDE;
        let Some(header) = NonNull::new(addr as *mut VirtIOHeader) else {
            continue;
        };
        if mmio_slot_device_type(header) != Some(DeviceType::Network) {
            continue;
        }

        let transport = match unsafe { MmioTransport::new(header, VIRTIO_MMIO_STRIDE) } {
            Ok(t) => t,
            Err(_) => continue,
        };

        let irq = VIRTIO_MMIO_IRQ_BASE + slot as u32;
        if let Some(dev) = VirtIONetDevice::try_new(SomeTransport::from(transport), irq) {
            let mac = dev.mac_address();
            info!(
                "[kernel] virtio-net found at slot {} irq {} mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                slot,
                irq,
                mac[0],
                mac[1],
                mac[2],
                mac[3],
                mac[4],
                mac[5]
            );
            *NET_DEVICE.lock() = Some(Arc::new(dev));
            return;
        }
    }

    info!("[kernel] no VirtIO network device found");
}

/// Handle one virtio-mmio IRQ for network devices.
pub fn handle_irq(irq: u32) {
    let dev = {
        let guard = NET_DEVICE.lock();
        guard.as_ref().cloned()
    };
    if let Some(dev) = dev {
        if dev.irq() != irq {
            return;
        }
        dev.handle_irq();
        crate::net::notify_irq();
    }
}

/// Execute `f` with the discovered network device (if any).
pub fn with_device<R>(f: impl FnOnce(&Arc<VirtIONetDevice>) -> R) -> Option<R> {
    let guard = NET_DEVICE.lock();
    guard.as_ref().map(f)
}
