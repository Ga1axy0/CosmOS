//! Platform-specific implementations.
#![allow(missing_docs)]

#[cfg(target_arch = "riscv64")]
pub mod qemu_virt;

#[cfg(target_arch = "loongarch64")]
pub mod loongarch_virt;

#[cfg(target_arch = "riscv64")]
pub use qemu_virt::{
    console_rx_irq_ready, handle_external_irq, init_external_irq, init_external_irq_hart,
    probe_platform_devices,
};

#[cfg(target_arch = "loongarch64")]
pub use loongarch_virt::{
    console_rx_irq_ready, handle_external_irq, init_external_irq, init_external_irq_hart,
    probe_platform_devices,
};
