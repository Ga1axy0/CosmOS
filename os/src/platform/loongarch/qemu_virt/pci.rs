//! LoongArch64 QEMU `virt` PCI/ECAM probing for VirtIO PCI devices.

use alloc::sync::Arc;
use core::ptr::read_volatile;
use virtio_drivers::transport::{
    pci::{
        bus::{
            BarInfo, Cam, Command, DeviceFunction, DeviceFunctionInfo, HeaderType, MemoryBarType,
            MmioCam, PciRoot,
        },
        virtio_device_type, PciTransport,
    },
    DeviceType, SomeTransport, Transport,
};

use crate::drivers::{
    block::{block_device_name, VirtIOBlock, BLOCK_DEVICES, BLOCK_DEVICES_BY_IRQ},
    net::{self, VirtIONetDevice},
};

const PCI_INTERRUPT_LINE_PIN_OFFSET: usize = 0x3c;

/// Probe the LA64 PCIe ECAM bus and register VirtIO PCI devices.
pub fn probe_platform_devices() {
    let Some(host) = crate::bootinfo::get().pci_host() else {
        return;
    };
    if host.memory_size == 0 {
        panic!("FDT PCI host has no non-prefetchable memory aperture");
    }
    let ecam_vaddr = crate::platform::mmio_phys_to_virt(host.ecam.start);
    let ecam_end = ecam_vaddr + host.ecam.size;
    if ecam_end < ecam_vaddr {
        panic!("PCI ECAM window overflow");
    }

    let mut root = unsafe { PciRoot::new(MmioCam::new(ecam_vaddr as *mut u8, Cam::Ecam)) };
    let mut allocator = PciRangeAllocator::new(host.memory_start as u64, host.memory_size as u64);
    let mut map = BLOCK_DEVICES.lock();
    let mut irq_map = BLOCK_DEVICES_BY_IRQ.lock();
    let mut block_idx = 0usize;

    for bus in host.bus_start..=host.bus_end {
        for (bdf, dev_info) in root.enumerate_bus(bus) {
            if dev_info.header_type != HeaderType::Standard {
                continue;
            }

            if let Err(err) = configure_pci_device(&mut root, bdf, &mut allocator) {
                warn!(
                    "[pci] failed to configure device at {} ({}:{:#06x}): {:?}",
                    bdf, dev_info.vendor_id, dev_info.device_id, err
                );
                continue;
            }

            let Some(transport) = probe_virtio_pci_device(&mut root, bdf, &dev_info) else {
                continue;
            };

            match transport.device_type() {
                DeviceType::Block => {
                    let Some(dev) = VirtIOBlock::try_new(SomeTransport::from(transport)) else {
                        warn!("[pci] failed to create virtio-blk for {}", bdf);
                        continue;
                    };
                    let dev = Arc::new(dev);
                    let name = block_device_name(block_idx);
                    let irq = gpex_intx_irq(host, bdf);
                    info!("[pci] virtio-blk {} at {} irq {:?}", name, bdf, irq);
                    map.insert(name, dev.clone());
                    if let Some(irq) = irq {
                        if super::irq::enable_device_irq(irq) {
                            irq_map.insert(irq, dev);
                        }
                    }
                    block_idx += 1;
                }
                DeviceType::Network => {
                    let Some(irq) = gpex_intx_irq(host, bdf) else {
                        warn!("[pci] virtio-net at {} has no INTx pin", bdf);
                        continue;
                    };
                    let Some(dev) = VirtIONetDevice::try_new(SomeTransport::from(transport), irq)
                    else {
                        warn!("[pci] failed to create virtio-net for {}", bdf);
                        continue;
                    };
                    let mac = dev.mac_address();
                    info!(
                        "[pci] virtio-net at {} irq {} mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        bdf,
                        irq,
                        mac[0],
                        mac[1],
                        mac[2],
                        mac[3],
                        mac[4],
                        mac[5]
                    );
                    super::irq::enable_device_irq(irq);
                    net::register_device(dev);
                }
                other => {
                    warn!(
                        "[pci] ignore unsupported virtio device {:?} at {}",
                        other, bdf
                    );
                }
            }
        }
    }

    if block_idx == 0 {
        panic!("[kernel] no VirtIO block devices found");
    }
}

