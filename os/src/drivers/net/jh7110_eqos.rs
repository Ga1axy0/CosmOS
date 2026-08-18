// SPDX-License-Identifier: GPL-2.0-or-later
//! Synopsys EQoS 5.20 driver for the JH7110 GMAC.
//!
//! The MAC/DMA path is a direct Rust port of StarFive U-Boot
//! `drivers/net/dwc_eth_qos.c` from branch `JH7110_VisionFive2_devel`, commit
//! `c4c67bb66ae6f41c98537d18cf5c3abc8b97b8e4`.  Cache maintenance, locking,
//! address translation, and the RX completion interrupt are the CosmOS
//! adapter around that polling driver.

mod phy;
mod soc;

use core::{mem::size_of, ptr::addr_of_mut};

use crate::{of::net::GmacResource, platform, sync::SpinNoIrqLock, timer::get_time_ns};

use phy::PhyLink;
use soc::SocControl;

const DEVICE_NAME: &str = "jh7110-eqos1";
const PREFERRED_PADDR: usize = 0x1604_0000;
const OTHER_PADDR: usize = 0x1603_0000;

const MAC_CONFIGURATION: usize = 0x0000;
const MAC_PACKET_FILTER: usize = 0x0008;
const MAC_Q0_TX_FLOW_CONTROL: usize = 0x0070;
const MAC_RX_FLOW_CONTROL: usize = 0x0090;
const MAC_TXQ_PRIORITY_MAP0: usize = 0x0098;
const MAC_RXQ_CTRL0: usize = 0x00a0;
const MAC_RXQ_CTRL1: usize = 0x00a4;
const MAC_RXQ_CTRL2: usize = 0x00a8;
const MAC_US_TIC_COUNTER: usize = 0x00dc;
const MAC_VERSION: usize = 0x0110;
const MAC_HW_FEATURE1: usize = 0x0120;
const MAC_MDIO_ADDRESS: usize = 0x0200;
const MAC_MDIO_DATA: usize = 0x0204;
const MAC_ADDRESS0_HIGH: usize = 0x0300;
const MAC_ADDRESS0_LOW: usize = 0x0304;

const MTL_TXQ0_OPERATION_MODE: usize = 0x0d00;
const MTL_TXQ0_DEBUG: usize = 0x0d08;
const MTL_TXQ0_QUANTUM_WEIGHT: usize = 0x0d18;
const MTL_RXQ0_OPERATION_MODE: usize = 0x0d30;

const DMA_MODE: usize = 0x1000;
const DMA_SYSBUS_MODE: usize = 0x1004;
const DMA_CH0_CONTROL: usize = 0x1100;
const DMA_CH0_TX_CONTROL: usize = 0x1104;
const DMA_CH0_RX_CONTROL: usize = 0x1108;
const DMA_CH0_TXDESC_LIST_HI: usize = 0x1110;
const DMA_CH0_TXDESC_LIST_LO: usize = 0x1114;
const DMA_CH0_RXDESC_LIST_HI: usize = 0x1118;
const DMA_CH0_RXDESC_LIST_LO: usize = 0x111c;
const DMA_CH0_TXDESC_TAIL: usize = 0x1120;
const DMA_CH0_RXDESC_TAIL: usize = 0x1128;
const DMA_CH0_TXDESC_RING_LENGTH: usize = 0x112c;
const DMA_CH0_RXDESC_RING_LENGTH: usize = 0x1130;
const DMA_CH0_INTERRUPT_ENABLE: usize = 0x1134;
const DMA_CH0_STATUS: usize = 0x1160;

// DWMAC MMC counters.  Keep these as raw hardware values: reset-on-read is a
// programmable MMC control bit and the firmware leaves it disabled here.
const MMC_CONTROL: usize = 0x0700;
const MMC_TX_FRAMECOUNT_GB: usize = 0x0718;
const MMC_TX_UNDERFLOW_ERROR: usize = 0x0748;
const MMC_TX_LATE_COLLISION: usize = 0x0758;
const MMC_TX_CARRIER_ERROR: usize = 0x0760;
const MMC_TX_FRAMECOUNT_G: usize = 0x0768;
const MMC_RX_FRAMECOUNT_GB: usize = 0x0780;
const MMC_RX_CRC_ERROR: usize = 0x0794;

// JH7110's GMAC is not DMA coherent.  The U74 cores do not implement Zicbom;
// Linux therefore maintains DMA coherency through the SiFive-compatible L2
// cache controller's FLUSH64 register.
const CCACHE_PADDR: usize = 0x0201_0000;
const CCACHE_FLUSH64: usize = 0x0200;
const CCACHE_LINE_SIZE: usize = 64;

