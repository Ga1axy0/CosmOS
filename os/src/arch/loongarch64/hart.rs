//! LoongArch64 hart-local register access.

use core::arch::asm;

use crate::hal::traits::HartId;

const CSR_CPUID: usize = 0x20;
const CSR_CRMD: usize = 0x0;
const CSR_ECFG: usize = 0x4;
const CSR_TCFG: usize = 0x41;
const CSR_TVAL: usize = 0x42;
const CRMD_IE: usize = 1 << 2;
const ECFG_FPE: usize = 1 << 0;
const TCFG_ENABLE: usize = 1 << 0;

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
    let delta = deadline.saturating_sub(now).max(1);
    asm!(
        "csrwr {delta}, {tval}",
        "csrwr {tcfg}, {tcfg_num}",
        delta = in(reg) delta,
        tcfg = in(reg) (TCFG_ENABLE | (1usize << 1)),
        tval = const CSR_TVAL,
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
        let mut ecfg: usize;
        asm!("csrrd {}, {}", out(reg) ecfg, const CSR_ECFG);
        ecfg |= ECFG_FPE;
        asm!("csrwr {}, {}", in(reg) ecfg, const CSR_ECFG);
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
