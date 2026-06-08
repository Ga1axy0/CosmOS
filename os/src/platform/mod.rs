//! Platform-specific implementations.
#![allow(missing_docs)]

#[cfg(target_arch = "riscv64")]
pub mod qemu_virt;

#[cfg(target_arch = "loongarch64")]
pub mod loongarch_virt;
