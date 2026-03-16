//! virtio_blk device driver

mod virtio_blk;

pub use virtio_blk::VirtIOBlock;

use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::string::String;
use virtio_drivers::{DeviceType, VirtIOHeader};
use alloc::sync::Arc;
use fs::BlockDevice;
use lazy_static::*;

lazy_static! {
    /// Registry of all discovered block devices, keyed by name (`"vda"`, `"vdb"`, …).
    ///
    /// Must be populated by [`probe_block_devices`] before any FS initialisation.
    pub static ref BLOCK_DEVICES: UPSafeCell<BTreeMap<String, Arc<dyn BlockDevice>>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
}

/// Scan the VirtIO MMIO bus slots and register every block device found.
///
/// QEMU's `virt` machine maps up to 8 VirtIO devices starting at `0x1000_1000`,
/// each occupying `0x1000` bytes.  Devices are named `vda`, `vdb`, … in the
/// order they are discovered.
///
/// Must be called **before** `fs::init_rootfs` and `fs::init_dev`.
pub fn probe_block_devices() {
    const VIRTIO_MMIO_BASE:   usize = 0x1000_1000;
    const VIRTIO_MMIO_STRIDE: usize = 0x1000;
    const VIRTIO_MMIO_SLOTS:  usize = 8;

    let mut map = BLOCK_DEVICES.exclusive_access();
    let mut idx = 0usize;
    for slot in 0..VIRTIO_MMIO_SLOTS {
        let addr = VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_STRIDE;

        let hdr = unsafe { &*(addr as *const VirtIOHeader) };
        if !hdr.verify() {
            continue; // Bad header or no device present
        }
        if hdr.device_type() != DeviceType::Block {
            debug!("[kernel] VirtIO slot {} is {:?}, skipping", slot, hdr.device_type());
            continue; // Not a block device
        }

        if let Some(dev) = VirtIOBlock::try_new(addr) {
            // let name = alloc::format!("vd{}", (b'a' + idx as u8) as char);
            let name = if idx > 0 {
                alloc::format!("vda{}", (idx + 1))
            } else {
                "vda".into()
            };
            debug!("[kernel] block device {} idx {} at {:#x}", name, idx, addr);
            map.insert(name, Arc::new(dev));
            idx += 1;
        }
    }
    if idx == 0 {
        panic!("[kernel] no VirtIO block devices found");
    }
}

lazy_static! {
    /// The primary block device (`vda`), provided for backward compatibility.
    ///
    /// [`probe_block_devices`] must be called before this is first accessed.
    pub static ref BLOCK_DEVICE: Arc<dyn BlockDevice> = BLOCK_DEVICES
        .exclusive_access()
        .get("vda")
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
