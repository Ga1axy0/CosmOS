//! Fixed-capacity Open Firmware device registry used before heap setup.

use crate::boot::memblock::PhysMemoryRegion;
use crate::of::block::MmcResource;
use crate::of::net::GmacResource;
use crate::of::pci::PciHostResource;
use crate::of::DeviceResource;

pub(crate) const MAX_MMIO_REGIONS: usize = 24;
pub(crate) const MAX_VIRTIO_MMIO_DEVICES: usize = 16;
pub(crate) const MAX_GMAC_DEVICES: usize = 4;
pub(crate) const MAX_PHY_DEVICES: usize = 8;
pub(crate) const MAX_MMC_DEVICES: usize = 4;
pub(crate) const MAX_CLOCK_RESOURCES: usize = 16;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PhyResource {
    pub(crate) phandle: u32,
    pub(crate) address: u8,
}

impl PhyResource {
    pub(crate) const EMPTY: Self = Self { phandle: 0, address: 0 };
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ClockResource {
    pub(crate) phandle: u32,
    pub(crate) parent_phandle: u32,
    pub(crate) frequency: usize,
}

/// OF-discovered device resources, separate from boot protocol state.
#[derive(Clone, Copy, Debug)]
pub struct OfDeviceRegistry {
    pub(crate) uart: Option<DeviceResource>,
    pub(crate) rtc: Option<DeviceResource>,
    pub(crate) plic: Option<DeviceResource>,
    pub(crate) pch_pic: Option<DeviceResource>,
    pub(crate) eiointc: Option<DeviceResource>,
    pub(crate) pci_host: Option<PciHostResource>,
    pub(crate) ahci: Option<DeviceResource>,
    pub(crate) syscrg: Option<DeviceResource>,
    pub(crate) gmac: [Option<GmacResource>; MAX_GMAC_DEVICES],
    pub(crate) gmac_count: usize,
    pub(crate) phys: [PhyResource; MAX_PHY_DEVICES],
    pub(crate) phy_count: usize,
    pub(crate) mmc: [Option<MmcResource>; MAX_MMC_DEVICES],
    pub(crate) mmc_count: usize,
    pub(crate) virtio_mmio: [DeviceResource; MAX_VIRTIO_MMIO_DEVICES],
    pub(crate) virtio_mmio_count: usize,
    pub(crate) mmio_regions: [PhysMemoryRegion; MAX_MMIO_REGIONS],
    pub(crate) mmio_region_count: usize,
    pub(crate) clocks: [ClockResource; MAX_CLOCK_RESOURCES],
    pub(crate) clock_count: usize,
    pub(crate) uart_clock_phandle: u32,
}

impl OfDeviceRegistry {
    /// Create an empty OF resource registry.
    pub(crate) const fn empty() -> Self {
        Self {
            uart: None, rtc: None, plic: None, pch_pic: None, eiointc: None,
            pci_host: None, ahci: None, syscrg: None,
            gmac: [None; MAX_GMAC_DEVICES], gmac_count: 0,
            phys: [PhyResource::EMPTY; MAX_PHY_DEVICES], phy_count: 0,
            mmc: [None; MAX_MMC_DEVICES], mmc_count: 0,
            virtio_mmio: [DeviceResource::empty(); MAX_VIRTIO_MMIO_DEVICES], virtio_mmio_count: 0,
            mmio_regions: [PhysMemoryRegion::EMPTY; MAX_MMIO_REGIONS], mmio_region_count: 0,
            clocks: [ClockResource { phandle: 0, parent_phandle: 0, frequency: 0 }; MAX_CLOCK_RESOURCES],
            clock_count: 0, uart_clock_phandle: 0,
        }
    }

    /// Return all enabled GMAC controllers in firmware order.
    pub fn gmac_devices(&self) -> impl Iterator<Item = GmacResource> + '_ {
        self.gmac[..self.gmac_count].iter().flatten().copied()
    }

    /// Return all enabled MMC controllers in firmware order.
    pub fn mmc_devices(&self) -> impl Iterator<Item = MmcResource> + '_ {
        self.mmc[..self.mmc_count].iter().flatten().copied()
    }

    /// Return all enabled VirtIO-MMIO transports.
    pub fn virtio_mmio_devices(&self) -> &[DeviceResource] {
        &self.virtio_mmio[..self.virtio_mmio_count]
    }

    /// Return the selected console UART resource.
    pub fn uart(&self) -> Option<DeviceResource> { self.uart }
    /// Return the RTC resource.
    pub fn rtc(&self) -> Option<DeviceResource> { self.rtc }
    /// Return the PLIC resource.
    pub fn plic(&self) -> Option<DeviceResource> { self.plic }
    /// Return the PCH PIC resource.
    pub fn pch_pic(&self) -> Option<DeviceResource> { self.pch_pic }
    /// Return the EIOINTC resource.
    pub fn eiointc(&self) -> Option<DeviceResource> { self.eiointc }
    /// Return the PCI host resource.
    pub fn pci_host(&self) -> Option<PciHostResource> { self.pci_host }
    /// Return the AHCI resource.
    pub fn ahci(&self) -> Option<DeviceResource> { self.ahci }
    /// Return the JH7110 SYSCRG resource.
    pub fn syscrg(&self) -> Option<DeviceResource> { self.syscrg }
    /// Return all MMIO windows requiring permanent mapping.
    pub fn mmio_regions(&self) -> &[PhysMemoryRegion] { &self.mmio_regions[..self.mmio_region_count] }
}
