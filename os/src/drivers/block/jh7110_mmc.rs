//! Native polling JH7110 DesignWare MMC driver for VisionFive 2.
//!
//! This module deliberately lives in the kernel and has no dependency on the
//! userspace/rootfs TGOSKits tree. It uses conservative SD default timing,
//! one-bit mode, and single-block PIO requests for the initial board path.

use core::{any::Any, hint::spin_loop, ptr};

use fs::{BlockDevice, BLOCK_SZ};

use crate::{of::block::MmcResource, sync::SpinNoIrqLock};

use super::{block_device_name, BLOCK_DEVICES};

const SD_BLOCK_SIZE: usize = 512;
// U-Boot reports a 50 MHz SD high-speed bus while the live controller keeps
// CLKDIV=2. DWMMC divides CIU by 2*n, so the JH7110 CIU input is 200 MHz.
const JH7110_REFERENCE_CLOCK_HZ: u32 = 200_000_000;
const IDENTIFICATION_CLOCK_HZ: u32 = 400_000;
const DEFAULT_SD_CLOCK_HZ: u32 = 25_000_000;
const FIFO_OFFSET: usize = 0x200;
const FIFO_DEPTH_WORDS: u32 = 32;
const POLL_LIMIT: usize = 2_000_000;

const REG_CTRL: usize = 0x00;
const REG_PWREN: usize = 0x04;
const REG_CLKDIV: usize = 0x08;
const REG_CLKSRC: usize = 0x0c;
const REG_CLKENA: usize = 0x10;
const REG_TMOUT: usize = 0x14;
const REG_CTYPE: usize = 0x18;
const REG_BLKSIZ: usize = 0x1c;
const REG_BYTCNT: usize = 0x20;
const REG_INTMASK: usize = 0x24;
const REG_CMDARG: usize = 0x28;
const REG_CMD: usize = 0x2c;
const REG_RESP0: usize = 0x30;
const REG_RINTSTS: usize = 0x44;
const REG_STATUS: usize = 0x48;
const REG_FIFOTH: usize = 0x4c;
const REG_UHS: usize = 0x74;
const REG_BMOD: usize = 0x80;

const CTRL_RESET: u32 = 1 << 0;
const CTRL_FIFO_RESET: u32 = 1 << 1;
const CTRL_DMA_RESET: u32 = 1 << 2;
const CTRL_INT_ENABLE: u32 = 1 << 4;
const CTRL_DMA_ENABLE: u32 = 1 << 5;
const CTRL_USE_IDMAC: u32 = 1 << 25;

const CMD_START: u32 = 1 << 31;
const CMD_USE_HOLD_REG: u32 = 1 << 29;
const CMD_UPDATE_CLOCK_ONLY: u32 = 1 << 21;
const CMD_SEND_INITIALIZATION: u32 = 1 << 15;
const CMD_WAIT_PRVDATA_COMPLETE: u32 = 1 << 13;
const CMD_WRITE: u32 = 1 << 10;
const CMD_DATA_EXPECTED: u32 = 1 << 9;
const CMD_CHECK_RESPONSE_CRC: u32 = 1 << 8;
const CMD_RESPONSE_LONG: u32 = 1 << 7;
const CMD_RESPONSE_EXPECTED: u32 = 1 << 6;

const INT_RESPONSE_ERROR: u32 = 1 << 1;
const INT_COMMAND_DONE: u32 = 1 << 2;
const INT_DATA_TRANSFER_OVER: u32 = 1 << 3;
const INT_TXDR: u32 = 1 << 4;
const INT_RXDR: u32 = 1 << 5;
const INT_RESPONSE_CRC_ERROR: u32 = 1 << 6;
const INT_DATA_CRC_ERROR: u32 = 1 << 7;
const INT_RESPONSE_TIMEOUT: u32 = 1 << 8;
const INT_DATA_READ_TIMEOUT: u32 = 1 << 9;
const INT_HOST_TIMEOUT: u32 = 1 << 10;
const INT_FIFO_UNDER_OVER_RUN: u32 = 1 << 11;
const INT_HARDWARE_LOCKED_WRITE: u32 = 1 << 12;
const INT_START_BIT_ERROR: u32 = 1 << 13;
const INT_END_BIT_ERROR: u32 = 1 << 15;
const INT_ERROR_MASK: u32 = INT_RESPONSE_ERROR
    | INT_RESPONSE_CRC_ERROR
    | INT_DATA_CRC_ERROR
    | INT_RESPONSE_TIMEOUT
    | INT_DATA_READ_TIMEOUT
    | INT_HOST_TIMEOUT
    | INT_FIFO_UNDER_OVER_RUN
    | INT_HARDWARE_LOCKED_WRITE
    | INT_START_BIT_ERROR
    | INT_END_BIT_ERROR;

