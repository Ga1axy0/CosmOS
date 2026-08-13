//! LoongArch64 hart-local register access.

use core::arch::asm;

use crate::hal::traits::HartId;

const CSR_CPUID: usize = 0x20;
const CSR_CRMD: usize = 0x0;
const CSR_EUEN: usize = 0x2;
const CSR_MISC: usize = 0x3;
const CSR_TCFG: usize = 0x41;
const CSR_TICLR: usize = 0x44;
const CRMD_IE: usize = 1 << 2;
const EUEN_FPEN: usize = 1 << 0;
const CPUCFG1_UAL: usize = 1 << 20;
const CPUCFG2_LSX: usize = 1 << 6;
const EUEN_SXEN: usize = 1 << 1;
const TCFG_ENABLE: usize = 1 << 0;
const TCFG_PERIODIC: usize = 1 << 1;
const TICLR_CLEAR: usize = 1 << 0;
const MISC_ALCL3: usize = 1 << 15;

/// LoongArch64 implementation of [`HartId`](crate::hal::traits::HartId).
pub struct LoongArchHartId;

#[inline]
fn read_cpucfg(index: usize) -> usize {
    let value: usize;
    unsafe {
        asm!(
            "cpucfg {value}, {index}",
            value = out(reg) value,
            index = in(reg) index,
        )
    };
    value
}

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

    unsafe fn init(_id: usize) {
        // The LoongArch LP64 userspace ABI and common toolchains emit ordinary
        // LD/ST instructions at non-natural addresses. On implementations
        // advertising CPUCFG.1.UAL, clear the PLV3 alignment-check bit so
        // those accesses are handled by hardware instead of raising ALE.
        // QEMU's LA464 model reports UAL but does not implement CSR.MISC
        // writes, so this board-specific control is only touched on LS2K1000.
        #[cfg(feature = "platform-ls2k1000-nebula")]
        if read_cpucfg(1) & CPUCFG1_UAL != 0 {
            let mut misc: usize;
            asm!("csrrd {}, {}", out(reg) misc, const CSR_MISC);
            misc &= !MISC_ALCL3;
            asm!("csrwr {}, {}", in(reg) misc, const CSR_MISC);
        }
    }

    unsafe fn enable_fp() {
        let has_lsx = read_cpucfg(2) & CPUCFG2_LSX != 0;
        let mut euen: usize;
        asm!("csrrd {}, {}", out(reg) euen, const CSR_EUEN);
        euen |= EUEN_FPEN;
        if has_lsx {
            euen |= EUEN_SXEN;
        } else {
            euen &= !EUEN_SXEN;
        }
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
