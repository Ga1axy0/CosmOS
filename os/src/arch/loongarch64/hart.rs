//! LoongArch64 hart-local register access.

use core::arch::asm;

use crate::hal::traits::HartId;

const CSR_CPUID: usize = 0x20;
const CSR_CRMD: usize = 0x0;
const CSR_EUEN: usize = 0x2;
const CSR_TCFG: usize = 0x41;
const CSR_TICLR: usize = 0x44;
const CRMD_IE: usize = 1 << 2;
const EUEN_FPEN: usize = 1 << 0;
const TCFG_ENABLE: usize = 1 << 0;
const TCFG_PERIODIC: usize = 1 << 1;
const TICLR_CLEAR: usize = 1 << 0;

/// LoongArch64 implementation of [`HartId`](crate::hal::traits::HartId).
pub struct LoongArchHartId;

#[inline]
pub fn read_time() -> usize {
    let time: usize;
    unsafe { asm!("rdtime.d {}, $zero", out(reg) time) };
    time
}

#[inline]
pub unsafe fn set_timer_deadline(deadline: usize) {
    let now = read_time();
    // TCFG.InitVal requires a multiple-of-4 countdown value.
    let delta = deadline.saturating_sub(now).max(4) & !0b11;
    asm!(
        "csrwr {clear}, {ticlr}",
        "csrwr {tcfg}, {tcfg_num}",
        clear = in(reg) TICLR_CLEAR,
        tcfg = in(reg) (delta | TCFG_ENABLE),
        ticlr = const CSR_TICLR,
        tcfg_num = const CSR_TCFG,
    );
}

impl HartId for LoongArchHartId {
    fn current() -> usize {
        let id: usize;
        unsafe { asm!("csrrd {}, {}", out(reg) id, const CSR_CPUID) }
        id
    }

    unsafe fn init(_id: usize) {}

    unsafe fn enable_fp() {
        let mut euen: usize;
        asm!("csrrd {}, {}", out(reg) euen, const CSR_EUEN);
        euen |= EUEN_FPEN;
        asm!("csrwr {}, {}", in(reg) euen, const CSR_EUEN);
    }

    fn irqs_enabled() -> bool {
        let crmd: usize;
        unsafe { asm!("csrrd {}, {}", out(reg) crmd, const CSR_CRMD) };
        crmd & CRMD_IE != 0
    }

    unsafe fn disable_irqs() {
        let mut crmd: usize;
        asm!("csrrd {}, {}", out(reg) crmd, const CSR_CRMD);
        crmd &= !CRMD_IE;
        asm!("csrwr {}, {}", in(reg) crmd, const CSR_CRMD);
    }

    unsafe fn enable_irqs() {
        let mut crmd: usize;
        asm!("csrrd {}, {}", out(reg) crmd, const CSR_CRMD);
        crmd |= CRMD_IE;
        asm!("csrwr {}, {}", in(reg) crmd, const CSR_CRMD);
    }

    unsafe fn wait_for_interrupt() {
        asm!("idle 0");
    }
}
