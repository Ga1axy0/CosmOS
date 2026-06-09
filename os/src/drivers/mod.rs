//! block device driver

pub mod block;
pub mod chardev;
pub mod net;
pub mod pci;
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
    #[cfg(target_arch = "riscv64")]
    {
        plic::init();
        block::probe_block_devices();
        net::probe_net_devices();
    }
    #[cfg(target_arch = "loongarch64")]
    {
        pci::probe_virtio_pci_devices();
    }
}
