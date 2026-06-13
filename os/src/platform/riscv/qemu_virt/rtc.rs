use core::sync::atomic::{AtomicBool, Ordering};

use alloc::sync::Arc;
use lazy_static::lazy_static;

use crate::sync::SpinNoIrqLock;

use super::VIRT_RTC;

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
        // Clear any pending interrupt.
        mmio_write32(self.base + 0x10, 1);
    }

    fn read_time_ns(&self) -> u64 {
        // Must read LOW first; HIGH then latches to match that read.
        let low = mmio_read32(self.base + 0x00) as u64;
        let high = mmio_read32(self.base + 0x04) as u64;
        (high << 32) | low
    }

    fn write_time_ns(&self, time_ns: u64) {
        // TODO: writes are non-atomic on Goldfish RTC.
        mmio_write32(self.base + 0x00, time_ns as u32);
        mmio_write32(self.base + 0x04, (time_ns >> 32) as u32);
    }
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
