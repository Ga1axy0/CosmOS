//! QEMU virt platform — memory map, exit, and board types.

pub mod sbi;

pub use crate::board::{
    BlockDeviceImpl, CharDeviceImpl, QEMUExit, QEMU_EXIT_HANDLE, CLOCK_FREQ, MMIO,
    RISCV64, VIRT_RTC, VIRT_UART,
};
pub use sbi::SbiPlatform;
