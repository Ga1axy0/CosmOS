//! VirtIO network device discovery and IRQ dispatch.

#[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
mod jh7110_eqos;
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

#[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
use jh7110_eqos::Jh7110EqosDevice;
#[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
use loongson_gmac::LoongsonGmacDevice;
pub use virtio_net::VirtIONetDevice;

/// Read-only JH7110 EQoS hardware state used by the temporary net diagnostics.
#[cfg(feature = "net_perf_counters")]
pub(crate) struct NetHardwareDebug {
    pub(crate) mmc_control: u32,
    pub(crate) mmc_tx_frames_gb: u64,
    pub(crate) mmc_tx_good_frames: u64,
    pub(crate) mmc_tx_underflow: u64,
    pub(crate) mmc_tx_late_collision: u64,
    pub(crate) mmc_tx_carrier_error: u64,
    pub(crate) mmc_rx_frames_gb: u64,
    pub(crate) mmc_rx_crc_error: u64,
    pub(crate) dma_status: u32,
    pub(crate) dma_interrupt_enable: u32,
    pub(crate) mtl_txq_debug: u32,
    pub(crate) tx_tail: u32,
    pub(crate) tx_next: usize,
    pub(crate) tx_owned_by_dma: usize,
    pub(crate) tx_error_summary: usize,
}

/// Runtime-selected Ethernet controller exposed to the kernel network stack.
pub(crate) enum NetworkDevice {
    /// A VirtIO network controller.
    Virtio(VirtIONetDevice),
    /// The LS2K1000 integrated DesignWare GMAC.
    #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
    LoongsonGmac(LoongsonGmacDevice),
    /// The JH7110 integrated Synopsys EQoS 5.20 controller.
    #[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
    Jh7110Eqos(Jh7110EqosDevice),
}

impl NetworkDevice {
    /// Return the device interrupt number.
    pub(crate) fn irq(&self) -> u32 {
        match self {
            Self::Virtio(dev) => dev.irq(),
            #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
            Self::LoongsonGmac(dev) => dev.irq(),
            #[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
            Self::Jh7110Eqos(dev) => dev.irq(),
        }
    }

    /// Return the station address used by this controller.
    pub(crate) fn mac_address(&self) -> [u8; 6] {
        match self {
            Self::Virtio(dev) => dev.mac_address(),
            #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
            Self::LoongsonGmac(dev) => dev.mac_address(),
            #[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
            Self::Jh7110Eqos(dev) => dev.mac_address(),
        }
    }

    /// Acknowledge and service one controller interrupt.
    pub(crate) fn handle_irq(&self) {
        match self {
            Self::Virtio(dev) => dev.handle_irq(),
            #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
            Self::LoongsonGmac(dev) => dev.handle_irq(),
            #[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
            Self::Jh7110Eqos(dev) => dev.handle_irq(),
        }
    }

    /// Complete one deferred network poll and re-arm device interrupts.
    pub(crate) fn complete_poll(&self) -> bool {
        match self {
            #[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
            Self::Jh7110Eqos(dev) => dev.complete_poll(),
            _ => false,
        }
    }

    #[cfg(feature = "net_perf_counters")]
    pub(crate) fn hardware_debug(&self) -> Option<NetHardwareDebug> {
        match self {
            #[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
            Self::Jh7110Eqos(dev) => Some(dev.hardware_debug()),
            _ => None,
        }
    }

    /// Run controller work that is intentionally excluded from hardirq context.
    pub(crate) fn service_deferred(&self) {
        match self {
            Self::Virtio(dev) => dev.service_deferred(),
            #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
            Self::LoongsonGmac(_) => {},
            #[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
            Self::Jh7110Eqos(_) => {},
        }
    }

    /// Return whether the transmit ring can accept one frame.
    pub(crate) fn can_send(&self) -> bool {
        match self {
            Self::Virtio(dev) => dev.can_send(),
            #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
            Self::LoongsonGmac(dev) => dev.can_send(),
            #[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
            Self::Jh7110Eqos(dev) => dev.can_send(),
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
            #[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
            Self::Jh7110Eqos(dev) => dev.try_send(frame),
        }
    }

