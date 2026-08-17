// SPDX-License-Identifier: GPL-2.0-or-later
//! Motorcomm YT8531 support for VisionFive 2 GMAC1.
//!
//! This is a direct Rust port of StarFive U-Boot's `motorcomm.c` YT8531
//! config/startup path and the GMAC1 properties in
//! `starfive_visionfive2.dts`, both at commit
//! `c4c67bb66ae6f41c98537d18cf5c3abc8b97b8e4`.

use super::Registers;
use crate::timer::get_time_ns;

const MII_BMCR: u32 = 0;
const MII_BMSR: u32 = 1;
const MII_PHYSID1: u32 = 2;
const MII_PHYSID2: u32 = 3;
const YTPHY_SPECIFIC_STATUS: u32 = 0x11;
const YTPHY_EXT_ADDRESS: u32 = 0x1e;
const YTPHY_EXT_DATA: u32 = 0x1f;

const YT8531_PHY_ID: u32 = 0x4f51_e91b;
const MOTORCOMM_PHY_ID_MASK: u32 = 0x0000_0fff;
const YTPHY_CHIP_CONFIG: u16 = 0xa001;
const YTPHY_RGMII_CONFIG1: u16 = 0xa003;
const YTPHY_PAD_DRIVE_STRENGTH: u16 = 0xa010;

// Exact GMAC1 PHY-node values from StarFive's VisionFive 2 DTS.
const VF2_RGMII_SW_DR: u16 = 3;
const VF2_RGMII_SW_DR_2: u16 = 0;
const VF2_RGMII_SW_DR_RXC: u16 = 6;
const VF2_RXC_DELAY_ENABLE: u16 = 0;
const VF2_RX_DELAY_SEL: u16 = 2;
const VF2_TX_DELAY_SEL_FE: u16 = 5;
const VF2_TX_DELAY_SEL: u16 = 0;
const VF2_TX_INVERTED_10: u16 = 1;
const VF2_TX_INVERTED_100: u16 = 1;
const VF2_TX_INVERTED_1000: u16 = 0;

const BMCR_POWER_DOWN: u16 = 1 << 11;
const BMCR_AUTONEG_ENABLE: u16 = 1 << 12;
const BMCR_RESTART_AUTONEG: u16 = 1 << 9;
const BMSR_LINK_STATUS: u16 = 1 << 2;
const BMSR_AUTONEG_COMPLETE: u16 = 1 << 5;
const YTPHY_DUPLEX: u16 = 1 << 13;
const YTPHY_SPEED_MASK: u16 = 0x3 << 14;
const YTPHY_SPEED_100: u16 = 1 << 14;
const YTPHY_SPEED_1000: u16 = 2 << 14;

const LINK_POLL_INTERVAL_NS: u64 = 50_000_000;
const LINK_TIMEOUT_NS: u64 = 4_000_000_000;

#[derive(Clone, Copy, Debug)]
pub(super) struct PhyLink {
    pub(super) address: u32,
    pub(super) id: u32,
    pub(super) up: bool,
    pub(super) speed_mbps: u32,
    pub(super) full_duplex: bool,
    pub(super) status: u16,
}

pub(super) fn discover(regs: Registers) -> Option<PhyLink> {
    // U-Boot gets the address from the PHY node. CosmOS currently exposes
    // only the GMAC resource, so scanning Clause 22 addresses is the adapter.
    for address in 0..32 {
        let Some(id1) = regs.mdio_read(address, MII_PHYSID1) else {
            continue;
        };
        let Some(id2) = regs.mdio_read(address, MII_PHYSID2) else {
            continue;
        };
        if (id1 == 0 && id2 == 0) || (id1 == u16::MAX && id2 == u16::MAX) {
            continue;
        }
        let id = ((id1 as u32) << 16) | id2 as u32;
        if id & MOTORCOMM_PHY_ID_MASK != YT8531_PHY_ID & MOTORCOMM_PHY_ID_MASK {
            continue;
        }

        // YT8531 .config is exactly genphy_config_aneg(). Do not soft-reset
        // the PHY and do not touch vendor SYNCE registers.
        if !config_aneg(regs, address) || !configure_visionfive2(regs, address) {
            println!("[jh7110-eqos] StarFive YT8531 configuration failed");
            return None;
        }

        // U-Boot's genphy_update_link, ytphy_parse_status, then inversion.
        let deadline = get_time_ns().saturating_add(LINK_TIMEOUT_NS);
        let mut link = read_link(regs, address, id);
        while (!link.up || !autoneg_complete(regs, address)) && get_time_ns() < deadline {
            spin_delay(LINK_POLL_INTERVAL_NS);
            link = read_link(regs, address, id);
        }
        if !apply_tx_clock_inversion(regs, address, link.speed_mbps) {
            println!("[jh7110-eqos] YT8531 TX clock inversion setup failed");
            return None;
        }
        return Some(link);
    }
    None
}

fn config_aneg(regs: Registers, address: u32) -> bool {
    let Some(mut bmcr) = regs.mdio_read(address, MII_BMCR) else {
        return false;
    };
    let original = bmcr;
    bmcr &= !BMCR_POWER_DOWN;
    if bmcr & BMCR_AUTONEG_ENABLE == 0 {
        bmcr |= BMCR_AUTONEG_ENABLE | BMCR_RESTART_AUTONEG;
    }
    bmcr == original || regs.mdio_write(address, MII_BMCR, bmcr)
}

