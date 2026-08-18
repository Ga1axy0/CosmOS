//! Temporary property state used while decoding one OF node.

use crate::boot::context::BootContext;
use crate::boot::memblock::PhysMemoryRegion;
use crate::of::block::MmcResource;
use crate::of::net::{GmacResource, PhyInterfaceMode, parse_phy_mode};
use crate::of::pci::PciHostResource;
use crate::of::registry::{MAX_GMAC_DEVICES, MAX_MMC_DEVICES};
use crate::of::DeviceResource;

#[derive(Clone, Copy, Default)]
pub(crate) struct NodeState {
    parent_is_cpus: bool,
    is_cpus: bool,
    is_cpu: bool,
    is_memory: bool,
    is_reserved_memory: bool,
    is_uart: bool,
    is_rtc: bool,
    is_plic: bool,
    is_virtio_mmio: bool,
    is_pch_pic: bool,
    is_eiointc: bool,
    is_pci_host: bool,
    is_ahci: bool,
    is_syscrg: bool,
    is_cache_controller: bool,
    is_gmac: bool,
    is_mdio: bool,
    is_ethernet_phy: bool,
    is_mmc: bool,
    address_cells: usize,
    size_cells: usize,
    child_address_cells: usize,
    child_size_cells: usize,
    status_ok: bool,
    timebase_frequency: usize,
    clock_frequency: usize,
    phandle: u32,
    clock_phandle: u32,
    phy_handle: u32,
    pinctrl_default: u32,
    phy_mode: PhyInterfaceMode,
    phy_reg: Option<u8>,
    irq: Option<u32>,
    mac_address: [u8; 6],
    has_mac_address: bool,
    bus_width: u32,
    no_sd: bool,
    no_mmc: bool,
    non_removable: bool,
    supports_1v8: bool,
    bus_start: u8,
    bus_end: u8,
    ranges_ptr: usize,
    ranges_len: usize,
    interrupt_map_ptr: usize,
    interrupt_map_len: usize,
    reg_regions: [PhysMemoryRegion; 4],
    reg_region_count: usize,
}

impl NodeState {
    pub(crate) fn for_child(parent: Self, name: &[u8]) -> Self {
        let is_cpus = name == b"cpus";
        let is_cpu = parent.is_cpus && starts_with(name, b"cpu@");
        let is_memory = name == b"memory" || starts_with(name, b"memory@");
        let is_reserved_memory = parent.is_reserved_memory || name == b"reserved-memory";
        let is_mdio = name == b"mdio" || starts_with(name, b"mdio@");
        let is_ethernet_phy = parent.is_mdio
            && (starts_with(name, b"ethernet-phy@") || starts_with(name, b"phy@"));
        Self {
            parent_is_cpus: parent.is_cpus,
            is_cpus,
            is_cpu,
            is_memory,
            is_reserved_memory,
            is_uart: false,
            is_rtc: false,
            is_plic: false,
            is_virtio_mmio: false,
            is_pch_pic: false,
            is_eiointc: false,
            is_pci_host: false,
            is_ahci: false,
            is_syscrg: false,
            is_cache_controller: false,
            is_gmac: false,
            is_mdio,
            is_ethernet_phy,
            is_mmc: false,
            address_cells: parent.child_address_cells.max(1),
            size_cells: parent.child_size_cells.max(1),
            child_address_cells: 2,
            child_size_cells: 1,
            status_ok: true,
            timebase_frequency: 0,
            clock_frequency: 0,
            phandle: 0,
            clock_phandle: 0,
            phy_handle: 0,
            pinctrl_default: 0,
            phy_mode: PhyInterfaceMode::Unknown,
            phy_reg: None,
            irq: None,
            mac_address: [0; 6],
            has_mac_address: false,
            bus_width: 1,
            no_sd: false,
            no_mmc: false,
            non_removable: false,
            supports_1v8: false,
            bus_start: 0,
            bus_end: 0,
            ranges_ptr: 0,
            ranges_len: 0,
            interrupt_map_ptr: 0,
            interrupt_map_len: 0,
            reg_regions: [PhysMemoryRegion::EMPTY; 4],
            reg_region_count: 0,
        }
    }

