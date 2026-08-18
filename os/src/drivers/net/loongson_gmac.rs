//! Loongson LS2K1000 DesignWare GMAC driver.
//!
//! The hardware-facing shape follows the repository's `ax-driver` GMAC HAL:
//! FDT supplies resources, a single lock owns the DMA rings, and smoltcp only
//! sees complete Ethernet frames through the generic kernel NIC wrapper.

use core::{mem::size_of, ptr::addr_of_mut};

use crate::{
    bootinfo::{GmacResource, PhyInterfaceMode},
    platform,
    sync::SpinNoIrqLock,
};

const DEVICE_NAME: &str = "ls2k1000-gmac";
const DEFAULT_MAC: [u8; 6] = [0x62, 0x19, 0x1a, 0x02, 0xa8, 0x91];

const DMA_OFFSET: usize = 0x1000;
const MAC_CONFIG: usize = 0x0000;
const MAC_FRAME_FILTER: usize = 0x0004;
const MAC_GMII_ADDR: usize = 0x0010;
const MAC_GMII_DATA: usize = 0x0014;
const MAC_FLOW_CONTROL: usize = 0x0018;
const MAC_VERSION: usize = 0x0020;
const MAC_INTERRUPT_STATUS: usize = 0x0038;
const MAC_ADDR0_HIGH: usize = 0x0040;
const MAC_ADDR0_LOW: usize = 0x0044;
const MAC_RGSMII_STATUS: usize = 0x00d8;
const MAC_MMC_INTR_MASK_RX: usize = 0x010c;
const MAC_MMC_INTR_MASK_TX: usize = 0x0110;
const MAC_MMC_RX_IPC_INTR_MASK: usize = 0x0200;

const DMA_BUS_MODE: usize = 0x0000;
const DMA_TX_POLL_DEMAND: usize = 0x0004;
const DMA_RX_POLL_DEMAND: usize = 0x0008;
const DMA_RX_BASE_ADDR: usize = 0x000c;
const DMA_TX_BASE_ADDR: usize = 0x0010;
const DMA_STATUS: usize = 0x0014;
const DMA_CONTROL: usize = 0x0018;
const DMA_INTERRUPT: usize = 0x001c;
const DMA_AXI_BUS_MODE: usize = 0x0028;

const GMII_BUSY: u32 = 1 << 0;
const GMII_CSR_CLK4: u32 = 1 << 4;
const GMII_REG_SHIFT: u32 = 6;
const GMII_DEV_SHIFT: u32 = 11;
const PHY_ADDR_MAX: u8 = 31;

const MAC_RX: u32 = 1 << 2;
const MAC_TX: u32 = 1 << 3;
const MAC_DEFERRAL_CHECK: u32 = 0x0000_0010;
const MAC_BACKOFF_LIMIT: u32 = 0x0000_0060;
const MAC_PAD_CRC_STRIP: u32 = 0x0000_0080;
const MAC_RETRY: u32 = 0x0000_0200;
const MAC_DUPLEX: u32 = 1 << 11;
const MAC_LOOPBACK: u32 = 0x0000_1000;
const MAC_RX_OWN: u32 = 0x0000_2000;
const MAC_SPEED_100: u32 = 1 << 14;
const MAC_PORT_SELECT: u32 = 1 << 15;
const MAC_TX_CONFIG: u32 = 1 << 24;
const MAC_JUMBO_FRAME: u32 = 0x0010_0000;
const MAC_FRAME_BURST: u32 = 0x0020_0000;
const MAC_JABBER: u32 = 0x0040_0000;
const MAC_WATCHDOG: u32 = 0x0080_0000;
const MAC_FILTER: u32 = 1 << 31;
const MAC_PROMISCUOUS_MODE: u32 = 0x0000_0001;
const MAC_UCAST_HASH_FILTER: u32 = 0x0000_0002;
const MAC_MCAST_HASH_FILTER: u32 = 0x0000_0004;
const MAC_DEST_ADDR_FILTER: u32 = 0x0000_0008;
const MAC_MULTICAST_FILTER: u32 = 0x0000_0010;
const MAC_BROADCAST: u32 = 0x0000_0020;
const MAC_PASS_CONTROL: u32 = 0x0000_00c0;
const MAC_SRC_ADDR_FILTER: u32 = 0x0000_0200;
const MAC_TX_FLOW_CONTROL: u32 = 0x0000_0002;
const MAC_RX_FLOW_CONTROL: u32 = 0x0000_0004;
const MAC_PAUSE_TIME_MASK: u32 = 0xffff_0000;

