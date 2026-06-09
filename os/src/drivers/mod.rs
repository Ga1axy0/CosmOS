//! block device driver

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
    #[cfg(target_arch = "riscv64")]
    {
        block::probe_block_devices();
        net::probe_net_devices();
    }
    crate::platform::probe_platform_devices();
}