    pub(crate) fn apply_property(&mut self, name: &[u8], value: &[u8]) {
        match name {
            b"#address-cells" => self.child_address_cells = read_cells_usize(value, 1).unwrap_or(2),
            b"#size-cells" => self.child_size_cells = read_cells_usize(value, 1).unwrap_or(1),
            b"status" => {
                self.status_ok = value == b"okay\0" || value == b"ok\0" || value.is_empty()
            }
            b"device_type" if self.parent_is_cpus && value == b"cpu\0" => self.is_cpu = true,
            b"device_type" if value == b"memory\0" => self.is_memory = true,
            b"compatible" => {
                self.is_uart = compatible_contains(value, b"ns16550a")
                    || compatible_contains(value, b"ns16550")
                    || compatible_contains(value, b"snps,dw-apb-uart")
                    || compatible_contains(value, b"starfive,jh7110-uart");
                self.is_rtc = compatible_contains(value, b"google,goldfish-rtc")
                    || compatible_contains(value, b"loongson,ls7a-rtc")
                    || compatible_contains(value, b"loongson,ls2k-rtc")
                    || compatible_contains(value, b"loongson,ls2k1000-rtc")
                    || compatible_contains(value, b"loongson,ls-rtc");
                self.is_plic = compatible_contains(value, b"riscv,plic0")
                    || compatible_contains(value, b"sifive,plic-1.0.0");
                self.is_virtio_mmio = compatible_contains(value, b"virtio,mmio");
                self.is_pch_pic = compatible_contains(value, b"loongson,pch-pic-1.0");
                self.is_eiointc = compatible_contains(value, b"loongson,ls2k2000-eiointc");
                self.is_pci_host = compatible_contains(value, b"pci-host-ecam-generic");
                self.is_ahci = compatible_contains(value, b"snps,spear-ahci")
                    || compatible_contains(value, b"loongson,ls-ahci")
                    || compatible_contains(value, b"loongson,ls2k1000-ahci")
                    || compatible_contains(value, b"loongson,2k1000-ahci")
                    || compatible_contains(value, b"generic-ahci")
                    || compatible_contains(value, b"snps,dwc-ahci");
                self.is_syscrg = compatible_contains(value, b"starfive,jh7110-clkgen")
                    || compatible_contains(value, b"starfive,jh7110-reset");
                self.is_cache_controller = compatible_contains(value, b"starfive,jh7110-ccache")
                    || compatible_contains(value, b"sifive,fu740-c000-ccache");
                self.is_gmac = compatible_contains(value, b"snps,dwmac-3.70a")
                    || compatible_contains(value, b"snps,arc-dwmac-3.70a")
                    || compatible_contains(value, b"ls,ls-gmac")
                    || compatible_contains(value, b"starfive,jh7110-eqos-5.20")
                    || compatible_contains(value, b"starfive,jh7110-dwmac")
                    || compatible_contains(value, b"snps,dwmac-5.20");
                self.is_mdio |= compatible_contains(value, b"snps,dwmac-mdio");
                self.is_ethernet_phy |= compatible_contains(value, b"ethernet-phy-ieee802.3-c22")
                    || compatible_contains(value, b"ethernet-phy-ieee802.3-c45")
                    || compatible_prefix(value, b"ethernet-phy-id");
                // StarFive's SDK U-Boot control FDT describes both JH7110
                // SDIO controllers with the generic DesignWare binding, while
                // newer Linux device trees also carry the SoC-specific name.
                self.is_mmc = compatible_contains(value, b"starfive,jh7110-mmc")
                    || compatible_contains(value, b"snps,dw-mshc");
            }
            b"timebase-frequency" => {
                self.timebase_frequency = read_cells_usize(value, 1).unwrap_or(0)
            }
            b"clock-frequency" => self.clock_frequency = read_cells_usize(value, 1).unwrap_or(0),
            b"phandle" | b"linux,phandle" if value.len() >= 4 => {
                self.phandle = read_be_u32(&value[..4]).unwrap_or(0)
            }
            b"clocks" if value.len() >= 4 => {
                self.clock_phandle = read_be_u32(&value[..4]).unwrap_or(0)
            }
            b"phy-handle" if value.len() >= 4 => {
                self.phy_handle = read_be_u32(&value[..4]).unwrap_or(0)
            }
            b"pinctrl-0" if value.len() >= 4 => {
                self.pinctrl_default = read_be_u32(&value[..4]).unwrap_or(0)
            }
            b"phy-mode" => self.phy_mode = parse_phy_mode(value),
            b"interrupts" => self.irq = crate::of::irq::parse_interrupts(value),
            b"bus-width" => self.bus_width = read_cells_usize(value, 1).unwrap_or(1) as u32,
            b"no-sd" => self.no_sd = true,
            b"no-mmc" => self.no_mmc = true,
            b"non-removable" => self.non_removable = true,
            b"mmc-hs200-1_8v" | b"mmc-hs400-1_8v" | b"mmc-ddr-1_8v" | b"sd-uhs-sdr104"
            | b"sd-uhs-sdr50" | b"sd-uhs-ddr50" | b"sd-uhs-sdr25" | b"sd-uhs-sdr12" => {
                self.supports_1v8 = true
            }
            b"local-mac-address" | b"mac-address" if value.len() >= 6 => {
                let mut mac = [0u8; 6];
                mac.copy_from_slice(&value[..6]);
                if valid_unicast_mac(mac) {
                    self.mac_address = mac;
                    self.has_mac_address = true;
                }
            }
            b"bus-range" if value.len() >= 8 => {
                self.bus_start = read_be_u32(&value[..4]).unwrap_or(0) as u8;
                self.bus_end = read_be_u32(&value[4..8]).unwrap_or(0) as u8;
            }
            b"ranges" => {
                self.ranges_ptr = value.as_ptr() as usize;
                self.ranges_len = value.len();
            }
            b"interrupt-map" => {
                self.interrupt_map_ptr = value.as_ptr() as usize;
                self.interrupt_map_len = value.len();
            }
            b"reg" => {
                // PHY nodes conventionally use a single-cell `reg`.  Record
                // that first cell independently of property ordering: some
                // firmware emits `reg` before `compatible`.
                if self.phy_reg.is_none() {
                    self.phy_reg =
                        read_cells_usize(value, 1).and_then(|cell| u8::try_from(cell).ok());
                }
                parse_reg(value, self.address_cells, self.size_cells, |start, size| {
                    self.push_reg_region(start, size);
                });
            }
            _ => {}
        }
    }