const LINK_DUPLEX: u32 = 1 << 0;
const LINK_SPEED_100: u32 = 1 << 1;
const LINK_SPEED_1000: u32 = 1 << 2;
const LINK_SPEED_MASK: u32 = LINK_SPEED_100 | LINK_SPEED_1000;
const LINK_UP: u32 = 1 << 3;

const DMA_RESET: u32 = 1 << 0;
const DMA_BURST_LENGTH32: u32 = 0x0000_2000;
const DMA_BURST_LENGTHX8: u32 = 0x0100_0000;
const DMA_MIXED_BURST_ENABLE: u32 = 0x0400_0000;
const DMA_RX_START: u32 = 1 << 1;
const DMA_TX_SECOND_FRAME: u32 = 1 << 2;
const DMA_EN_HW_FLOW_CTRL: u32 = 0x0000_0100;
const DMA_RX_FLOW_CTRL_ACT: u32 = 0x0080_0600;
const DMA_RX_FLOW_CTRL_DEACT: u32 = 0x0040_1800;
const DMA_TX_START: u32 = 1 << 13;
const DMA_STORE_AND_FORWARD: u32 = 0x0220_0000;

const DMA_INT_TX_COMPLETED: u32 = 1 << 0;
const DMA_INT_TX_STOPPED: u32 = 1 << 1;
const DMA_INT_TX_NO_BUFFER: u32 = 1 << 2;
const DMA_INT_RX_OVERFLOW: u32 = 1 << 4;
const DMA_INT_TX_UNDERFLOW: u32 = 1 << 5;
const DMA_INT_RX_COMPLETED: u32 = 1 << 6;
const DMA_INT_RX_NO_BUFFER: u32 = 1 << 7;
const DMA_INT_RX_STOPPED: u32 = 1 << 8;
const DMA_INT_BUS_ERROR: u32 = 1 << 13;
const DMA_INT_ABNORMAL: u32 = 1 << 15;
const DMA_INT_NORMAL: u32 = 1 << 16;
const DMA_INT_ENABLE: u32 = DMA_INT_NORMAL
    | DMA_INT_ABNORMAL
    | DMA_INT_BUS_ERROR
    | DMA_INT_RX_STOPPED
    | DMA_INT_RX_NO_BUFFER
    | DMA_INT_RX_COMPLETED
    | DMA_INT_TX_UNDERFLOW
    | DMA_INT_RX_OVERFLOW
    | DMA_INT_TX_NO_BUFFER
    | DMA_INT_TX_STOPPED
    | DMA_INT_TX_COMPLETED;

const DESC_SIZE_MASK: u32 = 0x1fff;
const RX_DESC_END_OF_RING: u32 = 1 << 15;
const TX_DESC_END_OF_RING: u32 = 1 << 21;
const DESC_TX_FIRST: u32 = 1 << 28;
const DESC_TX_LAST: u32 = 1 << 29;
const DESC_TX_INTERRUPT: u32 = 1 << 30;
const DESC_RX_LAST: u32 = 1 << 8;
const DESC_RX_FIRST: u32 = 1 << 9;
const DESC_ERROR: u32 = 1 << 15;
const DESC_FRAME_LENGTH_MASK: u32 = 0x3fff_0000;
const DESC_FRAME_LENGTH_SHIFT: u32 = 16;
const DESC_OWNED_BY_DMA: u32 = 1 << 31;

// Kept in sync with tgoskits' verified LS2K1000 GMAC implementation.
const RING_SIZE: usize = 128;
const BUFFER_SIZE: usize = 2048;
const RESET_TIMEOUT: usize = 1_000_000;
const MDIO_TIMEOUT: usize = 100_000;

#[repr(C)]
#[derive(Clone, Copy)]
struct DmaDesc {
    status: u32,
    length: u32,
    buffer1: u32,
    buffer2: u32,
}