const STATUS_FIFO_FULL: u32 = 1 << 3;
const STATUS_DATA_BUSY: u32 = 1 << 9;
const STATUS_FIFO_COUNT_SHIFT: u32 = 17;
const STATUS_FIFO_COUNT_MASK: u32 = 0x1fff;

#[derive(Clone, Copy, Debug)]
enum MmcError {
    Timeout {
        phase: &'static str,
        cmd: u8,
        status: u32,
    },
    Controller {
        phase: &'static str,
        cmd: u8,
        status: u32,
    },
    BadResponse {
        cmd: u8,
        response: u32,
    },
    UnsupportedCsd(u8),
    CapacityOverflow,
}

#[derive(Clone, Copy)]
enum ResponseKind {
    None,
    ShortCrc,
    ShortBusy,
    ShortNoCrc,
    LongCrc,
}

#[derive(Clone, Copy)]
enum DataDirection {
    Read,
    Write,
}

struct Jh7110DwMmc {
    base: usize,
    rca: u16,
    high_capacity: bool,
}

impl Jh7110DwMmc {
    fn new(base: usize) -> Self {
        Self {
            base,
            rca: 0,
            high_capacity: false,
        }
    }

    #[inline]
    fn read_reg(&self, offset: usize) -> u32 {
        // SAFETY: `base` is the exclusive, mapped DWMMC register window from FDT.
        unsafe { ptr::read_volatile((self.base + offset) as *const u32) }
    }

    #[inline]
    fn write_reg(&self, offset: usize, value: u32) {
        // SAFETY: `base` is the exclusive, mapped DWMMC register window from FDT.
        unsafe { ptr::write_volatile((self.base + offset) as *mut u32, value) }
    }

    #[inline]
    fn fifo_count(&self) -> usize {
        ((self.read_reg(REG_STATUS) >> STATUS_FIFO_COUNT_SHIFT) & STATUS_FIFO_COUNT_MASK) as usize
    }

    fn wait_clear(&self, offset: usize, mask: u32, phase: &'static str) -> Result<(), MmcError> {
        for _ in 0..POLL_LIMIT {
            if self.read_reg(offset) & mask == 0 {
                return Ok(());
            }
            spin_loop();
        }
        Err(MmcError::Timeout {
            phase,
            cmd: 0,
            status: self.read_reg(offset),
        })
    }

    fn reset(&self) -> Result<(), MmcError> {
        self.write_reg(REG_CLKENA, 0);
        self.write_reg(REG_PWREN, 1);
        let ctrl = self.read_reg(REG_CTRL) & !(CTRL_USE_IDMAC | CTRL_DMA_ENABLE | CTRL_INT_ENABLE);
        self.write_reg(
            REG_CTRL,
            ctrl | CTRL_RESET | CTRL_FIFO_RESET | CTRL_DMA_RESET,
        );
        self.wait_clear(
            REG_CTRL,
            CTRL_RESET | CTRL_FIFO_RESET | CTRL_DMA_RESET,
            "controller-reset",
        )?;
        self.write_reg(REG_INTMASK, 0);
        self.write_reg(REG_RINTSTS, u32::MAX);
        self.write_reg(REG_TMOUT, u32::MAX);
        self.write_reg(REG_CLKSRC, 0);
        self.write_reg(REG_CTYPE, 0);
        self.write_reg(REG_UHS, 0);
        self.write_reg(REG_BMOD, 0);

        let half = FIFO_DEPTH_WORDS / 2;
        let fifoth = (2 << 28) | ((half - 1) << 16) | half;
        self.write_reg(REG_FIFOTH, fifoth);
        self.program_clock(IDENTIFICATION_CLOCK_HZ)
    }

