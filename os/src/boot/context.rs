//! Immutable context published after early firmware discovery.

use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::boot::memblock::{MemBlock, PhysMemoryRegion, ReserveKind};
use crate::config::MAX_HARTS;
use crate::of::{DeviceResource, FdtBlob};
use crate::of::registry::{ClockResource, OfDeviceRegistry, PhyResource, MAX_CLOCK_RESOURCES, MAX_MMIO_REGIONS, MAX_PHY_DEVICES, MAX_VIRTIO_MMIO_DEVICES};

/// Failure while constructing the immutable early boot context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootError {
    /// No usable firmware device tree was supplied.
    MissingFdt,
    /// Firmware did not expose enabled RAM.
    MissingMemory,
    /// Firmware did not expose an enabled CPU node.
    MissingCpu,
    /// Firmware did not expose a usable timer frequency.
    MissingTimebase,
    /// Firmware did not expose an early console UART.
    MissingConsole,
}

/// Immutable firmware-derived state used by the rest of the kernel.
#[derive(Clone, Copy, Debug)]
pub struct BootContext {
    pub(crate) memblock: MemBlock,
    pub(crate) hart_count: usize,
    pub(crate) timer_frequency: usize,
    pub(crate) devices: OfDeviceRegistry,
    pub(crate) fdt: Option<FdtBlob>,
}

impl BootContext {
    /// Create an empty context while firmware discovery is in progress.
    pub(crate) const fn empty() -> Self {
        Self { memblock: MemBlock::empty(), hart_count: 0, timer_frequency: 0, devices: OfDeviceRegistry::empty(), fdt: None }
    }
    /// Return the immutable early physical-memory database.
    pub fn memblock(&self) -> &MemBlock { &self.memblock }
    /// Return OF-discovered device resources.
    pub fn devices(&self) -> &OfDeviceRegistry { &self.devices }
    /// Return the discovered hart count.
    pub fn hart_count(&self) -> usize { self.hart_count }
    /// Return the firmware timer frequency in ticks per second.
    pub fn timer_frequency(&self) -> usize { self.timer_frequency }
    /// Return the validated firmware FDT, when present.
    pub fn fdt(&self) -> Option<FdtBlob> { self.fdt }
    /// Return the validated FDT virtual address and total size, when present.
    pub fn fdt_blob(&self) -> Option<(usize, usize)> {
        self.fdt.map(|fdt| (fdt.virtual_address(), fdt.total_size()))
    }
    /// Register a firmware RAM range.
    pub(crate) fn push_memory_region(&mut self, start: usize, end: usize) {
        let start = crate::of::address::firmware_address_to_phys(start);
        let end = crate::of::address::firmware_address_to_phys(end);
        self.memblock.add_memory(start, end).expect("early RAM range capacity exhausted");
    }
    /// Register a firmware reservation.
    pub(crate) fn push_reserved_region(&mut self, start: usize, end: usize) {
        let start = crate::of::address::firmware_address_to_phys(start);
        let end = crate::of::address::firmware_address_to_phys(end);
        self.memblock.reserve(start, end, ReserveKind::Firmware).expect("early reservation capacity exhausted");
    }
    /// Set the discovered hart count.
    pub(crate) fn set_hart_count(&mut self, count: usize) { self.hart_count = count.clamp(1, MAX_HARTS); }
    /// Store the discovered timer frequency.
    pub(crate) fn set_timer_frequency(&mut self, frequency: usize) { self.timer_frequency = frequency; }
    /// Return the mutable device registry during early scanning.
    pub(crate) fn devices_mut(&mut self) -> &mut OfDeviceRegistry { &mut self.devices }
    /// Store the validated FDT handle.
    pub(crate) fn set_fdt(&mut self, fdt: FdtBlob) { self.fdt = Some(fdt); }

