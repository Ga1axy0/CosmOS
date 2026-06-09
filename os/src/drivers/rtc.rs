//! Goldfish RTC 驱动。
// 文档： https://android.googlesource.com/platform/external/qemu/%2B/master/docs/GOLDFISH-VIRTUAL-HARDWARE.TXT
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering};

use alloc::sync::Arc;
use lazy_static::lazy_static;

use crate::board::VIRT_RTC;
use crate::sync::SpinNoIrqLock;

/// Goldfish RTC 时间低 32 位寄存器。
const REG_TIME_LOW: usize = 0x00;
/// Goldfish RTC 时间高 32 位寄存器。
const REG_TIME_HIGH: usize = 0x04;
/// Goldfish RTC 闹钟低 32 位寄存器。
const REG_ALARM_LOW: usize = 0x08;
/// Goldfish RTC 闹钟高 32 位寄存器。
const REG_ALARM_HIGH: usize = 0x0c;
/// Goldfish RTC 中断清除寄存器。
const REG_CLEAR_INTERRUPT: usize = 0x10;

/// 简单的 MMIO 寄存器访问封装。
#[derive(Copy, Clone)]
struct Mmio<T> {
    addr: *mut T,
    _pd: PhantomData<T>,
}

impl<T> Mmio<T> {
    /// 根据 MMIO 地址创建访问句柄。
    const fn new(addr: usize) -> Self {
        Self {
            addr: addr as *mut T,
            _pd: PhantomData,
        }
    }
}

impl<T: Copy> Mmio<T> {
    /// 以 volatile 方式读取寄存器。
    fn read(&self) -> T {
        unsafe { core::ptr::read_volatile(self.addr) }
    }
}

impl<T> Mmio<T> {
    /// 以 volatile 方式写寄存器。
    fn write(&self, value: T) {
        unsafe { core::ptr::write_volatile(self.addr, value) }
    }
}

/// Goldfish RTC 原始寄存器访问器。
struct GoldfishRtcRaw {
    base_addr: usize,
}

impl GoldfishRtcRaw {
    /// 创建一个绑定到给定基址的 RTC 访问器。
    const fn new(base_addr: usize) -> Self {
        Self { base_addr }
    }

    /// 读取一个 32 位寄存器。
    fn reg(&self, offset: usize) -> Mmio<u32> {
        Mmio::new(self.base_addr + offset)
    }

    /// 初始化 RTC 设备。
    fn init(&self) {
        // Goldfish RTC 在 virt 机型上默认可直接读时间；这里主动清一次中断状态。
        self.reg(REG_CLEAR_INTERRUPT).write(1);
        // 兼容旧实现中暴露出来但当前未使用的寄存器，避免后续误判未映射。
        let _ = self.reg(REG_ALARM_LOW);
        let _ = self.reg(REG_ALARM_HIGH);
    }

    /// 读取当前 RTC 时间，单位为纳秒。
    fn read_time_ns(&self) -> u64 {
        // 按设备规范必须先读 TIME_LOW，再读 TIME_HIGH，后者返回前一次低位读取对应的高位快照。
        let low = self.reg(REG_TIME_LOW).read() as u64;
        let high = self.reg(REG_TIME_HIGH).read() as u64;
        (high << 32) | low
    }

    /// 写入当前 RTC 时间，单位为纳秒。
    fn write_time_ns(&self, time_ns: u64) {
        // TODO：Goldfish RTC 对 TIME_LOW/TIME_HIGH 的写入不是原子的；
        // 这里先提供一个“尽力设置当前时间”的接口，后续若用于严格校时，
        // 需要增加回读校验或改为更高层 offset 方案。
        self.reg(REG_TIME_LOW).write(time_ns as u32);
        self.reg(REG_TIME_HIGH).write((time_ns >> 32) as u32);
    }
}

/// RTC 驱动实例的内部状态。
struct GoldfishRtc {
    raw: GoldfishRtcRaw,
}

impl GoldfishRtc {
    /// 创建一个新的 Goldfish RTC 驱动实例。
    fn new(base_addr: usize) -> Self {
        Self {
            raw: GoldfishRtcRaw::new(base_addr),
        }
    }

    /// 初始化底层硬件状态。
    fn init(&self) {
        self.raw.init();
    }

    /// 读取 RTC 当前时间，单位为纳秒。
    fn read_time_ns(&self) -> u64 {
        self.raw.read_time_ns()
    }

    /// 写入 RTC 当前时间，单位为纳秒。
    fn write_time_ns(&self, time_ns: u64) {
        self.raw.write_time_ns(time_ns);
    }
}

lazy_static! {
    /// 全局 RTC 驱动实例。
    static ref RTC: Arc<SpinNoIrqLock<GoldfishRtc>> =
        Arc::new(SpinNoIrqLock::new(GoldfishRtc::new(VIRT_RTC)));
}

/// 标记 RTC 是否已完成初始化。
static RTC_READY: AtomicBool = AtomicBool::new(false);

#[cfg(target_arch = "loongarch64")]
/// 初始化全局 RTC 驱动。
pub fn init() {
    // QEMU loongarch64 virt does not currently expose the Goldfish RTC MMIO
    // block at the RISC-V-compatible address we use elsewhere.
    warn!("rtc init skipped on loongarch64 virt");
}

#[cfg(not(target_arch = "loongarch64"))]
/// 初始化全局 RTC 驱动。
pub fn init() {
    let rtc = RTC.lock();
    rtc.init();
    let time_ns = rtc.read_time_ns();
    drop(rtc);
    // Mix RTC-derived timestamp into kernel entropy pool so getrandom can seed early.
    crate::random::add_entropy(&time_ns.to_le_bytes());
    RTC_READY.store(true, Ordering::Release);
    info!(
        "rtc init done, realtime = {}.{:09} s",
        time_ns / 1_000_000_000,
        time_ns % 1_000_000_000
    );
}

/// 返回 RTC 是否已完成初始化。
pub fn rtc_ready() -> bool {
    RTC_READY.load(Ordering::Acquire)
}

/// 读取当前 RTC 时间，单位为纳秒。
pub fn read_time_ns() -> u64 {
    RTC.lock().read_time_ns()
}

/// 写入当前 RTC 时间，单位为纳秒。
pub fn write_time_ns(time_ns: u64) {
    RTC.lock().write_time_ns(time_ns);
}