    fn send_update_clock(&self) -> Result<(), MmcError> {
        self.write_reg(REG_CMD, CMD_START | CMD_UPDATE_CLOCK_ONLY);
        self.wait_clear(REG_CMD, CMD_START, "clock-update")
    }

    fn program_clock(&self, target_hz: u32) -> Result<(), MmcError> {
        self.write_reg(REG_CLKENA, 0);
        self.send_update_clock()?;
        let divisor = if target_hz == 0 || target_hz >= JH7110_REFERENCE_CLOCK_HZ {
            0
        } else {
            JH7110_REFERENCE_CLOCK_HZ
                .div_ceil(target_hz.saturating_mul(2))
                .min(0xff)
        };
        self.write_reg(REG_CLKDIV, divisor);
        self.send_update_clock()?;
        self.write_reg(REG_CLKENA, 1);
        self.send_update_clock()
    }

    fn reset_fifo(&self) -> Result<(), MmcError> {
        self.write_reg(REG_CTRL, self.read_reg(REG_CTRL) | CTRL_FIFO_RESET);
        self.wait_clear(REG_CTRL, CTRL_FIFO_RESET, "fifo-reset")
    }

    fn wait_data_idle(&self, cmd: u8) -> Result<(), MmcError> {
        for _ in 0..POLL_LIMIT {
            if self.read_reg(REG_STATUS) & STATUS_DATA_BUSY == 0 {
                return Ok(());
            }
            spin_loop();
        }
        Err(MmcError::Timeout {
            phase: "data-busy",
            cmd,
            status: self.read_reg(REG_STATUS),
        })
    }

    fn send_command(
        &self,
        index: u8,
        argument: u32,
        response: ResponseKind,
        data: Option<DataDirection>,
    ) -> Result<[u32; 4], MmcError> {
        self.wait_clear(REG_CMD, CMD_START, "command-inhibit")?;
        if data.is_some() || matches!(response, ResponseKind::ShortBusy) {
            self.wait_data_idle(index)?;
        }
        self.write_reg(REG_RINTSTS, u32::MAX);
        self.write_reg(REG_CMDARG, argument);

        let mut command =
            CMD_START | CMD_USE_HOLD_REG | CMD_WAIT_PRVDATA_COMPLETE | u32::from(index & 0x3f);
        command |= match response {
            ResponseKind::None => 0,
            ResponseKind::ShortCrc | ResponseKind::ShortBusy => {
                CMD_RESPONSE_EXPECTED | CMD_CHECK_RESPONSE_CRC
            }
            ResponseKind::ShortNoCrc => CMD_RESPONSE_EXPECTED,
            ResponseKind::LongCrc => {
                CMD_RESPONSE_EXPECTED | CMD_RESPONSE_LONG | CMD_CHECK_RESPONSE_CRC
            }
        };
        if index == 0 {
            command |= CMD_SEND_INITIALIZATION;
        }
        if let Some(direction) = data {
            command |= CMD_DATA_EXPECTED;
            if matches!(direction, DataDirection::Write) {
                command |= CMD_WRITE;
            }
        }
        self.write_reg(REG_CMD, command);

        for _ in 0..POLL_LIMIT {
            let raw = self.read_reg(REG_RINTSTS);
            if raw & INT_ERROR_MASK != 0 {
                self.write_reg(REG_RINTSTS, raw & (INT_ERROR_MASK | INT_COMMAND_DONE));
                return Err(MmcError::Controller {
                    phase: "command",
                    cmd: index,
                    status: raw,
                });
            }
            if raw & INT_COMMAND_DONE != 0 {
                self.write_reg(REG_RINTSTS, INT_COMMAND_DONE);
                let result = [
                    self.read_reg(REG_RESP0),
                    self.read_reg(REG_RESP0 + 4),
                    self.read_reg(REG_RESP0 + 8),
                    self.read_reg(REG_RESP0 + 12),
                ];
                if matches!(response, ResponseKind::ShortBusy) {
                    self.wait_data_idle(index)?;
                }
                return Ok(result);
            }
            spin_loop();
        }
        Err(MmcError::Timeout {
            phase: "command",
            cmd: index,
            status: self.read_reg(REG_RINTSTS),
        })
    }

