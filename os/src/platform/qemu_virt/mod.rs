//! QEMU virt platform — memory map, exit, and board types.

pub mod sbi;

pub use crate::board::{
    BlockDeviceImpl, CharDeviceImpl, QEMUExit, QEMU_EXIT_HANDLE, CLOCK_FREQ, MMIO,
    VIRT_RTC, VIRT_UART,
};
pub use sbi::SbiPlatform;

/// Initialize platform external interrupt routing on the bootstrap hart.
pub fn init_external_irq() {
    crate::drivers::plic::init();
}

/// Initialize per-hart external interrupt state.
pub fn init_external_irq_hart(hart_id: usize) {
    crate::drivers::plic::init_hart(hart_id);
}

/// Dispatch one platform external interrupt.
pub fn handle_external_irq() {
    crate::drivers::plic::handle_supervisor_external();
}

/// Whether the console RX interrupt path is ready for blocking reads.
pub fn console_rx_irq_ready() -> bool {
    true
}

/// Probe platform-specific devices after generic driver init.
pub fn probe_platform_devices() {}
