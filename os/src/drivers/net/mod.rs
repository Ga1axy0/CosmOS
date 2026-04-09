//! VirtIO network device discovery and IRQ dispatch.

mod virtio_net;

use alloc::sync::Arc;
use core::ptr::NonNull;
use lazy_static::lazy_static;
use virtio_drivers::transport::{
    DeviceType,
    Transport,
    mmio::{MmioTransport, VirtIOHeader},
};

use crate::sync::SpinNoIrqLock;

pub use virtio_net::VirtIONetDevice;

lazy_static! {
    /// Single discovered network device on QEMU virt for now.
    static ref NET_DEVICE: SpinNoIrqLock<Option<Arc<VirtIONetDevice>>> = unsafe {
        SpinNoIrqLock::new(None)
    };
}

/// Probe all VirtIO MMIO slots and register the first network device.
pub fn probe_net_devices() {
    const VIRTIO_MMIO_BASE: usize = 0x1000_1000;
    const VIRTIO_MMIO_STRIDE: usize = 0x1000;
    const VIRTIO_MMIO_SLOTS: usize = 8;
    const VIRTIO_MMIO_IRQ_BASE: u32 = 1;

    for slot in 0..VIRTIO_MMIO_SLOTS {
        let addr = VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_STRIDE;
        let Some(header) = NonNull::new(addr as *mut VirtIOHeader) else {
            continue;
        };
        let transport = match unsafe { MmioTransport::new(header, VIRTIO_MMIO_STRIDE) } {
            Ok(t) => t,
            Err(_) => continue,
        };
        if transport.device_type() != DeviceType::Network {
            continue;
        }

        let irq = VIRTIO_MMIO_IRQ_BASE + slot as u32;
        if let Some(dev) = VirtIONetDevice::try_new(transport, irq) {
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
