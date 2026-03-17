//! block device driver

pub mod block;
pub mod chardev;
pub mod plic;
pub use block::BLOCK_DEVICE;

/// Initialize all drivers (block, char, PLIC, …).
pub fn init() {
    chardev::init();
    plic::init();
    block::probe_block_devices();
}