fn configure_visionfive2(regs: Registers, address: u32) -> bool {
    let Some(chip_before) = ext_read(regs, address, YTPHY_CHIP_CONFIG) else {
        return false;
    };
    let chip_after = bitfield_replace(chip_before, 8, 1, VF2_RXC_DELAY_ENABLE);
    if !ext_write(regs, address, YTPHY_CHIP_CONFIG, chip_after) {
        return false;
    }

    let Some(pad_before) = ext_read(regs, address, YTPHY_PAD_DRIVE_STRENGTH) else {
        return false;
    };
    let mut pad_after = pad_before;
    pad_after = bitfield_replace(pad_after, 4, 2, VF2_RGMII_SW_DR);
    pad_after = bitfield_replace(pad_after, 12, 1, VF2_RGMII_SW_DR_2);
    pad_after = bitfield_replace(pad_after, 13, 3, VF2_RGMII_SW_DR_RXC);
    if !ext_write(regs, address, YTPHY_PAD_DRIVE_STRENGTH, pad_after) {
        return false;
    }

    let Some(rgmii_before) = ext_read(regs, address, YTPHY_RGMII_CONFIG1) else {
        return false;
    };
    let mut rgmii_after = rgmii_before;
    rgmii_after = bitfield_replace(rgmii_after, 10, 4, VF2_RX_DELAY_SEL);
    rgmii_after = bitfield_replace(rgmii_after, 4, 4, VF2_TX_DELAY_SEL_FE);
    rgmii_after = bitfield_replace(rgmii_after, 0, 4, VF2_TX_DELAY_SEL);
    if !ext_write(regs, address, YTPHY_RGMII_CONFIG1, rgmii_after) {
        return false;
    }

    println!(
        "[jh7110-eqos] StarFive YT8531 chip={:#06x}->{:#06x} pad={:#06x}->{:#06x} rgmii={:#06x}->{:#06x}",
        chip_before, chip_after, pad_before, pad_after, rgmii_before, rgmii_after
    );
    true
}

pub(super) fn apply_tx_clock_inversion(
    regs: Registers,
    address: u32,
    speed_mbps: u32,
) -> bool {
    let inverted = match speed_mbps {
        1000 => VF2_TX_INVERTED_1000,
        100 => VF2_TX_INVERTED_100,
        10 => VF2_TX_INVERTED_10,
        _ => return false,
    };
    let Some(before) = ext_read(regs, address, YTPHY_RGMII_CONFIG1) else {
        return false;
    };
    let after = bitfield_replace(before, 14, 1, inverted);
    if !ext_write(regs, address, YTPHY_RGMII_CONFIG1, after) {
        return false;
    }
    println!(
        "[jh7110-eqos] YT8531 TX inversion {}Mbps={} rgmii={:#06x}->{:#06x}",
        speed_mbps, inverted, before, after
    );
    true
}

fn bitfield_replace(value: u16, offset: u32, size: u32, field: u16) -> u16 {
    let mask = (((1u32 << size) - 1) << offset) as u16;
    (value & !mask) | ((field << offset) & mask)
}

fn spin_delay(duration_ns: u64) {
    let deadline = get_time_ns().saturating_add(duration_ns);
    while get_time_ns() < deadline {
        core::hint::spin_loop();
    }
}

fn ext_read(regs: Registers, address: u32, register: u16) -> Option<u16> {
    regs.mdio_write(address, YTPHY_EXT_ADDRESS, register)
        .then(|| regs.mdio_read(address, YTPHY_EXT_DATA))
        .flatten()
}

fn ext_write(regs: Registers, address: u32, register: u16, value: u16) -> bool {
    regs.mdio_write(address, YTPHY_EXT_ADDRESS, register)
        && regs.mdio_write(address, YTPHY_EXT_DATA, value)
}

fn autoneg_complete(regs: Registers, address: u32) -> bool {
    let _ = regs.mdio_read(address, MII_BMSR);
    regs.mdio_read(address, MII_BMSR)
        .is_some_and(|bmsr| bmsr & BMSR_AUTONEG_COMPLETE != 0)
}

pub(super) fn read_link(regs: Registers, address: u32, id: u32) -> PhyLink {
    // BMSR link is latch-low, so follow genphy and read it twice.
    let _ = regs.mdio_read(address, MII_BMSR);
    let bmsr = regs.mdio_read(address, MII_BMSR).unwrap_or(0);
    let status = regs.mdio_read(address, YTPHY_SPECIFIC_STATUS).unwrap_or(0);
    let speed_mbps = match status & YTPHY_SPEED_MASK {
        YTPHY_SPEED_1000 => 1000,
        YTPHY_SPEED_100 => 100,
        _ => 10,
    };
    PhyLink {
        address,
        id,
        up: bmsr & BMSR_LINK_STATUS != 0,
        speed_mbps,
        full_duplex: status & YTPHY_DUPLEX != 0,
        status,
    }
}