const MAC_CONFIG_TE: u32 = 1 << 1;
const MAC_CONFIG_RE: u32 = 1 << 0;
const MAC_CONFIG_GPSLCE: u32 = 1 << 23;
const MAC_CONFIG_CST: u32 = 1 << 21;
const MAC_CONFIG_ACS: u32 = 1 << 20;
const MAC_CONFIG_WD: u32 = 1 << 19;
const MAC_CONFIG_JD: u32 = 1 << 17;
const MAC_CONFIG_JE: u32 = 1 << 16;
const MAC_CONFIG_PS: u32 = 1 << 15;
const MAC_CONFIG_FES: u32 = 1 << 14;
const MAC_CONFIG_DM: u32 = 1 << 13;
const MAC_PACKET_FILTER_PROMISCUOUS: u32 = 1 << 0;
const MAC_RXQ0_ENABLE_MASK: u32 = 0x3;
const MAC_RXQ0_ENABLE_DCB: u32 = 0x2;
const MAC_RXQ_CTRL1_MCBCQEN: u32 = 1 << 20;
const MAC_Q0_TX_FLOW_PAUSE_TIME: u32 = 0xffff << 16;
const MAC_Q0_TX_FLOW_ENABLE: u32 = 1 << 1;
const MAC_RX_FLOW_ENABLE: u32 = 1;

const MTL_TX_TQS_SHIFT: u32 = 16;
const MTL_TX_TQS_MASK: u32 = 0x1ff << MTL_TX_TQS_SHIFT;
const MTL_TXQ_ENABLE_SHIFT: u32 = 2;
const MTL_TXQ_ENABLE_MASK: u32 = 0x3 << MTL_TXQ_ENABLE_SHIFT;
const MTL_TXQ_ENABLE: u32 = 0x2 << MTL_TXQ_ENABLE_SHIFT;
const MTL_TX_STORE_FORWARD: u32 = 1 << 1;
const MTL_RX_RQS_SHIFT: u32 = 20;
const MTL_RX_RQS_MASK: u32 = 0x3ff << MTL_RX_RQS_SHIFT;
const MTL_RX_RFD_SHIFT: u32 = 14;
const MTL_RX_RFD_MASK: u32 = 0x3f << MTL_RX_RFD_SHIFT;
const MTL_RX_RFA_SHIFT: u32 = 8;
const MTL_RX_RFA_MASK: u32 = 0x3f << MTL_RX_RFA_SHIFT;
const MTL_RX_ENHANCED_FLOW_CONTROL: u32 = 1 << 7;
const MTL_RX_STORE_FORWARD: u32 = 1 << 5;

const DMA_SYSBUS_RD_OSR_LMT_SHIFT: u32 = 16;
const DMA_SYSBUS_EAME: u32 = 1 << 11;
const DMA_SYSBUS_BLEN16: u32 = 1 << 3;
const DMA_SYSBUS_BLEN8: u32 = 1 << 2;
const DMA_SYSBUS_BLEN4: u32 = 1 << 1;
const DMA_CH0_DSL_SHIFT: u32 = 18;
const DMA_CH0_DSL: u32 = ((CCACHE_LINE_SIZE - 16) / 8) as u32;
const DMA_CH0_PBLX8: u32 = 1 << 16;
const DMA_TX_PBL_SHIFT: u32 = 16;
const DMA_TX_OSP: u32 = 1 << 4;
const DMA_TX_START: u32 = 1 << 0;
const DMA_RX_PBL_SHIFT: u32 = 16;
const DMA_RX_BUFFER_SIZE_SHIFT: u32 = 1;
const DMA_RX_START: u32 = 1 << 0;
const DMA_MODE_SWR: u32 = 1;

const MDIO_PHY_SHIFT: u32 = 21;
const MDIO_REG_SHIFT: u32 = 16;
const MDIO_CR_250_300: u32 = 5 << 8;
const MDIO_GOC_READ: u32 = 3 << 2;
const MDIO_GOC_WRITE: u32 = 1 << 2;
const MDIO_BUSY: u32 = 1;

const DESC_OWN: u32 = 1 << 31;
const RX_DESC_INTERRUPT_ON_COMPLETION: u32 = 1 << 30;
const DESC_FIRST: u32 = 1 << 29;
const DESC_LAST: u32 = 1 << 28;
const DESC_BUF1_VALID: u32 = 1 << 24;
const RX_DESC_ERROR: u32 = 1 << 15;
const RX_DESC_LENGTH_MASK: u32 = 0x7fff;

// DWMAC 4.10 channel interrupt layout used by the JH7110 EQoS core.
const DMA_CH_STATUS_NORMAL: u32 = 1 << 15;
const DMA_CH_STATUS_ABNORMAL: u32 = 1 << 14;
const DMA_CH_STATUS_FATAL_BUS_ERROR: u32 = 1 << 12;
const DMA_CH_STATUS_RX: u32 = 1 << 6;
const DMA_CH_INTERRUPT_DEFAULT: u32 = DMA_CH_STATUS_NORMAL
    | DMA_CH_STATUS_ABNORMAL
    | DMA_CH_STATUS_FATAL_BUS_ERROR
    | DMA_CH_STATUS_RX;