impl DmaDesc {
    const ZERO: Self = Self {
        status: 0,
        length: 0,
        buffer1: 0,
        buffer2: 0,
    };
}

#[repr(C, align(64))]
struct GmacDmaStorage {
    tx_desc: [DmaDesc; RING_SIZE],
    rx_desc: [DmaDesc; RING_SIZE],
    tx_buf: [[u8; BUFFER_SIZE]; RING_SIZE],
    rx_buf: [[u8; BUFFER_SIZE]; RING_SIZE],
    marker: u64,
}

impl GmacDmaStorage {
    const fn zeroed() -> Self {
        Self {
            tx_desc: [DmaDesc::ZERO; RING_SIZE],
            rx_desc: [DmaDesc::ZERO; RING_SIZE],
            tx_buf: [[0; BUFFER_SIZE]; RING_SIZE],
            rx_buf: [[0; BUFFER_SIZE]; RING_SIZE],
            // Keep this allocation in an ELF PROGBITS section. Unlike `.bss`,
            // firmware loads it before the CPU can create dirty cached aliases.
            marker: 0x474d_4143_444d_4121,
        }
    }
}

#[link_section = ".data.gmac_dma"]
static mut DMA_STORAGE: GmacDmaStorage = GmacDmaStorage::zeroed();

#[derive(Clone, Copy)]
struct Mmio(*mut u8);

impl Mmio {
    fn read(self, offset: usize) -> u32 {
        unsafe { self.0.add(offset).cast::<u32>().read_volatile() }
    }

    fn write(self, offset: usize, value: u32) {
        unsafe { self.0.add(offset).cast::<u32>().write_volatile(value) }
    }

    fn set(self, offset: usize, bits: u32) {
        self.write(offset, self.read(offset) | bits);
    }

    fn clear(self, offset: usize, bits: u32) {
        self.write(offset, self.read(offset) & !bits);
    }
}

unsafe impl Send for Mmio {}
unsafe impl Sync for Mmio {}

#[derive(Clone, Copy)]
struct Registers {
    mac: Mmio,
    dma: Mmio,
}

impl Registers {
    fn new(base: usize) -> Self {
        let base = base as *mut u8;
        Self {
            mac: Mmio(base),
            dma: Mmio(unsafe { base.add(DMA_OFFSET) }),
        }
    }

