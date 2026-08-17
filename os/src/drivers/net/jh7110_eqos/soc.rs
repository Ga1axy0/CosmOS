// SPDX-License-Identifier: GPL-2.0-or-later
//! JH7110 GMAC1 clock/reset control.
//!
//! The enabled clock/reset set and negotiated-rate sequence mirror StarFive
//! U-Boot's JH7110 EQoS and clock drivers at commit
//! `c4c67bb66ae6f41c98537d18cf5c3abc8b97b8e4`.

use crate::{bootinfo, platform};

const SYSCRG_PADDR: usize = 0x1302_0000;
const RESET_ASSERT_BASE: usize = 0x2f8;
const RESET_STATUS_BASE: usize = 0x308;
const CLOCK_ENABLE: u32 = 1 << 31;
const CLOCK_DIV_MASK: u32 = 0x00ff_ffff;
const RESET_POLL_LIMIT: usize = 100_000;

const GMAC1_CLOCKS: [usize; 7] = [100, 105, 102, 97, 98, 107, 101];
const GMAC1_GTX_CLOCK: usize = 100;
const GMAC1_RMII_RTX_CLOCK: usize = 101;
const GMAC1_TX_CLOCK: usize = 105;
const GMAC1_RESETS: [usize; 2] = [67, 66];
const GMACUSB_ROOT_RATE_HZ: u32 = 1_000_000_000;
const GMAC1_RMII_REFIN_RATE_HZ: u32 = 50_000_000;

#[derive(Clone, Copy)]
pub(super) struct SocControl {
    base: *mut u8,
}

unsafe impl Send for SocControl {}
unsafe impl Sync for SocControl {}

impl SocControl {
    pub(super) fn start_gmac1() -> Option<Self> {
        let resource = bootinfo::get().syscrg()?;
        if resource.start != SYSCRG_PADDR || resource.size < RESET_STATUS_BASE + 16 {
            println!(
                "[jh7110-eqos] unsupported SYSCRG resource pa={:#x} size={:#x}",
                resource.start, resource.size
            );
            return None;
        }

        let control = Self {
            base: platform::mmio_phys_to_virt(resource.start) as *mut u8,
        };
        for id in GMAC1_CLOCKS {
            let value = control.read(id * 4);
            control.write(id * 4, value | CLOCK_ENABLE);
        }
        io_barrier();

        // Both GMAC1 resets live in SYSCRG word 2.  A set status bit means
        // the corresponding reset has been deasserted.
        let word = GMAC1_RESETS[0] / 32;
        let mask = GMAC1_RESETS
            .iter()
            .fold(0u32, |bits, id| bits | (1u32 << (id % 32)));
        let assert_offset = RESET_ASSERT_BASE + word * 4;
        control.write(assert_offset, control.read(assert_offset) & !mask);
        io_barrier();

        let status_offset = RESET_STATUS_BASE + word * 4;
        let mut status = 0;
        for _ in 0..RESET_POLL_LIMIT {
            status = control.read(status_offset);
            if status & mask == mask {
                println!(
                    "[jh7110-eqos] SYSCRG clocks enabled resets deasserted status={:#010x}",
                    status
                );
                return Some(control);
            }
            core::hint::spin_loop();
        }
        println!(
            "[jh7110-eqos] SYSCRG reset deassert timeout status={:#010x} mask={:#010x}",
            status, mask
        );
        None
    }

    /// Select the standard RGMII transmit clock for the negotiated speed.
    pub(super) fn set_tx_speed(self, speed_mbps: u32) -> bool {
        let rate = match speed_mbps {
            1000 => 125_000_000,
            100 => 25_000_000,
            10 => 2_500_000,
            _ => return false,
        };

        // Equivalent to clk_set_rate(gtx, rate).
        let divider = GMACUSB_ROOT_RATE_HZ / rate;
        let gtx_offset = GMAC1_GTX_CLOCK * 4;
        let gtx = (self.read(gtx_offset) & !CLOCK_DIV_MASK) | CLOCK_ENABLE | divider;
        self.write(gtx_offset, gtx);

        // VisionFive 2's override also calls clk_set_rate(rmii_rtx, rate).
        // The external parent is 50 MHz; the clock framework clamps requests
        // above the parent rate to divider 1.
        let rmii_divider = (GMAC1_RMII_REFIN_RATE_HZ / rate).max(1);
        let rmii_offset = GMAC1_RMII_RTX_CLOCK * 4;
        let rmii = (self.read(rmii_offset) & !CLOCK_DIV_MASK) | CLOCK_ENABLE | rmii_divider;
        self.write(rmii_offset, rmii);

        // The generic U-Boot path does not override the firmware-selected TX
        // mux parent; preserve it and only ensure the clock is enabled.
        let tx_offset = GMAC1_TX_CLOCK * 4;
        let tx = self.read(tx_offset) | CLOCK_ENABLE;
        self.write(tx_offset, tx);
        io_barrier();
        println!(
            "[jh7110-eqos] StarFive TX clock {}Mbps gtx={:#010x} rmii={:#010x} tx={:#010x}",
            speed_mbps,
            self.read(gtx_offset),
            self.read(rmii_offset),
            self.read(tx_offset)
        );
        true
    }

    pub(super) fn gtx_rate_hz(self) -> u32 {
        let divider = (self.read(GMAC1_GTX_CLOCK * 4) & CLOCK_DIV_MASK).max(1);
        GMACUSB_ROOT_RATE_HZ / divider
    }

    fn read(self, offset: usize) -> u32 {
        unsafe { self.base.add(offset).cast::<u32>().read_volatile() }
    }

    fn write(self, offset: usize, value: u32) {
        unsafe { self.base.add(offset).cast::<u32>().write_volatile(value) }
    }
}

#[inline]
fn io_barrier() {
    unsafe { core::arch::asm!("fence iorw, iorw", options(nostack, preserves_flags)) };
}
