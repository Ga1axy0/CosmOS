//! SBI glue for the RISC-V QEMU `virt` platform.

pub(crate) use crate::sbi::{
    console_getchar_raw as console_getchar, console_putchar_raw as console_putchar,
    hart_get_status_raw as hart_get_status, hart_start_raw as hart_start, hart_state,
    send_ipi_mask_raw as send_ipi_mask, set_timer_raw as set_timer, shutdown_raw as shutdown,
    HartState,
};

use crate::hal::traits::{HartCtrl, Timer};

/// SBI-backed implementation of [`Timer`] and [`HartCtrl`] for QEMU `virt`.
pub struct SbiPlatform;

impl Timer for SbiPlatform {
    fn read_time() -> usize {
        riscv::register::time::read()
    }

    fn set_next(deadline: usize) {
        set_timer(deadline);
    }

    fn clock_freq() -> usize {
        crate::config::CLOCK_FREQ
    }
}

impl HartCtrl for SbiPlatform {
    fn start_hart(hart_id: usize, start_addr: usize, opaque: usize) -> Result<(), ()> {
        let ret = hart_start(hart_id, start_addr, opaque);
        if ret.error == 0 { Ok(()) } else { Err(()) }
    }

    fn send_ipi(hart_mask: usize) {
        send_ipi_mask(hart_mask);
    }
}
