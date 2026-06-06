//! HAL — re-exports arch/platform concrete types under stable aliases.
#![allow(missing_docs)]

pub mod traits;

#[cfg(target_arch = "riscv64")]
pub use crate::arch::riscv::{RiscvHartId as ArchHart, RiscvInterruptControl as ArchInterrupt};

#[cfg(feature = "platform-qemu-virt")]
pub use crate::platform::qemu_virt::SbiPlatform as Plat;