const DMA_CH_INTERRUPT_COMPLETION: u32 = DMA_CH_STATUS_RX;

const RING_SIZE: usize = 4;
const BUFFER_SIZE: usize = 1600;
const RX_DMA_PBL: u32 = 8;
const MDIO_TIMEOUT_NS: u64 = 10_000_000;
const DMA_RESET_TIMEOUT_NS: u64 = 50_000_000;
const TX_COMPLETE_POLLS: usize = 1_000_000;

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct EqosDesc {
    des0: u32,
    des1: u32,
    des2: u32,
    des3: u32,
    reserved: [u32; 12],
}

impl EqosDesc {
    const ZERO: Self = Self {
        des0: 0,
        des1: 0,
        des2: 0,
        des3: 0,
        reserved: [0; 12],
    };
}

#[repr(C, align(64))]
struct EqosDmaStorage {
    tx_desc: [EqosDesc; RING_SIZE],
    rx_desc: [EqosDesc; RING_SIZE],
    tx_buf: [u8; BUFFER_SIZE],
    rx_buf: [[u8; BUFFER_SIZE]; RING_SIZE],
    marker: u64,
}

impl EqosDmaStorage {
    const fn zeroed() -> Self {
        Self {
            tx_desc: [EqosDesc::ZERO; RING_SIZE],
            rx_desc: [EqosDesc::ZERO; RING_SIZE],
            tx_buf: [0; BUFFER_SIZE],
            rx_buf: [[0; BUFFER_SIZE]; RING_SIZE],
            // Keep the DMA allocation in a firmware-loaded PROGBITS section.
            marker: 0x4a48_3731_3130_4551,
        }
    }
}

#[link_section = ".data.eqos_dma"]
static mut DMA_STORAGE: EqosDmaStorage = EqosDmaStorage::zeroed();

#[derive(Clone, Copy)]
struct Registers(*mut u8);

impl Registers {
    fn read(self, offset: usize) -> u32 {
        unsafe { self.0.add(offset).cast::<u32>().read_volatile() }
    }

    fn write(self, offset: usize, value: u32) {
        unsafe { self.0.add(offset).cast::<u32>().write_volatile(value) }
    }

    fn update(self, offset: usize, clear: u32, set: u32) {
        self.write(offset, (self.read(offset) & !clear) | set);
    }

