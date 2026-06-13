use core::sync::atomic::{AtomicBool, Ordering};

use alloc::sync::Arc;
use lazy_static::lazy_static;

use crate::sync::SpinNoIrqLock;

use super::VIRT_RTC;

const TOY_TRIM: usize = 0x20;
const TOY_WRITE0: usize = 0x24;
const TOY_WRITE1: usize = 0x28;
const TOY_READ0: usize = 0x2c;
const TOY_READ1: usize = 0x30;
const RTC_CTRL: usize = 0x40;

#[inline(always)]
fn mmio_read32(addr: usize) -> u32 {
    unsafe { core::ptr::read_volatile(addr as *const u32) }
}

#[inline(always)]
fn mmio_write32(addr: usize, val: u32) {
    unsafe { core::ptr::write_volatile(addr as *mut u32, val) }
}

struct Rtc {
    base: usize,
}

impl Rtc {
    fn new(base: usize) -> Self {
        Self { base }
    }

    fn init(&self) {
        mmio_write32(self.base + TOY_TRIM, 0);
        let ctrl = mmio_read32(self.base + RTC_CTRL);
        mmio_write32(self.base + RTC_CTRL, ctrl | (1 << 11) | (1 << 8));
    }

    fn read_time_ns(&self) -> u64 {
        let read0 = mmio_read32(self.base + TOY_READ0);
        let raw_year = mmio_read32(self.base + TOY_READ1) as u64;
        let year = if raw_year < 1900 {
            raw_year + 1900
        } else {
            raw_year
        };
        warn!("ls7a-rtc raw: TOY_READ0={:#010x} TOY_READ1={}", read0, raw_year);
        let mon = ((read0 >> 26) & 0x3f) as u64;
        let day = ((read0 >> 21) & 0x1f) as u64;
        let hour = ((read0 >> 16) & 0x1f) as u64;
        let min = ((read0 >> 10) & 0x3f) as u64;
        let sec = ((read0 >> 4) & 0x3f) as u64;
        let days = days_since_epoch(year, mon, day);
        (days * 86_400 + hour * 3_600 + min * 60 + sec) * 1_000_000_000
    }

    fn write_time_ns(&self, time_ns: u64) {
        let secs = time_ns / 1_000_000_000;
        let (year, mon, day, hour, min, sec) = secs_to_calendar(secs);
        let read0 = ((mon as u32) << 26)
            | ((day as u32) << 21)
            | ((hour as u32) << 16)
            | ((min as u32) << 10)
            | ((sec as u32) << 4);
        mmio_write32(self.base + TOY_WRITE0, read0);
        let write_year = if year >= 1900 { year - 1900 } else { year };
        mmio_write32(self.base + TOY_WRITE1, write_year as u32);
    }
}

fn days_since_epoch(year: u64, mon: u64, day: u64) -> u64 {
    const DAYS_BEFORE_MONTH: [u64; 13] = [0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let month = mon.max(1);
    let day = day.max(1);
    let leap_days = (year / 4).saturating_sub(year / 100) + year / 400;
    let base = year * 365 + leap_days;
    let is_leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let leap_adjust = if month > 2 && is_leap { 1 } else { 0 };
    let year_days = base + DAYS_BEFORE_MONTH[month as usize] + leap_adjust + day - 1;
    const EPOCH_DAYS: u64 = 1970 * 365 + (1970 / 4) - (1970 / 100) + (1970 / 400);
    year_days.saturating_sub(EPOCH_DAYS)
}

fn secs_to_calendar(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    const DAYS_IN_MONTH: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

    let sec = secs % 60;
    let min = (secs / 60) % 60;
    let hour = (secs / 3_600) % 24;
    let mut days = secs / 86_400;
    let mut year = 1970u64;
    loop {
        let days_in_year = if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
            366
        } else {
            365
        };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }
    let is_leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let mut mon = 1u64;
    for (idx, days_in_month) in DAYS_IN_MONTH.iter().enumerate() {
        let days_in_month = *days_in_month + if idx == 1 && is_leap { 1 } else { 0 };
        if days < days_in_month {
            mon = idx as u64 + 1;
            break;
        }
        days -= days_in_month;
    }
    (year, mon, days + 1, hour, min, sec)
}

lazy_static! {
    static ref RTC: Arc<SpinNoIrqLock<Rtc>> = Arc::new(SpinNoIrqLock::new(Rtc::new(VIRT_RTC)));
}

static RTC_READY: AtomicBool = AtomicBool::new(false);

pub fn init() {
    if !super::rtc_is_supported() {
        error!("rtc init skipped on this platform");
        return;
    }
    let rtc = RTC.lock();
    rtc.init();
    let time_ns = rtc.read_time_ns();
    drop(rtc);
    crate::random::add_entropy(&time_ns.to_le_bytes());
    RTC_READY.store(true, Ordering::Release);
    warn!(
        "rtc init done, realtime = {}.{:09} s",
        time_ns / 1_000_000_000,
        time_ns % 1_000_000_000
    );
}

pub fn rtc_ready() -> bool {
    RTC_READY.load(Ordering::Acquire)
}

pub fn read_time_ns() -> u64 {
    RTC.lock().read_time_ns()
}

pub fn write_time_ns(time_ns: u64) {
    RTC.lock().write_time_ns(time_ns);
}
