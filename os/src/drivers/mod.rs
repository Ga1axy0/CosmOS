//! Reusable device drivers.
//!
//! Drivers implement one device IP block or transport protocol such as
//! NS16550A, VirtIO, or Goldfish RTC. They should stay as platform-agnostic as
//! practical; the `platform` layer is responsible for deciding which drivers
//! are instantiated and how their MMIO ranges and IRQs are routed.

pub mod block;
pub mod chardev;
pub mod net;
pub mod plic;
pub mod rtc;
pub mod virtio;
pub use block::BLOCK_DEVICE;

fn virtio_blk_name(idx: usize) -> String {
    alloc::format!("vd{}", (b'a' + idx as u8) as char)
}

/// Initialize all drivers (block, char, PLIC, …).
pub fn init() {
    chardev::init();
    rtc::init();
    crate::platform::init_external_irq();
    crate::platform::probe_platform_devices();
}