    pub(crate) fn finish(&self, info: &mut BootContext) {
        if self.is_cpu && self.status_ok {
            info.set_hart_count(info.hart_count.saturating_add(1));
        }
        if !self.status_ok {
            return;
        }
        if self.is_cpus && self.timebase_frequency != 0 {
            info.timer_frequency = self.timebase_frequency;
        }
        if self.is_cpu && info.timer_frequency == 0 && self.clock_frequency != 0 {
            info.timer_frequency = self.clock_frequency;
        }
        if self.phandle != 0 && (self.clock_phandle != 0 || self.clock_frequency != 0) {
            info.push_clock(self.phandle, self.clock_phandle, self.clock_frequency);
        }
        if self.is_ethernet_phy {
            if let Some(address) = self.phy_reg {
                info.push_phy(self.phandle, address);
            }
        }
        for region in self.reg_regions[..self.reg_region_count].iter().copied() {
            if self.is_memory {
                info.push_memory_region(region.start, region.end);
            } else if self.is_reserved_memory {
                info.push_reserved_region(region.start, region.end);
            }
        }
        let Some(region) = self.reg_regions.first().copied() else {
            return;
        };
        if region.is_empty() {
            return;
        }
        let resource = DeviceResource {
            start: region.start,
            size: region.end - region.start,
            irq: self.irq,
        };
        if self.is_uart && info.devices.uart.is_none() {
            info.devices.uart = Some(resource);
            info.devices.uart_clock_phandle = self.clock_phandle;
            info.push_mmio_region(resource);
            // LoongArch QEMU and LS2K firmware describe the constant timer
            // clock on the UART node instead of /cpus/timebase-frequency.
            if info.timer_frequency == 0 && self.clock_frequency != 0 {
                info.timer_frequency = self.clock_frequency;
            }
        } else if self.is_rtc && info.devices.rtc.is_none() {
            info.devices.rtc = Some(resource);
            info.push_mmio_region(resource);
        } else if self.is_plic && info.devices.plic.is_none() {
            info.devices.plic = Some(resource);
            info.push_mmio_region(resource);
        } else if self.is_virtio_mmio {
            info.push_virtio_mmio(resource);
        } else if self.is_pch_pic && info.devices.pch_pic.is_none() {
            info.devices.pch_pic = Some(resource);
            info.push_mmio_region(resource);
        } else if self.is_eiointc && info.devices.eiointc.is_none() {
            info.devices.eiointc = Some(resource);
        } else if self.is_pci_host && info.devices.pci_host.is_none() {
            let mut host = PciHostResource::empty();
            host.ecam = resource;
            host.bus_start = self.bus_start;
            host.bus_end = self.bus_end;
            self.parse_pci_ranges(&mut host);
            self.parse_pci_interrupt_map(&mut host);
            info.devices.pci_host = Some(host);
            info.push_mmio_region(resource);
        } else if self.is_ahci && info.devices.ahci.is_none() {
            info.devices.ahci = Some(resource);
            info.push_mmio_region(resource);
        } else if self.is_syscrg && info.devices.syscrg.is_none() {
            // The clock and reset provider nodes expose the same SYSCRG
            // register window as their first `reg` tuple.
            info.devices.syscrg = Some(resource);
            info.push_mmio_region(resource);
        } else if self.is_cache_controller {
            // JH7110 uses the SiFive-compatible CCACHE FLUSH64 register for
            // non-coherent DMA maintenance, so its control window must be
            // mapped before the GMAC driver starts.
            info.push_mmio_region(resource);
        } else if self.is_gmac && info.devices.gmac_count < MAX_GMAC_DEVICES {
            let devices = &mut info.devices;
            devices.gmac[devices.gmac_count] = Some(GmacResource::new(
                resource,
                self.has_mac_address.then_some(self.mac_address),
                self.phy_mode,
                (self.phy_handle != 0).then_some(self.phy_handle),
                None,
                (self.pinctrl_default != 0).then_some(self.pinctrl_default),
            ));
            devices.gmac_count += 1;
            info.push_mmio_region(resource);
        } else if self.is_mmc && info.devices.mmc_count < MAX_MMC_DEVICES {
            let devices = &mut info.devices;
            devices.mmc[devices.mmc_count] = Some(MmcResource::new(
                resource,
                self.bus_width,
                self.no_sd,
                self.no_mmc,
                self.non_removable,
                self.supports_1v8,
            ));
            devices.mmc_count += 1;
            info.push_mmio_region(resource);
        }
    }