#[derive(Debug)]
struct PciRangeAllocator {
    end: u64,
    current: u64,
}

impl PciRangeAllocator {
    const fn new(base: u64, size: u64) -> Self {
        Self {
            end: base + size,
            current: base,
        }
    }

    fn alloc(&mut self, size: u64) -> Option<u64> {
        if !size.is_power_of_two() {
            return None;
        }
        let ret = align_up(self.current, size);
        if ret + size > self.end {
            return None;
        }
        self.current = ret + size;
        Some(ret)
    }
}

const fn align_up(addr: u64, align: u64) -> u64 {
    (addr + align - 1) & !(align - 1)
}

fn configure_pci_device(
    root: &mut PciRoot<MmioCam<'static>>,
    bdf: DeviceFunction,
    allocator: &mut PciRangeAllocator,
) -> Result<(), ()> {
    let mut bar = 0u8;
    while bar < 6 {
        let info = root.bar_info(bdf, bar).map_err(|_| ())?;
        if let Some(BarInfo::Memory {
            address_type,
            address,
            size,
            ..
        }) = info.clone()
        {
            if size > 0 && address == 0 {
                let Some(new_addr) = allocator.alloc(size) else {
                    return Err(());
                };
                match address_type {
                    MemoryBarType::Width32 => root.set_bar_32(bdf, bar, new_addr as u32),
                    MemoryBarType::Width64 => root.set_bar_64(bdf, bar, new_addr),
                    MemoryBarType::Below1MiB => root.set_bar_32(bdf, bar, new_addr as u32),
                }
            }
        }
        let takes_two = info.as_ref().is_some_and(BarInfo::takes_two_entries);
        bar += if takes_two { 2 } else { 1 };
    }

    let (_status, cmd) = root.get_status_command(bdf);
    root.set_command(
        bdf,
        (cmd | Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER)
            & !Command::INTERRUPT_DISABLE,
    );
    Ok(())
}

fn pci_config_read_word(
    host: crate::bootinfo::PciHostResource,
    bdf: DeviceFunction,
    offset: usize,
) -> u32 {
    debug_assert!(offset % core::mem::size_of::<u32>() == 0);
    let addr = crate::platform::mmio_phys_to_virt(host.ecam.start)
        + ((bdf.bus as usize) << 20)
        + ((bdf.device as usize) << 15)
        + ((bdf.function as usize) << 12)
        + offset;
    unsafe { read_volatile(addr as *const u32) }
}

fn gpex_intx_irq(host: crate::bootinfo::PciHostResource, bdf: DeviceFunction) -> Option<u32> {
    let line_pin = pci_config_read_word(host, bdf, PCI_INTERRUPT_LINE_PIN_OFFSET);
    let interrupt_pin = ((line_pin >> 8) & 0xff) as u8;
    if interrupt_pin == 0 {
        return None;
    }

    host.intx_irq(bdf.device, interrupt_pin)
}

fn probe_virtio_pci_device(
    root: &mut PciRoot<MmioCam<'static>>,
    bdf: DeviceFunction,
    dev_info: &DeviceFunctionInfo,
) -> Option<PciTransport> {
    let dev_type = virtio_device_type(dev_info)?;
    match (dev_type, dev_info.device_id) {
        (DeviceType::Network, 0x1000) | (DeviceType::Network, 0x1040) => {}
        (DeviceType::Block, 0x1001) | (DeviceType::Block, 0x1041) => {}
        _ => return None,
    }
    info!("[pci] found virtio device {:?} at {}", dev_type, bdf);
    PciTransport::new::<crate::drivers::virtio::VirtioHal, MmioCam<'static>>(root, bdf).ok()
}
