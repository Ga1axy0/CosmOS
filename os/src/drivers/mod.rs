//! block device driver

pub mod block;
pub mod chardev;
pub mod net;
pub mod plic;
pub mod rtc;
pub mod virtio;
pub use block::BLOCK_DEVICE;

/// Initialize all drivers (block, char, PLIC, …).
pub fn init() {
    chardev::init();
    rtc::init();
    plic::init();
    block::probe_block_devices();
    net::probe_net_devices();
}