    fn init_sd(&mut self) -> Result<u64, MmcError> {
        self.reset()?;
        busy_wait_ms(2);
        self.send_command(0, 0, ResponseKind::None, None)?;
        busy_wait_ms(2);

        let if_cond = self.send_command(8, 0x1aa, ResponseKind::ShortCrc, None)?[0];
        if if_cond & 0xfff != 0x1aa {
            return Err(MmcError::BadResponse {
                cmd: 8,
                response: if_cond,
            });
        }

        let mut ocr = 0;
        for _ in 0..100 {
            self.send_command(55, 0, ResponseKind::ShortCrc, None)?;
            ocr = self.send_command(41, 0x40ff_8000, ResponseKind::ShortNoCrc, None)?[0];
            if ocr & (1 << 31) != 0 {
                break;
            }
            busy_wait_ms(10);
        }
        if ocr & (1 << 31) == 0 {
            return Err(MmcError::Timeout {
                phase: "acmd41",
                cmd: 41,
                status: ocr,
            });
        }
        self.high_capacity = ocr & (1 << 30) != 0;

        self.send_command(2, 0, ResponseKind::LongCrc, None)?;
        let r6 = self.send_command(3, 0, ResponseKind::ShortCrc, None)?[0];
        self.rca = (r6 >> 16) as u16;
        if self.rca == 0 {
            return Err(MmcError::BadResponse {
                cmd: 3,
                response: r6,
            });
        }
        let csd = self.send_command(9, u32::from(self.rca) << 16, ResponseKind::LongCrc, None)?;
        let capacity_blocks = capacity_blocks_from_csd(csd)?;
        self.send_command(7, u32::from(self.rca) << 16, ResponseKind::ShortBusy, None)?;
        if !self.high_capacity {
            self.send_command(16, SD_BLOCK_SIZE as u32, ResponseKind::ShortCrc, None)?;
        }

        // Keep the first kernel-native implementation in one-bit/default mode.
        // It avoids board-specific 4-bit sampling/tuning until basic rootfs I/O
        // is proven, while still raising the bus from 400 kHz to 25 MHz.
        self.program_clock(DEFAULT_SD_CLOCK_HZ)?;
        Ok(capacity_blocks)
    }

    fn card_address(&self, block: u32) -> Result<u32, MmcError> {
        if self.high_capacity {
            Ok(block)
        } else {
            block
                .checked_mul(SD_BLOCK_SIZE as u32)
                .ok_or(MmcError::CapacityOverflow)
        }
    }

    fn read_sector(&self, block: u32, output: &mut [u8]) -> Result<(), MmcError> {
        debug_assert_eq!(output.len(), SD_BLOCK_SIZE);
        self.reset_fifo()?;
        self.write_reg(REG_BLKSIZ, SD_BLOCK_SIZE as u32);
        self.write_reg(REG_BYTCNT, SD_BLOCK_SIZE as u32);
        self.send_command(
            17,
            self.card_address(block)?,
            ResponseKind::ShortCrc,
            Some(DataDirection::Read),
        )?;

        let mut offset = 0usize;
        let mut transfer_done = false;
        for _ in 0..POLL_LIMIT {
            let raw = self.read_reg(REG_RINTSTS);
            if raw & INT_ERROR_MASK != 0 {
                self.write_reg(REG_RINTSTS, raw & (INT_ERROR_MASK | INT_RXDR));
                let _ = self.reset_fifo();
                return Err(MmcError::Controller {
                    phase: "read-data",
                    cmd: 17,
                    status: raw,
                });
            }
            transfer_done |= raw & INT_DATA_TRANSFER_OVER != 0;
            let clear = raw & (INT_RXDR | INT_DATA_TRANSFER_OVER);
            if clear != 0 {
                self.write_reg(REG_RINTSTS, clear);
            }

            let mut words = self.fifo_count();
            while words != 0 && offset < output.len() {
                let value = self.read_reg(FIFO_OFFSET).to_le_bytes();
                output[offset..offset + 4].copy_from_slice(&value);
                offset += 4;
                words -= 1;
            }
            if offset == output.len() && transfer_done {
                return Ok(());
            }
            spin_loop();
        }
        Err(MmcError::Timeout {
            phase: "read-data",
            cmd: 17,
            status: self.read_reg(REG_RINTSTS),
        })
    }

