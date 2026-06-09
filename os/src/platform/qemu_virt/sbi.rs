//! SBI platform for QEMU virt — implements Timer and HartCtrl HAL traits.

pub use crate::sbi::{
    hart_get_status, hart_start, hart_state, send_ipi_mask,
    set_timer, shutdown, HartState, SbiRet,
};

use crate::hal::traits::{HartCtrl, Timer};

/// SBI-backed implementation of [`Timer`] and [`HartCtrl`] for QEMU virt.
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
