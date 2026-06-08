//! Architecture-specific implementations.

#[cfg(target_arch = "riscv64")]
pub mod riscv;

#[cfg(target_arch = "loongarch64")]
pub mod loongarch64;