    fn write_sector(&self, block: u32, input: &[u8]) -> Result<(), MmcError> {
        debug_assert_eq!(input.len(), SD_BLOCK_SIZE);
        self.reset_fifo()?;
        self.write_reg(REG_BLKSIZ, SD_BLOCK_SIZE as u32);
        self.write_reg(REG_BYTCNT, SD_BLOCK_SIZE as u32);
        self.send_command(
            24,
            self.card_address(block)?,
            ResponseKind::ShortCrc,
            Some(DataDirection::Write),
        )?;

        let mut offset = 0usize;
        let mut transfer_done = false;
        for _ in 0..POLL_LIMIT {
            let raw = self.read_reg(REG_RINTSTS);
            if raw & INT_ERROR_MASK != 0 {
                self.write_reg(REG_RINTSTS, raw & (INT_ERROR_MASK | INT_TXDR));
                let _ = self.reset_fifo();
                return Err(MmcError::Controller {
                    phase: "write-data",
                    cmd: 24,
                    status: raw,
                });
            }
            transfer_done |= raw & INT_DATA_TRANSFER_OVER != 0;
            let clear = raw & (INT_TXDR | INT_DATA_TRANSFER_OVER);
            if clear != 0 {
                self.write_reg(REG_RINTSTS, clear);
            }

            while offset < input.len() && self.read_reg(REG_STATUS) & STATUS_FIFO_FULL == 0 {
                let value = u32::from_le_bytes(input[offset..offset + 4].try_into().unwrap());
                self.write_reg(FIFO_OFFSET, value);
                offset += 4;
            }
            if offset == input.len() && transfer_done {
                self.wait_data_idle(24)?;
                return Ok(());
            }
            spin_loop();
        }
        Err(MmcError::Timeout {
            phase: "write-data",
            cmd: 24,
            status: self.read_reg(REG_RINTSTS),
        })
    }
}

fn response_r2_bytes(response: [u32; 4]) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    bytes[0..4].copy_from_slice(&response[3].to_be_bytes());
    bytes[4..8].copy_from_slice(&response[2].to_be_bytes());
    bytes[8..12].copy_from_slice(&response[1].to_be_bytes());
    bytes[12..16].copy_from_slice(&response[0].to_be_bytes());
    bytes
}

fn r2_bits(bytes: &[u8; 16], msb: u8, lsb: u8) -> u32 {
    debug_assert!(msb >= lsb);
    debug_assert!(msb < 128);
    debug_assert!(u16::from(msb) - u16::from(lsb) < 32);
    let mut value = 0u32;
    for card_bit in lsb..=msb {
        let linear = 127usize - usize::from(card_bit);
        let bit = (bytes[linear / 8] >> (7 - linear % 8)) & 1;
        value |= u32::from(bit) << (card_bit - lsb);
    }
    value
}

fn capacity_blocks_from_csd(response: [u32; 4]) -> Result<u64, MmcError> {
    let bytes = response_r2_bytes(response);
    let structure = r2_bits(&bytes, 127, 126) as u8;
    match structure {
        0 => {
            let read_block_len = r2_bits(&bytes, 83, 80);
            let c_size = r2_bits(&bytes, 73, 62);
            let c_size_mult = r2_bits(&bytes, 49, 47);
            let block_len = 1u64 << read_block_len;
            let block_count = u64::from(c_size + 1) << (c_size_mult + 2);
            block_count
                .checked_mul(block_len)
                .map(|bytes| bytes / SD_BLOCK_SIZE as u64)
                .ok_or(MmcError::CapacityOverflow)
        }
        1 => Ok((u64::from(r2_bits(&bytes, 69, 48)) + 1) * 1024),
        other => Err(MmcError::UnsupportedCsd(other)),
    }
}

/// A synchronous, single-sector PIO SD disk.
pub struct Jh7110MmcBlock {
    controller: SpinNoIrqLock<Jh7110DwMmc>,
    capacity_blocks: u64,
}