    /// Copy one completed Ethernet frame into `out`.
    pub(crate) fn try_recv(&self, out: &mut [u8]) -> Option<usize> {
        match self {
            Self::Virtio(dev) => dev.try_recv(out),
            #[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
            Self::LoongsonGmac(dev) => dev.try_recv(out),
            #[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
            Self::Jh7110Eqos(dev) => dev.try_recv(out),
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

/// Probe and register the usable LS2K1000 GMAC described by firmware.
///
/// A legacy CosmOS network stack has one physical-Ethernet slot.  Probe every
/// FDT-described port rather than binding an address constant; ports that
/// declare a default pinctrl state are attempted first because they are the
/// board-routed ports.  Once the stack grows multi-interface support this loop
/// can register each successful device without changing FDT discovery again.
#[cfg(all(target_arch = "loongarch64", feature = "platform-ls2k1000-nebula"))]
pub fn probe_loongson_gmac() -> Option<u32> {
    let info = crate::boot::context::get();
    if info.devices().gmac_devices().next().is_none() {
        println!("[net] LS2K1000 FDT has no supported GMAC resource");
        return None;
    }

    for require_pinctrl in [true, false] {
        for resource in info.devices().gmac_devices() {
            if resource.pinctrl_default().is_some() != require_pinctrl {
                continue;
            }
            println!(
                "[net] probing LS2K1000 GMAC resource {:?}, pinctrl_default={:?}, phy_mode={:?}, phy_handle={:?}, phy_addr={:?}",
                resource,
                resource.pinctrl_default(),
                resource.phy_mode(),
                resource.phy_handle(),
                resource.phy_addr(),
            );
            let Some(dev) = LoongsonGmacDevice::try_new(resource) else {
                println!("[net] LS2K1000 GMAC initialization failed for {:#x}", resource.device().start);
                continue;
            };
            let irq = dev.irq();
            *NET_DEVICE.lock() = Some(Arc::new(NetworkDevice::LoongsonGmac(dev)));
            println!("[net] LS2K1000 GMAC registered on IRQ {}", irq);
            return Some(irq);
        }
    }
    println!("[net] no FDT-described LS2K1000 GMAC completed initialization");
    None
}

/// Probe the JH7110 EQoS port used by the successful U-Boot TFTP path.
#[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
pub fn probe_jh7110_eqos() -> Option<u32> {
    let info = crate::boot::context::get();
    let preferred = info
        .devices()
        .gmac_devices()
        .find(|resource| resource.device().start == 0x1604_0000)
        .or_else(|| info.devices().gmac_devices().next());
    let Some(resource) = preferred else {
        println!("[jh7110-eqos] live FDT has no supported EQoS resource");
        return None;
    };
    println!("[jh7110-eqos] probing resource {:?}", resource);
    let Some(dev) = Jh7110EqosDevice::try_new(resource) else {
        println!("[jh7110-eqos] native initialization failed");
        return None;
    };
    let irq = dev.irq();
    *NET_DEVICE.lock() = Some(Arc::new(NetworkDevice::Jh7110Eqos(dev)));
    println!("[jh7110-eqos] registered interrupt-driven network device");
    Some(irq)
}

/// Probe all VirtIO MMIO slots and register the first network device.
pub fn probe_net_devices() {
    for (slot, resource) in crate::boot::context::get()
        .devices()
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

#[cfg(feature = "net_perf_counters")]
pub(crate) fn hardware_debug() -> Option<NetHardwareDebug> {
    with_device(|device| device.hardware_debug()).flatten()
}

/// Service deferred NIC completions in scheduler task context.
pub(crate) fn service_deferred() {
    let dev = {
        let guard = NET_DEVICE.lock();
        guard.as_ref().cloned()
    };
    if let Some(dev) = dev {
        dev.service_deferred();
    }
}
