//! LoongArch64 QEMU virt platform hooks.

pub use crate::board::{
    BlockDeviceImpl, CharDeviceImpl, QEMUExit, QEMU_EXIT_HANDLE, CLOCK_FREQ, MMIO, VIRT_RTC,
    VIRT_UART,
};

use crate::hal::traits::{HartCtrl, Timer};

/// LoongArch64 platform implementation used by the generic HAL façade.
pub struct LoongArchPlatform;

impl Timer for LoongArchPlatform {
    fn read_time() -> usize {
        crate::arch::loongarch64::read_time()
    }

    fn set_next(deadline: usize) {
        unsafe { crate::arch::loongarch64::set_timer_deadline(deadline) };
    }

    fn clock_freq() -> usize {
        crate::config::CLOCK_FREQ
    }
}

impl HartCtrl for LoongArchPlatform {
    fn start_hart(_hart_id: usize, _start_addr: usize, _opaque: usize) -> Result<(), ()> {
        Err(())
    }

    fn send_ipi(_hart_mask: usize) {
        // Single-core bring-up only for now.
    }
}
