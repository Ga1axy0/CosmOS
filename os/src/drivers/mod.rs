//! Reusable device drivers.
//!
//! Drivers implement one device IP block or transport protocol such as
//! NS16550A or VirtIO. They should stay as platform-agnostic as
//! practical; the `platform` layer is responsible for deciding which drivers
//! are instantiated and how their MMIO ranges and IRQs are routed.

pub mod block;
pub mod chardev;
pub mod net;
pub mod plic;
pub mod virtio;
pub use block::BLOCK_DEVICE;

/// Initialize reusable drivers.
pub fn init() {
    chardev::init();
}
