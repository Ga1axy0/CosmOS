//! VirtIO network device discovery and IRQ dispatch.

#[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
mod loongson_gmac;
mod virtio_net;

use alloc::sync::Arc;
use core::convert::TryFrom;
use core::ptr::NonNull;
use lazy_static::lazy_static;
use virtio_drivers::transport::{
    mmio::{MmioTransport, VirtIOHeader},
    DeviceType, SomeTransport,
};

use crate::sync::SpinNoIrqLock;

#[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
use loongson_gmac::LoongsonGmacDevice;
pub use virtio_net::VirtIONetDevice;

/// Runtime-selected Ethernet controller exposed to the kernel network stack.
pub(crate) enum NetworkDevice {
    /// A VirtIO network controller.
    Virtio(VirtIONetDevice),
    /// The LS2K1000 integrated DesignWare GMAC.
    #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
    LoongsonGmac(LoongsonGmacDevice),
}

impl NetworkDevice {
    /// Return the device interrupt number.
    pub(crate) fn irq(&self) -> u32 {
        match self {
            Self::Virtio(dev) => dev.irq(),
            #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
            Self::LoongsonGmac(dev) => dev.irq(),
        }
    }

    /// Return the station address used by this controller.
    pub(crate) fn mac_address(&self) -> [u8; 6] {
        match self {
            Self::Virtio(dev) => dev.mac_address(),
            #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
            Self::LoongsonGmac(dev) => dev.mac_address(),
        }
    }

    /// Acknowledge and service one controller interrupt.
    pub(crate) fn handle_irq(&self) {
        match self {
            Self::Virtio(dev) => dev.handle_irq(),
            #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
            Self::LoongsonGmac(dev) => dev.handle_irq(),
        }
    }

    /// Return whether the transmit ring can accept one frame.
    pub(crate) fn can_send(&self) -> bool {
        match self {
            Self::Virtio(dev) => dev.can_send(),
            #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
            Self::LoongsonGmac(dev) => dev.can_send(),
        }
    }

    /// Queue one Ethernet frame without blocking.
    pub(crate) fn try_send(&self, frame: &[u8]) -> bool {
        match self {
            Self::Virtio(dev) => match dev.try_send(frame) {
                Ok(queued) => queued,
                Err(err) => {
                    warn!("virtio-net: failed to queue TX frame: {err:?}");
                    false
                }
            },
            #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
            Self::LoongsonGmac(dev) => dev.try_send(frame),
        }
    }

    /// Copy one completed Ethernet frame into `out`.
    pub(crate) fn try_recv(&self, out: &mut [u8]) -> Option<usize> {
        match self {
            Self::Virtio(dev) => dev.try_recv(out),
            #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
            Self::LoongsonGmac(dev) => dev.try_recv(out),
        }
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
    /// Single discovered network device on QEMU virt for now.
    static ref NET_DEVICE: SpinNoIrqLock<Option<Arc<NetworkDevice>>> = SpinNoIrqLock::new(None);
}

/// Register a discovered network device (used by the unified probe).
pub fn register_device(dev: VirtIONetDevice) {
    *NET_DEVICE.lock() = Some(Arc::new(NetworkDevice::Virtio(dev)));
}

/// Probe and register the LS2K1000 GMAC described by firmware.
#[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
pub fn probe_loongson_gmac() -> Option<u32> {
    let Some(resource) = crate::bootinfo::get().gmac() else {
        println!("[net] LS2K1000 FDT has no supported GMAC resource");
        return None;
    };
    println!("[net] probing LS2K1000 GMAC resource {:?}", resource);
    let Some(dev) = LoongsonGmacDevice::try_new(resource) else {
        println!("[net] LS2K1000 GMAC initialization failed");
        return None;
    };
    let irq = dev.irq();
    *NET_DEVICE.lock() = Some(Arc::new(NetworkDevice::LoongsonGmac(dev)));
    println!("[net] LS2K1000 GMAC registered on IRQ {}", irq);
    Some(irq)
}

/// Probe all VirtIO MMIO slots and register the first network device.
pub fn probe_net_devices() {
    for (slot, resource) in crate::bootinfo::get()
        .virtio_mmio_devices()
        .iter()
        .enumerate()
    {
        let addr = crate::platform::mmio_phys_to_virt(resource.start);
        let Some(header) = NonNull::new(addr as *mut VirtIOHeader) else {
            continue;
        };
        if mmio_slot_device_type(header) != Some(DeviceType::Network) {
            continue;
        }

        let transport = match unsafe { MmioTransport::new(header, resource.size) } {
            Ok(t) => t,
            Err(_) => continue,
        };

        let irq = resource
            .irq
            .expect("FDT VirtIO-MMIO network transport has no interrupt");
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
            *NET_DEVICE.lock() = Some(Arc::new(NetworkDevice::Virtio(dev)));
            return;
        }
    }

    info!("[kernel] no VirtIO network device found");
}

/// Handle one IRQ for the registered network device.
pub fn handle_irq(irq: u32) -> bool {
    let dev = {
        let guard = NET_DEVICE.lock();
        guard.as_ref().cloned()
    };
    if let Some(dev) = dev {
        if dev.irq() != irq {
            return false;
        }
        dev.handle_irq();
        crate::net::notify_irq();
        true
    } else {
        false
    }
}

/// Execute `f` with the discovered network device (if any).
pub(crate) fn with_device<R>(f: impl FnOnce(&Arc<NetworkDevice>) -> R) -> Option<R> {
    let guard = NET_DEVICE.lock();
    guard.as_ref().map(f)
}