    /// Add an MMIO window to the permanent mapping list.
    pub(crate) fn push_mmio_region(&mut self, resource: DeviceResource) {
        if resource.size == 0 || self.devices.mmio_region_count >= MAX_MMIO_REGIONS { return; }
        let end = resource.start.saturating_add(resource.size);
        if resource.start >= end { return; }
        self.devices.mmio_regions[self.devices.mmio_region_count] = PhysMemoryRegion::new(resource.start, end);
        self.devices.mmio_region_count += 1;
    }
    /// Add a VirtIO-MMIO transport in stable physical-address order.
    pub(crate) fn push_virtio_mmio(&mut self, resource: DeviceResource) {
        if self.devices.virtio_mmio_count >= MAX_VIRTIO_MMIO_DEVICES { return; }
        let mut index = self.devices.virtio_mmio_count;
        while index != 0 && self.devices.virtio_mmio[index - 1].start > resource.start {
            self.devices.virtio_mmio[index] = self.devices.virtio_mmio[index - 1];
            index -= 1;
        }
        self.devices.virtio_mmio[index] = resource;
        self.devices.virtio_mmio_count += 1;
        self.push_mmio_region(resource);
    }
    /// Register a clock-provider relation discovered from OF.
    pub(crate) fn push_clock(&mut self, phandle: u32, parent_phandle: u32, frequency: usize) {
        if phandle == 0 || (parent_phandle == 0 && frequency == 0) || self.devices.clock_count >= MAX_CLOCK_RESOURCES { return; }
        self.devices.clocks[self.devices.clock_count] = ClockResource { phandle, parent_phandle, frequency };
        self.devices.clock_count += 1;
    }
    /// Register a PHY phandle-to-address mapping.
    pub(crate) fn push_phy(&mut self, phandle: u32, address: u8) {
        if phandle == 0 || self.devices.phy_count >= MAX_PHY_DEVICES { return; }
        self.devices.phys[self.devices.phy_count] = PhyResource { phandle, address };
        self.devices.phy_count += 1;
    }
    /// Resolve deferred GMAC-to-PHY links after the full tree was scanned.
    pub(crate) fn resolve_gmac_phys(&mut self) {
        for index in 0..self.devices.gmac_count {
            let Some(resource) = self.devices.gmac[index] else { continue; };
            let phy = resource.phy_handle().and_then(|handle| self.devices.phys[..self.devices.phy_count].iter().find(|phy| phy.phandle == handle).map(|phy| phy.address));
            self.devices.gmac[index] = Some(resource.with_phy_addr(phy));
        }
    }
    /// Resolve a timer frequency through the UART clock parent chain.
    pub(crate) fn resolve_timer_frequency(&mut self) {
        if self.timer_frequency != 0 || self.devices.uart_clock_phandle == 0 { return; }
        let mut phandle = self.devices.uart_clock_phandle;
        for _ in 0..self.devices.clock_count {
            let Some(clock) = self.devices.clocks[..self.devices.clock_count].iter().find(|clock| clock.phandle == phandle) else { break; };
            if clock.frequency != 0 { self.timer_frequency = clock.frequency; break; }
            if clock.parent_phandle == 0 || clock.parent_phandle == phandle { break; }
            phandle = clock.parent_phandle;
        }
    }
}

static READY: AtomicBool = AtomicBool::new(false);
static mut BOOT_CONTEXT: BootContext = BootContext::empty();

/// Publish the completed context exactly once before secondary harts proceed.
pub(crate) fn publish(context: BootContext) {
    unsafe { ptr::write(ptr::addr_of_mut!(BOOT_CONTEXT), context); }
    READY.store(true, Ordering::Release);
}

/// Return the published boot context.
pub fn get() -> &'static BootContext {
    assert!(READY.load(Ordering::Acquire), "boot context accessed before initialization");
    unsafe { &*ptr::addr_of!(BOOT_CONTEXT) }
}

/// Return the context only after it has been published.
pub fn try_get() -> Option<&'static BootContext> {
    READY.load(Ordering::Acquire).then(|| unsafe { &*ptr::addr_of!(BOOT_CONTEXT) })
}

/// Return the discovered hart count.
pub fn hart_count() -> usize { get().hart_count() }

/// Return the firmware timer frequency in ticks per second.
pub fn timer_frequency() -> usize { get().timer_frequency() }
