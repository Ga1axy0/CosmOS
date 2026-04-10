//! block device driver

pub mod block;
pub mod chardev;
pub mod net;
pub mod plic;
pub mod rtc;
pub mod virtio;
use core::ptr::NonNull;

use alloc::{string::String, sync::Arc};
pub use block::BLOCK_DEVICE;
use virtio_drivers::transport::{DeviceType, mmio::{MmioTransport, VirtIOHeader}};

use crate::drivers::{block::{BLOCK_DEVICES, BLOCK_DEVICES_BY_IRQ, VirtIOBlock}, net::VirtIONetDevice};

/// Initialize all drivers (block, char, PLIC, …).
pub fn init() {
    chardev::init();
    rtc::init();
    plic::init();
    probe_virtio_devices();
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

/// Probe all VirtIO MMIO slots once and register devices found.
pub fn probe_virtio_devices() {
    const VIRTIO_MMIO_BASE: usize = 0x1000_1000;
    const VIRTIO_MMIO_STRIDE: usize = 0x1000;
    const VIRTIO_MMIO_SLOTS: usize = 8;
    const VIRTIO_MMIO_IRQ_BASE: u32 = 1;

    let mut map = BLOCK_DEVICES.lock();
    let mut irq_map = BLOCK_DEVICES_BY_IRQ.lock();
    let mut idx = 0usize;

    for slot in 0..VIRTIO_MMIO_SLOTS {
        let addr = VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_STRIDE;

        let Some(header) = NonNull::new(addr as *mut VirtIOHeader) else {
            continue;
        };

        match mmio_slot_device_type(header) {
            Some(DeviceType::Block) => {
                let transport = match unsafe { MmioTransport::new(header, VIRTIO_MMIO_STRIDE) } {
                    Ok(t) => t,
                    Err(_) => continue,
                };

                if let Some(dev) = VirtIOBlock::try_new(transport) {
                    let dev = Arc::new(dev);
                    let name: String = if idx > 0 {
                        alloc::format!("vda{}", (idx + 1))
                    } else {
                        "vda".into()
                    };
                    debug!("[kernel] block device {} idx {} at {:#x}", name, idx, addr);
                    map.insert(name, dev.clone());
                    irq_map.insert(VIRTIO_MMIO_IRQ_BASE + slot as u32, dev);
                    idx += 1;
                }
            }
            Some(DeviceType::Network) => {
                let transport = match unsafe { MmioTransport::new(header, VIRTIO_MMIO_STRIDE) } {
                    Ok(t) => t,
                    Err(_) => continue,
                };

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
                    // Register into net module via its public helper.
                    net::register_device(Arc::new(dev));
                    // For now we stop at the first net device like the old behaviour.
                    // Continue scanning for block devices though.
                }
            }
            Some(kind) => {
                debug!("[kernel] VirtIO slot {} is {:?}, skipping", slot, kind);
            }
            None => {}
        }
    }

    if idx == 0 {
        panic!("[kernel] no VirtIO block devices found");
    }
}