impl Jh7110MmcBlock {
    fn try_new(resource: MmcResource) -> Result<Self, MmcError> {
        let device = resource.device();
        let base = crate::platform::mmio_phys_to_virt(device.start);
        let mut controller = Jh7110DwMmc::new(base);
        println!(
            "[jh7110-mmc] resetting native controller at {:#x}",
            device.start
        );
        let capacity_blocks = controller.init_sd()?;
        println!(
            "[jh7110-mmc] native SD online: rca={:#06x} high-capacity={} blocks={} ({} MiB) bus=1-bit@{}Hz fifo={}w",
            controller.rca,
            controller.high_capacity,
            capacity_blocks,
            capacity_blocks.saturating_mul(SD_BLOCK_SIZE as u64) / (1024 * 1024),
            DEFAULT_SD_CLOCK_HZ,
            FIFO_DEPTH_WORDS,
        );
        Ok(Self {
            controller: SpinNoIrqLock::new(controller),
            capacity_blocks,
        })
    }

    fn checked_request(&self, start_block: usize, len: usize) -> usize {
        assert_eq!(len % SD_BLOCK_SIZE, 0, "MMC request is not sector aligned");
        let blocks = len / SD_BLOCK_SIZE;
        let end = start_block.checked_add(blocks).expect("MMC LBA overflow") as u64;
        assert!(
            end <= self.capacity_blocks,
            "MMC request exceeds card capacity"
        );
        blocks
    }

    fn read_range(&self, start_block: usize, output: &mut [u8]) {
        let blocks = self.checked_request(start_block, output.len());
        let controller = self.controller.lock();
        for index in 0..blocks {
            let block = u32::try_from(start_block + index).expect("MMC LBA exceeds u32");
            let sector = &mut output[index * SD_BLOCK_SIZE..(index + 1) * SD_BLOCK_SIZE];
            controller
                .read_sector(block, sector)
                .unwrap_or_else(|error| panic!("JH7110 native MMC read failed: {:?}", error));
        }
    }

    fn write_range(&self, start_block: usize, input: &[u8]) {
        let blocks = self.checked_request(start_block, input.len());
        let controller = self.controller.lock();
        for index in 0..blocks {
            let block = u32::try_from(start_block + index).expect("MMC LBA exceeds u32");
            let sector = &input[index * SD_BLOCK_SIZE..(index + 1) * SD_BLOCK_SIZE];
            controller
                .write_sector(block, sector)
                .unwrap_or_else(|error| panic!("JH7110 native MMC write failed: {:?}", error));
        }
    }
}

impl BlockDevice for Jh7110MmcBlock {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        self.read_range(block_id, buf);
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) {
        self.write_range(block_id, buf);
    }

    fn read_blocks(&self, start_block: usize, buf: &mut [u8]) {
        self.read_range(start_block, buf);
    }

    fn write_blocks(&self, start_block: usize, buf: &[u8]) {
        self.write_range(start_block, buf);
    }
}

fn busy_wait_ms(milliseconds: usize) {
    let start = crate::timer::get_time_ms();
    while crate::timer::get_time_ms().saturating_sub(start) < milliseconds {
        spin_loop();
    }
}

/// Probe enabled JH7110 SD controllers until one returns a usable card.
pub fn probe_jh7110_mmc() {
    assert_eq!(BLOCK_SZ, SD_BLOCK_SIZE);
    for resource in crate::boot::context::get().devices().mmc_devices() {
        let device = resource.device();
        if resource.no_sd() {
            println!(
                "[jh7110-mmc] skipping non-SD controller at {:#x}",
                device.start
            );
            continue;
        }
        println!(
            "[jh7110-mmc] probing native driver pa={:#x} size={:#x} irq={:?}",
            device.start, device.size, device.irq,
        );
        match Jh7110MmcBlock::try_new(resource) {
            Ok(device) => {
                let name = block_device_name(0);
                let mut first = [0u8; SD_BLOCK_SIZE];
                device.read_block(0, &mut first);
                println!(
                    "[jh7110-mmc] {} native read probe passed: sector0={:02x}{:02x}{:02x}{:02x}",
                    name, first[0], first[1], first[2], first[3]
                );
                BLOCK_DEVICES
                    .lock()
                    .insert(name, alloc::sync::Arc::new(device));
                return;
            }
            Err(error) => println!(
                "[jh7110-mmc] controller at {:#x} failed: {:?}",
                device.start, error
            ),
        }
    }
    println!("[jh7110-mmc] no enabled controller returned a usable SD card");
}