    fn push_reg_region(&mut self, start: usize, size: usize) {
        if size == 0 || self.reg_region_count >= self.reg_regions.len() {
            return;
        }
        let start = firmware_address_to_phys(start);
        let end = start.saturating_add(size);
        if start >= end {
            return;
        }
        self.reg_regions[self.reg_region_count] = PhysMemoryRegion::new(start, end);
        self.reg_region_count += 1;
    }

    fn parse_pci_ranges(&self, host: &mut PciHostResource) {
        let Some(mut value) = bytes_at(self.ranges_ptr, self.ranges_len) else {
            return;
        };
        if let Some((start, size)) = crate::of::address::parse_pci_memory_range(
            value,
            self.child_address_cells,
            self.address_cells,
            self.child_size_cells,
        ) {
            host.memory_start = firmware_address_to_phys(start);
            host.memory_size = size;
        }
    }

    fn parse_pci_interrupt_map(&self, host: &mut PciHostResource) {
        let Some(mut value) = bytes_at(self.interrupt_map_ptr, self.interrupt_map_len) else {
            return;
        };
        crate::of::irq::parse_pci_interrupt_map(value, host);
    }
}

fn valid_unicast_mac(mac: [u8; 6]) -> bool {
    mac != [0; 6] && mac != [0xff; 6] && mac[0] & 1 == 0
}

fn compatible_contains(value: &[u8], needle: &[u8]) -> bool {
    crate::of::compat::contains(value, needle)
}

fn compatible_prefix(value: &[u8], prefix: &[u8]) -> bool {
    crate::of::compat::has_prefix(value, prefix)
}

fn parse_reg(
    mut value: &[u8],
    address_cells: usize,
    size_cells: usize,
    mut f: impl FnMut(usize, usize),
) {
    crate::of::address::parse_reg(value, address_cells, size_cells, f)
}

/// Convert a firmware-described CPU address into the physical-address form
/// used internally. Some LoongArch U-Boot trees describe RAM and MMIO through
/// a DMW alias; QEMU and RISC-V trees already contain physical addresses.
fn firmware_address_to_phys(address: usize) -> usize {
    crate::of::address::firmware_address_to_phys(address)
}

fn read_cells_usize(value: &[u8], cells: usize) -> Option<usize> {
    crate::of::address::read_cells(value, cells)
}

fn starts_with(value: &[u8], prefix: &[u8]) -> bool {
    value.len() >= prefix.len() && &value[..prefix.len()] == prefix
}

fn bytes_at(addr: usize, len: usize) -> Option<&'static [u8]> {
    if addr == 0 {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(addr as *const u8, len) })
}

fn read_be_u32(bytes: &[u8]) -> Option<u32> {
    crate::of::address::read_be_u32(bytes)
}