    fn mdio_wait(self) -> bool {
        let deadline = get_time_ns().saturating_add(MDIO_TIMEOUT_NS);
        while get_time_ns() < deadline {
            if self.read(MAC_MDIO_ADDRESS) & MDIO_BUSY == 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    fn mdio_read(self, phy: u32, reg: u32) -> Option<u16> {
        if !self.mdio_wait() {
            return None;
        }
        self.write(
            MAC_MDIO_ADDRESS,
            (phy << MDIO_PHY_SHIFT)
                | (reg << MDIO_REG_SHIFT)
                | MDIO_CR_250_300
                | MDIO_GOC_READ
                | MDIO_BUSY,
        );
        self.mdio_wait()
            .then(|| (self.read(MAC_MDIO_DATA) & 0xffff) as u16)
    }

    fn mdio_write(self, phy: u32, reg: u32, value: u16) -> bool {
        if !self.mdio_wait() {
            return false;
        }
        self.write(MAC_MDIO_DATA, value as u32);
        self.write(
            MAC_MDIO_ADDRESS,
            (phy << MDIO_PHY_SHIFT)
                | (reg << MDIO_REG_SHIFT)
                | MDIO_CR_250_300
                | MDIO_GOC_WRITE
                | MDIO_BUSY,
        );
        self.mdio_wait()
    }

    fn wait_dma_reset_clear(self) -> bool {
        let deadline = get_time_ns().saturating_add(DMA_RESET_TIMEOUT_NS);
        while get_time_ns() < deadline {
            if self.read(DMA_MODE) & DMA_MODE_SWR == 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }

    fn configure_link(self, link: PhyLink) {
        let mut set = 0;
        if link.full_duplex {
            set |= MAC_CONFIG_DM;
        }
        match link.speed_mbps {
            1000 => {}
            100 => set |= MAC_CONFIG_PS | MAC_CONFIG_FES,
            _ => set |= MAC_CONFIG_PS,
        }
        self.update(
            MAC_CONFIGURATION,
            MAC_CONFIG_PS | MAC_CONFIG_FES | MAC_CONFIG_DM,
            set,
        );
    }

    fn station_address(self) -> Option<[u8; 6]> {
        let high = self.read(MAC_ADDRESS0_HIGH);
        let low = self.read(MAC_ADDRESS0_LOW);
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

    fn set_station_address(self, mac: [u8; 6]) {
        self.write(MAC_ADDRESS0_HIGH, ((mac[5] as u32) << 8) | mac[4] as u32);
        self.write(
            MAC_ADDRESS0_LOW,
            ((mac[3] as u32) << 24)
                | ((mac[2] as u32) << 16)
                | ((mac[1] as u32) << 8)
                | mac[0] as u32,
        );
    }
}

unsafe impl Send for Registers {}
unsafe impl Sync for Registers {}

struct EqosState {
    regs: Registers,
    soc: SocControl,
    link: PhyLink,
    link_poll: u32,
    tx_next: usize,
    rx_next: usize,
}

/// One JH7110 EQoS port using kernel-owned interrupt-driven DMA rings.
pub(crate) struct Jh7110EqosDevice {
    irq: u32,
    mac: [u8; 6],
    state: SpinNoIrqLock<EqosState>,
}

impl Jh7110EqosDevice {
    pub(crate) fn try_new(resource: GmacResource) -> Option<Self> {
        let device = resource.device();
        if !matches!(device.start, PREFERRED_PADDR | OTHER_PADDR)
            || device.size < DMA_CH0_STATUS + size_of::<u32>()
        {
            println!(
                "[jh7110-eqos] unsupported resource pa={:#x} size={:#x}",
                device.start, device.size
            );
            return None;
        }

        let irq = device.irq?;
        let soc = SocControl::start_gmac1()?;
        spin_delay_ns(10_000);
        let regs = Registers(platform::mmio_phys_to_virt(device.start) as *mut u8);
        // StarFive's start path only waits for the reset bit released by the
        // reset controller.  It does not initiate an extra DMA software reset.
        if !regs.wait_dma_reset_clear() {
            println!("[jh7110-eqos] DMA software-reset bit remained asserted");
            return None;
        }
        let version = regs.read(MAC_VERSION);
        if version == 0 || version == u32::MAX {
            println!("[jh7110-eqos] inaccessible MAC version={:#x}", version);
            return None;
        }
        let tick_rate = soc.gtx_rate_hz();
        if tick_rate < 1_000_000 {
            println!("[jh7110-eqos] invalid GTX tick clock rate {}Hz", tick_rate);
            return None;
        }
        regs.write(MAC_US_TIC_COUNTER, tick_rate / 1_000_000 - 1);

        let mac = resource
            .mac_address()
            .or_else(|| regs.station_address())
            .unwrap_or([0x02, 0x00, 0x00, 0x16, 0x04, 0x00]);
        let link = phy::discover(regs)?;
        if !soc.set_tx_speed(link.speed_mbps) {
            println!(
                "[jh7110-eqos] unsupported negotiated PHY speed {}Mbps",
                link.speed_mbps
            );
            return None;
        }
        println!(
            "[jh7110-eqos] pa={:#x} version={:#x} mac={:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} phy={} id={:#010x} link={} {}Mbps {} status={:#06x}",
            device.start,
            version,
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
            link.address,
            link.id,
            if link.up { "up" } else { "down" },
            link.speed_mbps,
            if link.full_duplex { "full-duplex" } else { "half-duplex" },
            link.status,
        );

        // The IRQ registers are the only CosmOS-specific setup at this point;
        // MAC, MTL, and DMA programming below follows U-Boot's start path.
        regs.write(DMA_CH0_INTERRUPT_ENABLE, 0);
        regs.write(DMA_CH0_STATUS, u32::MAX);

        let feature1 = regs.read(MAC_HW_FEATURE1);
        let tx_fifo = 128u32 << ((feature1 >> 6) & 0x1f);
        let rx_fifo = 128u32 << (feature1 & 0x1f);
        let tqs = (tx_fifo / 256).saturating_sub(1).min(0x1ff);
        let rqs = (rx_fifo / 256).saturating_sub(1).min(0x3ff);
        // StarFive U-Boot: store-and-forward on both queues, queue 0 enabled,
        // and the complete on-chip FIFO assigned to queue 0.
        regs.update(
            MTL_TXQ0_OPERATION_MODE,
            MTL_TX_TQS_MASK | MTL_TXQ_ENABLE_MASK,
            (tqs << MTL_TX_TQS_SHIFT) | MTL_TXQ_ENABLE | MTL_TX_STORE_FORWARD,
        );
        regs.write(MTL_TXQ0_QUANTUM_WEIGHT, 0x10);
        regs.update(
            MTL_RXQ0_OPERATION_MODE,
            MTL_RX_RQS_MASK,
            (rqs << MTL_RX_RQS_SHIFT) | MTL_RX_STORE_FORWARD,
        );
        // This branch is inactive on VF2's 2 KiB RX FIFO, but is retained
        // exactly from U-Boot for hardware reporting 4 KiB or more.
        if rqs >= (4096 / 256) - 1 {
            let (rfd, rfa) = match rqs {
                15 => (0x3, 0x1),
                31 => (0x6, 0xa),
                63 => (0x6, 0x12),
                _ => (0x6, 0x1e),
            };
            regs.update(
                MTL_RXQ0_OPERATION_MODE,
                MTL_RX_RFD_MASK | MTL_RX_RFA_MASK,
                MTL_RX_ENHANCED_FLOW_CONTROL
                    | (rfd << MTL_RX_RFD_SHIFT)
                    | (rfa << MTL_RX_RFA_SHIFT),
            );
        }
        regs.update(MAC_RXQ_CTRL0, MAC_RXQ0_ENABLE_MASK, MAC_RXQ0_ENABLE_DCB);
        regs.update(MAC_RXQ_CTRL1, 0, MAC_RXQ_CTRL1_MCBCQEN);
        regs.update(MAC_PACKET_FILTER, 0, MAC_PACKET_FILTER_PROMISCUOUS);
        regs.update(
            MAC_Q0_TX_FLOW_CONTROL,
            0,
            MAC_Q0_TX_FLOW_PAUSE_TIME | MAC_Q0_TX_FLOW_ENABLE,
        );
        regs.update(MAC_RX_FLOW_CONTROL, 0, MAC_RX_FLOW_ENABLE);
        regs.update(MAC_TXQ_PRIORITY_MAP0, 0xff, 0);
        regs.update(MAC_RXQ_CTRL2, 0xff, 0);
        regs.update(
            MAC_CONFIGURATION,
            MAC_CONFIG_GPSLCE | MAC_CONFIG_WD | MAC_CONFIG_JD | MAC_CONFIG_JE,
            MAC_CONFIG_CST | MAC_CONFIG_ACS,
        );
        regs.configure_link(link);
        regs.set_station_address(mac);

        // U-Boot enables OSP, uses a 1600-byte receive buffer, PBLx8, a TX
        // PBL derived from FIFO size (capped at 32), and RX PBL 8.
        let tx_pbl = (tqs + 1).min(32);
        regs.update(DMA_CH0_TX_CONTROL, 0, DMA_TX_OSP);
        regs.update(
            DMA_CH0_RX_CONTROL,
            0x3fff << DMA_RX_BUFFER_SIZE_SHIFT,
            (BUFFER_SIZE as u32) << DMA_RX_BUFFER_SIZE_SHIFT,
        );
        regs.update(
            DMA_CH0_CONTROL,
            0,
            DMA_CH0_PBLX8 | (DMA_CH0_DSL << DMA_CH0_DSL_SHIFT),
        );
        regs.update(
            DMA_CH0_TX_CONTROL,
            0x3f << DMA_TX_PBL_SHIFT,
            tx_pbl << DMA_TX_PBL_SHIFT,
        );
        regs.update(
            DMA_CH0_RX_CONTROL,
            0x3f << DMA_RX_PBL_SHIFT,
            RX_DMA_PBL << DMA_RX_PBL_SHIFT,
        );
        regs.write(
            DMA_SYSBUS_MODE,
            (2 << DMA_SYSBUS_RD_OSR_LMT_SHIFT)
                | DMA_SYSBUS_EAME
                | DMA_SYSBUS_BLEN16
                | DMA_SYSBUS_BLEN8
                | DMA_SYSBUS_BLEN4,
        );

        let storage = dma_storage();
        let tx_desc = unsafe { addr_of_mut!((*storage).tx_desc) }.cast::<EqosDesc>();
        let rx_desc = unsafe { addr_of_mut!((*storage).rx_desc) }.cast::<EqosDesc>();
        let tx_desc_pa = dma_address(tx_desc.cast())?;
        let rx_desc_pa = dma_address(rx_desc.cast())?;

        for index in 0..RING_SIZE {
            let rx_buffer = unsafe { addr_of_mut!((*storage).rx_buf[index]) }.cast::<u8>();
            let rx_buffer_pa = dma_address(rx_buffer)?;
            unsafe {
                tx_desc.add(index).write_volatile(EqosDesc::ZERO);
                rx_desc.add(index).write_volatile(EqosDesc::ZERO);
                addr_of_mut!((*rx_desc.add(index)).des0).write_volatile(rx_buffer_pa as u32);
                addr_of_mut!((*rx_desc.add(index)).des1)
                    .write_volatile((rx_buffer_pa >> 32) as u32);
                dma_barrier();
                // IOC is the sole descriptor-level CosmOS adapter: U-Boot
                // polls RX, while CosmOS needs an IRQ to wake its net worker.
                addr_of_mut!((*rx_desc.add(index)).des3).write_volatile(
                    DESC_OWN | DESC_BUF1_VALID | RX_DESC_INTERRUPT_ON_COMPLETION,
                );
            }
        }
        dma_cache_flush(storage.cast(), size_of::<EqosDmaStorage>());

        regs.write(DMA_CH0_TXDESC_LIST_HI, (tx_desc_pa >> 32) as u32);
        regs.write(DMA_CH0_TXDESC_LIST_LO, tx_desc_pa as u32);
        regs.write(DMA_CH0_RXDESC_LIST_HI, (rx_desc_pa >> 32) as u32);
        regs.write(DMA_CH0_RXDESC_LIST_LO, rx_desc_pa as u32);
        regs.write(DMA_CH0_TXDESC_RING_LENGTH, (RING_SIZE - 1) as u32);
        regs.write(DMA_CH0_RXDESC_RING_LENGTH, (RING_SIZE - 1) as u32);
        // U-Boot deliberately leaves the TX tail untouched until first send.
        regs.write(
            DMA_CH0_RXDESC_TAIL,
            rx_desc_pa.wrapping_add(((RING_SIZE - 1) * size_of::<EqosDesc>()) as u64) as u32,
        );

        regs.update(DMA_CH0_TX_CONTROL, 0, DMA_TX_START);
        regs.update(DMA_CH0_RX_CONTROL, 0, DMA_RX_START);
        regs.update(MAC_CONFIGURATION, 0, MAC_CONFIG_TE | MAC_CONFIG_RE);
        regs.write(DMA_CH0_STATUS, u32::MAX);
        regs.write(DMA_CH0_INTERRUPT_ENABLE, DMA_CH_INTERRUPT_DEFAULT);
        dma_barrier();

        println!(
            "[jh7110-eqos] StarFive U-Boot DMA port online irq={:?} tx_ring={:#x} rx_ring={:#x} fifo={}K/{}K pblx8=on pbl={}/{} store-forward",
            irq,
            tx_desc_pa,
            rx_desc_pa,
            tx_fifo / 1024,
            rx_fifo / 1024,
            tx_pbl,
            RX_DMA_PBL,
        );

        Some(Self {
            irq,
            mac,
            state: SpinNoIrqLock::new(EqosState {
                regs,
                soc,
                link,
                link_poll: 0,
                tx_next: 0,
                rx_next: 0,
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
        let state = self.state.lock();
        let status = state.regs.read(DMA_CH0_STATUS);
        let enabled = state.regs.read(DMA_CH0_INTERRUPT_ENABLE);
        let pending = status & enabled;
        if pending != 0 && status != u32::MAX && enabled != u32::MAX {
            // Match Linux stmmac: acknowledge only enabled causes. Status that
            // arrives while a completion source is masked remains latched and
            // will assert the channel IRQ when deferred polling re-enables it.
            state.regs.write(DMA_CH0_STATUS, pending);
            if pending & DMA_CH_INTERRUPT_COMPLETION != 0 {
                // The deferred poll masks RIE. Keep normal/abnormal summary and
                // fatal-bus-error reporting enabled while the ring is drained.
                state
                    .regs
                    .update(DMA_CH0_INTERRUPT_ENABLE, DMA_CH_INTERRUPT_COMPLETION, 0);
            }
        }
    }

    /// Re-enable channel interrupts after the deferred network poll completes.
    ///
    /// Return `true` when a completion became pending while RIE was
    /// masked.  Rechecking after the enable closes the NAPI re-arm race on
    /// systems where an already-latched channel status does not create a new
    /// interrupt edge.
    pub(crate) fn complete_poll(&self) -> bool {
        let state = self.state.lock();
        state
            .regs
            .update(DMA_CH0_INTERRUPT_ENABLE, 0, DMA_CH_INTERRUPT_COMPLETION);
        dma_barrier();

        let status = state.regs.read(DMA_CH0_STATUS);
        let enabled = state.regs.read(DMA_CH0_INTERRUPT_ENABLE);
        let pending = status & enabled;
        if pending & DMA_CH_INTERRUPT_COMPLETION == 0 || status == u32::MAX || enabled == u32::MAX {
            return false;
        }

        // Consume the raced completion exactly as the hard-IRQ path does,
        // then keep RIE masked until the worker drains the ring again.
        state.regs.write(DMA_CH0_STATUS, pending);
        state
            .regs
            .update(DMA_CH0_INTERRUPT_ENABLE, DMA_CH_INTERRUPT_COMPLETION, 0);
        true
    }

    #[cfg(feature = "net_perf_counters")]
    pub(crate) fn hardware_debug(&self) -> super::NetHardwareDebug {
        let state = self.state.lock();
        let regs = state.regs;

        let storage = dma_storage();
        let mut tx_owned_by_dma = 0;
        let mut tx_error_summary = 0;
        for index in 0..RING_SIZE {
            let desc_ptr = unsafe { addr_of_mut!((*storage).tx_desc[index]) };
            dma_cache_flush(desc_ptr.cast(), size_of::<EqosDesc>());
            let desc = unsafe { desc_ptr.read_volatile() };
            if desc.des3 & DESC_OWN != 0 {
                tx_owned_by_dma += 1;
            } else if desc.des3 & (1 << 15) != 0 {
                tx_error_summary += 1;
            }
        }

        super::NetHardwareDebug {
            mmc_control: regs.read(MMC_CONTROL),
            mmc_tx_frames_gb: regs.read(MMC_TX_FRAMECOUNT_GB) as u64,
            mmc_tx_good_frames: regs.read(MMC_TX_FRAMECOUNT_G) as u64,
            mmc_tx_underflow: regs.read(MMC_TX_UNDERFLOW_ERROR) as u64,
            mmc_tx_late_collision: regs.read(MMC_TX_LATE_COLLISION) as u64,
            mmc_tx_carrier_error: regs.read(MMC_TX_CARRIER_ERROR) as u64,
            mmc_rx_frames_gb: regs.read(MMC_RX_FRAMECOUNT_GB) as u64,
            mmc_rx_crc_error: regs.read(MMC_RX_CRC_ERROR) as u64,
            dma_status: regs.read(DMA_CH0_STATUS),
            dma_interrupt_enable: regs.read(DMA_CH0_INTERRUPT_ENABLE),
            mtl_txq_debug: regs.read(MTL_TXQ0_DEBUG),
            tx_tail: regs.read(DMA_CH0_TXDESC_TAIL),
            tx_next: state.tx_next,
            tx_owned_by_dma,
            tx_error_summary,
        }
    }

    pub(crate) fn can_send(&self) -> bool {
        let mut state = self.state.lock();
        state.link_poll = state.link_poll.wrapping_add(1);
        if state.link_poll & 0xff == 0 {
            refresh_link(&mut state);
        }
        let storage = dma_storage();
        let desc_ptr = unsafe { addr_of_mut!((*storage).tx_desc[state.tx_next]) };
        dma_cache_flush(desc_ptr.cast(), size_of::<EqosDesc>());
        let desc = unsafe { desc_ptr.read_volatile() };
        state.link.up && desc.des3 & DESC_OWN == 0
    }

    pub(crate) fn try_send(&self, frame: &[u8]) -> bool {
        if frame.is_empty() || frame.len() > BUFFER_SIZE || frame.len() > 0x7fff {
            return false;
        }
        let mut state = self.state.lock();
        if !state.link.up {
            return false;
        }
        let index = state.tx_next;
        let storage = dma_storage();
        let desc = unsafe { addr_of_mut!((*storage).tx_desc[index]) };
        dma_cache_flush(desc.cast(), size_of::<EqosDesc>());
        let old_des3 = unsafe { desc.read_volatile() }.des3;
        if old_des3 & DESC_OWN != 0 {
            return false;
        }
        // StarFive U-Boot intentionally uses one shared TX DMA buffer and
        // waits for completion before returning, so re-use is serialized.
        let buffer = unsafe { addr_of_mut!((*storage).tx_buf) }.cast::<u8>();
        let Some(buffer_pa) = dma_address(buffer) else {
            return false;
        };
        let next_index = ring_next(index);
        let next_desc = unsafe { addr_of_mut!((*storage).tx_desc[next_index]) };
        let Some(next_pa) = dma_address(next_desc.cast()) else {
            return false;
        };
        unsafe { buffer.copy_from_nonoverlapping(frame.as_ptr(), frame.len()) };
        dma_cache_flush(buffer, frame.len());
        unsafe {
            addr_of_mut!((*desc).des0).write_volatile(buffer_pa as u32);
            addr_of_mut!((*desc).des1).write_volatile((buffer_pa >> 32) as u32);
            addr_of_mut!((*desc).des2).write_volatile(frame.len() as u32);
            dma_barrier();
            addr_of_mut!((*desc).des3).write_volatile(
                DESC_OWN | DESC_FIRST | DESC_LAST | frame.len() as u32,
            );
        }
        dma_cache_flush(desc.cast(), size_of::<EqosDesc>());
        state.tx_next = next_index;
        state.regs.write(DMA_CH0_TXDESC_TAIL, next_pa as u32);

        for _ in 0..TX_COMPLETE_POLLS {
            dma_cache_flush(desc.cast(), size_of::<EqosDesc>());
            if unsafe { addr_of_mut!((*desc).des3).read_volatile() } & DESC_OWN == 0 {
                return true;
            }
            spin_delay_ns(1_000);
        }
        println!("[jh7110-eqos] TX completion timed out");
        false
    }

    pub(crate) fn try_recv(&self, out: &mut [u8]) -> Option<usize> {
        let mut state = self.state.lock();
        let index = state.rx_next;
        let storage = dma_storage();
        let desc_ptr = unsafe { addr_of_mut!((*storage).rx_desc[index]) };
        dma_cache_flush(desc_ptr.cast(), size_of::<EqosDesc>());
        let desc = unsafe { desc_ptr.read_volatile() };
        if desc.des3 & DESC_OWN != 0 {
            return None;
        }
        dma_barrier();

        let length = (desc.des3 & RX_DESC_LENGTH_MASK) as usize;
        let valid = desc.des3 & RX_DESC_ERROR == 0
            && desc.des3 & DESC_FIRST != 0
            && desc.des3 & DESC_LAST != 0
            && length != 0
            && length <= BUFFER_SIZE;
        let copy_len = length.min(out.len());
        if valid {
            let buffer = unsafe { addr_of_mut!((*storage).rx_buf[index]) }.cast::<u8>();
            dma_cache_flush(buffer, length);
            unsafe { out.as_mut_ptr().copy_from_nonoverlapping(buffer, copy_len) };
        }

        let buffer = unsafe { addr_of_mut!((*storage).rx_buf[index]) }.cast::<u8>();
        let buffer_pa = dma_address(buffer)?;
        // Follow U-Boot's two-phase non-coherent RX handoff: publish des0=0,
        // evict the consumed buffer, restore the address fields, then publish
        // OWN last.
        unsafe { addr_of_mut!((*desc_ptr).des0).write_volatile(0) };
        dma_barrier();
        dma_cache_flush(desc_ptr.cast(), size_of::<EqosDesc>());
        dma_cache_flush(buffer, length.min(BUFFER_SIZE));
        unsafe {
            addr_of_mut!((*desc_ptr).des0).write_volatile(buffer_pa as u32);
            addr_of_mut!((*desc_ptr).des1).write_volatile((buffer_pa >> 32) as u32);
            addr_of_mut!((*desc_ptr).des2).write_volatile(0);
            dma_barrier();
            addr_of_mut!((*desc_ptr).des3).write_volatile(
                DESC_OWN | DESC_BUF1_VALID | RX_DESC_INTERRUPT_ON_COMPLETION,
            );
        }
        dma_cache_flush(desc_ptr.cast(), size_of::<EqosDesc>());
        state.regs.write(
            DMA_CH0_RXDESC_TAIL,
            dma_address(desc_ptr.cast()).unwrap_or(0) as u32,
        );
        state.rx_next = ring_next(index);

        valid.then_some(copy_len)
    }
}

fn refresh_link(state: &mut EqosState) {
    let link = phy::read_link(state.regs, state.link.address, state.link.id);
    if link.up != state.link.up
        || link.speed_mbps != state.link.speed_mbps
        || link.full_duplex != state.link.full_duplex
    {
        if !phy::apply_tx_clock_inversion(state.regs, link.address, link.speed_mbps) {
            println!("[jh7110-eqos] failed to update YT8531 TX clock inversion");
            return;
        }
        state.regs.configure_link(link);
        let _ = state.soc.set_tx_speed(link.speed_mbps);
        println!(
            "[jh7110-eqos] link={} {}Mbps {} status={:#06x}",
            if link.up { "up" } else { "down" },
            link.speed_mbps,
            if link.full_duplex {
                "full-duplex"
            } else {
                "half-duplex"
            },
            link.status
        );
        state.link = link;
    }
}

fn dma_storage() -> *mut EqosDmaStorage {
    addr_of_mut!(DMA_STORAGE)
}

fn dma_address(ptr: *const u8) -> Option<u64> {
    platform::translate_direct_mapped_kernel_va(ptr as usize).map(|address| address as u64)
}

fn valid_mac(mac: [u8; 6]) -> bool {
    mac != [0; 6] && mac != [0xff; 6] && mac[0] & 1 == 0
}

fn ring_next(index: usize) -> usize {
    (index + 1) % RING_SIZE
}

fn spin_delay_ns(duration_ns: u64) {
    let deadline = get_time_ns().saturating_add(duration_ns);
    while get_time_ns() < deadline {
        core::hint::spin_loop();
    }
}

#[inline]
fn dma_barrier() {
    unsafe { core::arch::asm!("fence iorw, iorw", options(nostack, preserves_flags)) };
}

/// Clean and invalidate every JH7110 L2 cache line intersecting a DMA range.
///
/// This follows Linux's `sifive_ccache` non-standard DMA cache operation: a
/// 64-bit physical line address written to FLUSH64 flushes that cache line.
fn dma_cache_flush(ptr: *const u8, size: usize) {
    if size == 0 {
        return;
    }
    let Some(start) = dma_address(ptr) else {
        return;
    };
    let Some(end) = start.checked_add(size as u64) else {
        return;
    };
    let flush = (platform::mmio_phys_to_virt(CCACHE_PADDR) + CCACHE_FLUSH64) as *mut u64;
    let mut line = start & !((CCACHE_LINE_SIZE as u64) - 1);
    dma_barrier();
    while line < end {
        unsafe { flush.write_volatile(line) };
        line += CCACHE_LINE_SIZE as u64;
    }
    dma_barrier();
}

const _: () = assert!(size_of::<EqosDesc>() == CCACHE_LINE_SIZE);