    fn wait_mdio(&self) -> bool {
        for _ in 0..MDIO_TIMEOUT {
            if self.mac.read(MAC_GMII_ADDR) & GMII_BUSY == 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    fn mdio_read(&self, phy_addr: u8, register: u32) -> Option<u16> {
        if !self.wait_mdio() {
            return None;
        }
        self.mac.write(
            MAC_GMII_ADDR,
            ((phy_addr as u32) << GMII_DEV_SHIFT)
                | (register << GMII_REG_SHIFT)
                | GMII_CSR_CLK4
                | GMII_BUSY,
        );
        self.wait_mdio()
            .then(|| self.mac.read(MAC_GMII_DATA) as u16)
    }

    fn discover_phy(&self) -> Option<u8> {
        for phy_addr in 0..=PHY_ADDR_MAX {
            let Some(high) = self.mdio_read(phy_addr, 2) else {
                continue;
            };
            let Some(low) = self.mdio_read(phy_addr, 3) else {
                continue;
            };
            if high != 0 && high != u16::MAX && low != 0 && low != u16::MAX {
                return Some(phy_addr);
            }
        }
        None
    }

    fn reset_dma(&self) -> bool {
        let before = self.dma.read(DMA_BUS_MODE);
        self.dma.write(DMA_BUS_MODE, DMA_RESET);
        let requested = self.dma.read(DMA_BUS_MODE);
        println!(
            "[net][gmac] DMA reset request: bus_mode={:#010x}->{:#010x}, \
             dma_status={:#010x}, dma_control={:#010x}, mac_config={:#010x}, rgmii={:#010x}",
            before,
            requested,
            self.dma.read(DMA_STATUS),
            self.dma.read(DMA_CONTROL),
            self.mac.read(MAC_CONFIG),
            self.mac.read(MAC_RGSMII_STATUS),
        );
        for _ in 0..RESET_TIMEOUT {
            if self.dma.read(DMA_BUS_MODE) & DMA_RESET == 0 {
                println!(
                    "[net][gmac] DMA reset complete: bus_mode={:#010x}, dma_status={:#010x}",
                    self.dma.read(DMA_BUS_MODE),
                    self.dma.read(DMA_STATUS),
                );
                return true;
            }
            core::hint::spin_loop();
        }
        println!(
            "[net][gmac] DMA reset TIMEOUT: bus_mode={:#010x}, dma_status={:#010x}, \
             dma_control={:#010x}, mac_config={:#010x}, mac_intr={:#010x}, rgmii={:#010x}, \
             gmii_addr={:#010x}, gmii_data={:#010x}",
            self.dma.read(DMA_BUS_MODE),
            self.dma.read(DMA_STATUS),
            self.dma.read(DMA_CONTROL),
            self.mac.read(MAC_CONFIG),
            self.mac.read(MAC_INTERRUPT_STATUS),
            self.mac.read(MAC_RGSMII_STATUS),
            self.mac.read(MAC_GMII_ADDR),
            self.mac.read(MAC_GMII_DATA),
        );
        false
    }

    /// Configure the MAC side of the FDT-selected PHY interface before the
    /// DesignWare DMA reset.  In particular, MII needs PORT_SELECT set for
    /// the reset handshake to see the PHY clock; RGMII and RMII clear it.
    fn select_phy_mode(&self, mode: PhyInterfaceMode) {
        match mode {
            PhyInterfaceMode::Mii => self.mac.set(MAC_CONFIG, MAC_PORT_SELECT),
            PhyInterfaceMode::Rmii
            | PhyInterfaceMode::Rgmii
            | PhyInterfaceMode::RgmiiId
            | PhyInterfaceMode::RgmiiRxId
            | PhyInterfaceMode::RgmiiTxId => self.mac.clear(MAC_CONFIG, MAC_PORT_SELECT),
            PhyInterfaceMode::Unknown => {}
        }
        println!("[net][gmac] FDT PHY mode: {:?}, mac_config={:#010x}", mode, self.mac.read(MAC_CONFIG));
    }

    fn station_address(&self) -> Option<[u8; 6]> {
        let high = self.mac.read(MAC_ADDR0_HIGH);
        let low = self.mac.read(MAC_ADDR0_LOW);
        let mac = [
            low as u8,
            (low >> 8) as u8,
            (low >> 16) as u8,
            (low >> 24) as u8,
            high as u8,
            (high >> 8) as u8,
        ];
        valid_mac(mac).then_some(mac)
    }

    fn set_station_address(&self, mac: [u8; 6]) {
        self.mac
            .write(MAC_ADDR0_HIGH, ((mac[5] as u32) << 8) | mac[4] as u32);
        self.mac.write(
            MAC_ADDR0_LOW,
            ((mac[3] as u32) << 24)
                | ((mac[2] as u32) << 16)
                | ((mac[1] as u32) << 8)
                | mac[0] as u32,
        );
    }

    fn link(&self) -> LinkState {
        LinkState::from_raw(self.mac.read(MAC_RGSMII_STATUS))
    }

    fn configure_link(&self, link: LinkState) {
        let mut config = self.mac.read(MAC_CONFIG);
        config &= !(MAC_PORT_SELECT | MAC_SPEED_100 | MAC_DUPLEX);
        if link.full_duplex {
            config |= MAC_DUPLEX;
        }
        match link.speed_mbps {
            1000 => {}
            100 => config |= MAC_PORT_SELECT | MAC_SPEED_100,
            _ => config |= MAC_PORT_SELECT,
        }
        self.mac.write(MAC_CONFIG, config);
    }

    /// Register sequence copied from tgoskits' verified LS2K1000 GMAC driver.
    fn init_dma_regs(&self, tx_base: u32, rx_base: u32) {
        self.dma.write(
            DMA_BUS_MODE,
            DMA_MIXED_BURST_ENABLE | DMA_BURST_LENGTHX8 | DMA_BURST_LENGTH32,
        );
        self.dma
            .write(DMA_CONTROL, DMA_STORE_AND_FORWARD | DMA_TX_SECOND_FRAME);
        self.dma.write(DMA_AXI_BUS_MODE, 0xff | (0x77 << 16));
        self.dma.write(DMA_TX_BASE_ADDR, tx_base);
        self.dma.write(DMA_RX_BASE_ADDR, rx_base);
    }

    /// Register sequence copied from tgoskits' verified LS2K1000 GMAC driver.
    fn init_mac_regs(&self) {
        self.mac.set(MAC_CONFIG, MAC_TX_CONFIG);
        self.mac.clear(
            MAC_CONFIG,
            MAC_WATCHDOG
                | MAC_JABBER
                | MAC_FRAME_BURST
                | MAC_JUMBO_FRAME
                | MAC_RX_OWN
                | MAC_LOOPBACK
                | MAC_RETRY
                | MAC_PAD_CRC_STRIP
                | MAC_DEFERRAL_CHECK
                | MAC_BACKOFF_LIMIT,
        );
        self.mac.set(MAC_CONFIG, MAC_DUPLEX);

        self.mac.clear(
            MAC_FRAME_FILTER,
            MAC_SRC_ADDR_FILTER
                | MAC_BROADCAST
                | MAC_MULTICAST_FILTER
                | MAC_DEST_ADDR_FILTER
                | MAC_MCAST_HASH_FILTER
                | MAC_UCAST_HASH_FILTER
                | MAC_PROMISCUOUS_MODE
                | MAC_PASS_CONTROL,
        );
        self.mac.set(MAC_FRAME_FILTER, MAC_FILTER);

        let mut dma_control = self.dma.read(DMA_CONTROL);
        dma_control &= !(DMA_RX_FLOW_CTRL_ACT | DMA_RX_FLOW_CTRL_DEACT | DMA_EN_HW_FLOW_CTRL);
        self.dma.write(DMA_CONTROL, dma_control);

        let mut flow_control = MAC_PAUSE_TIME_MASK;
        flow_control &= !(MAC_RX_FLOW_CONTROL | MAC_TX_FLOW_CONTROL);
        self.mac.write(MAC_FLOW_CONTROL, flow_control);
    }

    /// Clear all controller-side latched state before enabling interrupts.
    fn clear_pending_irq(&self) {
        self.mac.write(MAC_MMC_INTR_MASK_TX, u32::MAX);
        self.mac.write(MAC_MMC_INTR_MASK_RX, u32::MAX);
        self.mac.write(MAC_MMC_RX_IPC_INTR_MASK, u32::MAX);
        self.dma.write(DMA_STATUS, self.dma.read(DMA_STATUS));
    }

    fn start(&self) {
        self.mac.set(MAC_CONFIG, MAC_RX | MAC_TX);
        self.dma.set(DMA_CONTROL, DMA_RX_START | DMA_TX_START);
        dma_barrier();
        self.dma.write(DMA_RX_POLL_DEMAND, 0);
    }

    fn stop(&self) {
        self.dma.clear(DMA_CONTROL, DMA_RX_START | DMA_TX_START);
        self.mac.clear(MAC_CONFIG, MAC_RX | MAC_TX);
        dma_barrier();
    }
}

#[derive(Clone, Copy)]
struct LinkState {
    raw: u32,
    up: bool,
    speed_mbps: u32,
    full_duplex: bool,
}

impl LinkState {
    fn from_raw(raw: u32) -> Self {
        Self {
            raw,
            up: raw & LINK_UP != 0,
            speed_mbps: match raw & LINK_SPEED_MASK {
                LINK_SPEED_1000 => 1000,
                LINK_SPEED_100 => 100,
                _ => 10,
            },
            full_duplex: raw & LINK_DUPLEX != 0,
        }
    }
}

struct GmacState {
    regs: Registers,
    tx_next: usize,
    tx_reclaim: usize,
    rx_next: usize,
    tx_in_use: [bool; RING_SIZE],
    tx_packets: u64,
    rx_packets: u64,
}

/// A single LS2K1000 GMAC network device.
pub(crate) struct LoongsonGmacDevice {
    irq: u32,
    mac: [u8; 6],
    state: SpinNoIrqLock<GmacState>,
}

impl LoongsonGmacDevice {
    pub(crate) fn try_new(resource: GmacResource) -> Option<Self> {
        let device = resource.device();
        println!(
            "[net][gmac] begin: pa={:#x} size={:#x} irq={:?} fdt_mac={:?}",
            device.start,
            device.size,
            device.irq,
            resource.mac_address(),
        );
        if device.size < DMA_OFFSET + 0x100 {
            println!(
                "[net][gmac] reject resource: minimum_size={:#x}",
                DMA_OFFSET + 0x100,
            );
            warn!(
                "{DEVICE_NAME}: unsupported resource size at base={:#x}: {:#x}",
                device.start, device.size
            );
            return None;
        }
        let Some(irq) = device.irq else {
            println!("[net][gmac] reject resource: FDT supplied no IRQ");
            return None;
        };
        let regs = Registers::new(platform::mmio_phys_to_virt(device.start));
        let version = regs.mac.read(MAC_VERSION);
        let inherited_mac = regs.station_address();
        let mac = resource
            .mac_address()
            .or(inherited_mac)
            .unwrap_or(DEFAULT_MAC);

        let phy_addr = resource.phy_addr().or_else(|| {
            println!("[net][gmac] FDT has no phy-handle; scanning MDIO addresses 0..31");
            regs.discover_phy()
        });
        let Some(phy_addr) = phy_addr else {
            println!("[net][gmac] no valid PHY found through FDT or MDIO scan");
            return None;
        };
        let phy_id = regs
            .mdio_read(phy_addr, 2)
            .zip(regs.mdio_read(phy_addr, 3))
            .map(|(high, low)| ((high as u32) << 16) | low as u32);
        let link = regs.link();
        println!(
            "[kernel] {}: version={:#x} phy_addr={} phy={:?} link={} {}Mbps {}",
            DEVICE_NAME,
            version,
            phy_addr,
            phy_id,
            if link.up { "up" } else { "down" },
            link.speed_mbps,
            if link.full_duplex {
                "full-duplex"
            } else {
                "half-duplex"
            },
        );

        regs.stop();
        regs.select_phy_mode(resource.phy_mode());
        regs.dma.write(DMA_INTERRUPT, 0);
        if !regs.reset_dma() {
            error!("{DEVICE_NAME}: DMA reset timed out");
            return None;
        }
        regs.set_station_address(mac);

        let storage = dma_storage();
        let tx_desc = unsafe { addr_of_mut!((*storage).tx_desc) }.cast::<DmaDesc>();
        let rx_desc = unsafe { addr_of_mut!((*storage).rx_desc) }.cast::<DmaDesc>();
        let rx_buf = unsafe { addr_of_mut!((*storage).rx_buf) }.cast::<u8>();
        let Some(tx_desc_pa) = dma_addr32(tx_desc.cast()) else {
            println!("[net][gmac] reject TX ring: VA {:#x} has no 32-bit DMA address", tx_desc as usize);
            return None;
        };
        let Some(rx_desc_pa) = dma_addr32(rx_desc.cast()) else {
            println!("[net][gmac] reject RX ring: VA {:#x} has no 32-bit DMA address", rx_desc as usize);
            return None;
        };
        println!(
            "[net][gmac] DMA memory: storage_va={:#x} tx_desc_pa={:#010x} \
             rx_desc_pa={:#010x} rx_buf_va={:#x}",
            storage as usize,
            tx_desc_pa,
            rx_desc_pa,
            rx_buf as usize,
        );

        for index in 0..RING_SIZE {
            let tx_end = if index + 1 == RING_SIZE {
                TX_DESC_END_OF_RING
            } else {
                0
            };
            let rx_end = if index + 1 == RING_SIZE {
                RX_DESC_END_OF_RING
            } else {
                0
            };
            unsafe {
                tx_desc.add(index).write_volatile(DmaDesc {
                    status: tx_end,
                    ..DmaDesc::ZERO
                });
                rx_desc.add(index).write_volatile(DmaDesc {
                    status: DESC_OWNED_BY_DMA,
                    length: BUFFER_SIZE as u32 | rx_end,
                    buffer1: match dma_addr32(rx_buf.add(index * BUFFER_SIZE)) {
                        Some(address) => address,
                        None => {
                            println!(
                                "[net][gmac] reject RX buffer {}: VA {:#x} has no 32-bit DMA address",
                                index,
                                rx_buf.add(index * BUFFER_SIZE) as usize,
                            );
                            return None;
                        }
                    },
                    buffer2: 0,
                });
            }
        }
        dma_barrier();

        // Keep the post-reset hardware sequence aligned with the verified
        // tgoskits LS2K1000 implementation.  The surrounding state object is
        // CosmOS-specific glue for its NetworkDevice interface.
        regs.init_dma_regs(tx_desc_pa, rx_desc_pa);
        regs.init_mac_regs();
        regs.configure_link(link);
        regs.clear_pending_irq();
        regs.dma.write(DMA_INTERRUPT, DMA_INT_ENABLE);
        regs.start();

        println!(
            "[kernel] {}: irq={} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} tx_ring={:#x} rx_ring={:#x}",
            DEVICE_NAME,
            irq,
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5],
            tx_desc_pa,
            rx_desc_pa
        );

        Some(Self {
            irq,
            mac,
            state: SpinNoIrqLock::new(GmacState {
                regs,
                tx_next: 0,
                tx_reclaim: 0,
                rx_next: 0,
                tx_in_use: [false; RING_SIZE],
                tx_packets: 0,
                rx_packets: 0,
            }),
        })
    }

    pub(crate) fn irq(&self) -> u32 {
        self.irq
    }

    pub(crate) fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    pub(crate) fn handle_irq(&self) {
        let mut state = self.state.lock();
        let status = state.regs.dma.read(DMA_STATUS);
        if status == 0 || status == u32::MAX {
            return;
        }
        state.regs.dma.write(DMA_STATUS, status);
        if status & DMA_INT_BUS_ERROR != 0 {
            error!("{DEVICE_NAME}: fatal DMA bus error status={status:#010x}");
        }
        if status & DMA_INT_RX_STOPPED != 0 {
            warn!("{DEVICE_NAME}: RX stopped; restarting DMA");
            state.regs.dma.set(DMA_CONTROL, DMA_RX_START);
            state.regs.dma.write(DMA_RX_POLL_DEMAND, 0);
        }
        if status & (1 << 26) != 0 {
            let _ = state.regs.mac.read(MAC_INTERRUPT_STATUS);
            let link = state.regs.link();
            state.regs.configure_link(link);
            info!(
                "[kernel] {DEVICE_NAME}: link={} {}Mbps {} status={:#x}",
                if link.up { "up" } else { "down" },
                link.speed_mbps,
                if link.full_duplex {
                    "full-duplex"
                } else {
                    "half-duplex"
                },
                link.raw
            );
        }
        reclaim_tx(&mut state);
    }

    pub(crate) fn can_send(&self) -> bool {
        let mut state = self.state.lock();
        reclaim_tx(&mut state);
        let index = state.tx_next;
        !state.tx_in_use[index] && state.regs.link().up
    }

    pub(crate) fn try_send(&self, frame: &[u8]) -> bool {
        if frame.is_empty() || frame.len() > BUFFER_SIZE || frame.len() > DESC_SIZE_MASK as usize {
            warn!("{DEVICE_NAME}: invalid TX frame length {}", frame.len());
            return false;
        }
        let mut state = self.state.lock();
        reclaim_tx(&mut state);
        if !state.regs.link().up {
            return false;
        }
        let index = state.tx_next;
        if state.tx_in_use[index] {
            return false;
        }

        let storage = dma_storage();
        let desc = unsafe { addr_of_mut!((*storage).tx_desc[index]) };
        if unsafe { desc.read_volatile() }.status & DESC_OWNED_BY_DMA != 0 {
            return false;
        }
        let buffer = unsafe { addr_of_mut!((*storage).tx_buf[index]) }.cast::<u8>();
        unsafe { buffer.copy_from_nonoverlapping(frame.as_ptr(), frame.len()) };
        let end = if index + 1 == RING_SIZE {
            TX_DESC_END_OF_RING
        } else {
            0
        };
        unsafe {
            desc.write_volatile(DmaDesc {
                status: DESC_TX_INTERRUPT | DESC_TX_FIRST | DESC_TX_LAST | end,
                length: frame.len() as u32,
                buffer1: dma_addr32(buffer).expect("GMAC TX buffer moved above 4GiB"),
                buffer2: 0,
            });
        }
        dma_barrier();
        unsafe {
            addr_of_mut!((*desc).status).write_volatile(
                DESC_OWNED_BY_DMA | DESC_TX_INTERRUPT | DESC_TX_FIRST | DESC_TX_LAST | end,
            )
        };
        dma_barrier();
        state.tx_in_use[index] = true;
        state.tx_next = ring_next(index);
        state.tx_packets += 1;
        state.regs.dma.write(DMA_TX_POLL_DEMAND, 0);
        true
    }

    pub(crate) fn try_recv(&self, out: &mut [u8]) -> Option<usize> {
        let mut state = self.state.lock();
        for _ in 0..RING_SIZE {
            let index = state.rx_next;
            let storage = dma_storage();
            let desc_ptr = unsafe { addr_of_mut!((*storage).rx_desc[index]) };
            let desc = unsafe { desc_ptr.read_volatile() };
            if desc.status & DESC_OWNED_BY_DMA != 0 {
                return None;
            }
            dma_barrier();

            let valid = desc.status & DESC_ERROR == 0
                && desc.status & DESC_RX_FIRST != 0
                && desc.status & DESC_RX_LAST != 0;
            let frame_len =
                ((desc.status & DESC_FRAME_LENGTH_MASK) >> DESC_FRAME_LENGTH_SHIFT) as usize;
            let copy_len = frame_len.min(out.len()).min(BUFFER_SIZE);
            if valid && copy_len != 0 {
                let buffer = unsafe { addr_of_mut!((*storage).rx_buf[index]) }.cast::<u8>();
                unsafe { out.as_mut_ptr().copy_from_nonoverlapping(buffer, copy_len) };
            }

            let end = if index + 1 == RING_SIZE {
                RX_DESC_END_OF_RING
            } else {
                0
            };
            unsafe {
                desc_ptr.write_volatile(DmaDesc {
                    status: DESC_OWNED_BY_DMA,
                    length: BUFFER_SIZE as u32 | end,
                    buffer1: dma_addr32(addr_of_mut!((*storage).rx_buf[index]).cast::<u8>())
                        .expect("GMAC RX buffer moved above 4GiB"),
                    buffer2: 0,
                });
            }
            dma_barrier();
            state.rx_next = ring_next(index);
            state.regs.dma.write(DMA_RX_POLL_DEMAND, 0);

            if valid && copy_len != 0 {
                state.rx_packets += 1;
                return Some(copy_len);
            }
            warn!(
                "{DEVICE_NAME}: dropped RX descriptor status={:#010x} len={frame_len}",
                desc.status
            );
        }
        None
    }
}

fn reclaim_tx(state: &mut GmacState) {
    loop {
        let index = state.tx_reclaim;
        if !state.tx_in_use[index] {
            return;
        }
        let storage = dma_storage();
        let desc_ptr = unsafe { addr_of_mut!((*storage).tx_desc[index]) };
        let desc = unsafe { desc_ptr.read_volatile() };
        if desc.status & DESC_OWNED_BY_DMA != 0 {
            return;
        }
        let end = if index + 1 == RING_SIZE {
            TX_DESC_END_OF_RING
        } else {
            0
        };
        unsafe {
            desc_ptr.write_volatile(DmaDesc {
                status: end,
                ..DmaDesc::ZERO
            })
        };
        state.tx_in_use[index] = false;
        state.tx_reclaim = ring_next(index);
    }
}

fn dma_addr32(ptr: *const u8) -> Option<u32> {
    let paddr = platform::translate_direct_mapped_kernel_va(ptr as usize)?;
    u32::try_from(paddr).ok()
}

fn dma_storage() -> *mut GmacDmaStorage {
    let cached = addr_of_mut!(DMA_STORAGE) as usize;
    let paddr = platform::direct_map_virt_to_phys(cached);
    platform::mmio_phys_to_virt(paddr) as *mut GmacDmaStorage
}

fn valid_mac(mac: [u8; 6]) -> bool {
    mac != [0; 6] && mac != [0xff; 6] && mac[0] & 1 == 0
}

fn ring_next(index: usize) -> usize {
    (index + 1) % RING_SIZE
}

#[inline]
fn dma_barrier() {
    unsafe { core::arch::asm!("dbar 0", options(nostack, preserves_flags)) };
}

const _: () = assert!(size_of::<DmaDesc>() == 16);
