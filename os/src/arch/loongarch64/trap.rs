//! LoongArch64 interrupt control, trap decoding and user-return operations.

use core::arch::{asm, global_asm};

use crate::config::TRAMPOLINE;
use crate::hal::traits::{
    CloneArgs, InterruptControl, NamedReg, SyscallAbi, TrapCause, TrapContextAbi, TrapInfo,
    TrapMachine,
};
use crate::signal::{SignalAbi, SignalAction, SignalBit, SigSetT, StackT};
use crate::syscall::Pod;
use crate::trap::TrapContext;

global_asm!(include_str!("trap.S"));

const CSR_CRMD: usize = 0x0;
const CSR_PRMD: usize = 0x1;
const CSR_EUEN: usize = 0x2;
const CSR_ECFG: usize = 0x4;
const CSR_ESTAT: usize = 0x5;
const CSR_ERA: usize = 0x6;
const CSR_BADV: usize = 0x7;
const CSR_BADI: usize = 0x8;
const CSR_EENTRY: usize = 0xc;
const CSR_TLBRENTRY: usize = 0x88;
const CSR_TLBREHI: usize = 0x8e;
const CSR_PWCL: usize = 0x1c;
const CSR_PWCH: usize = 0x1d;
const CSR_STLBPS: usize = 0x1e;

const CRMD_IE: usize = 1 << 2;
const EUEN_FPEN: usize = 1 << 0;
const ECFG_SIP: usize = 1 << 1;
const ECFG_HWI0: usize = 1 << 2;
const ECFG_TIMER: usize = 1 << 11;
const ECFG_IPI: usize = 1 << 12;
// QEMU `virt` routes EXTIOI sources to CPU IP3, which is exposed in
// ESTAT/ECFG as HWI3 (interrupt number 5, bit 5).
const ECFG_EXTERNAL: usize = ECFG_HWI0 << 3;

const ECODE_INT: usize = 0x0;
const ECODE_PIL: usize = 0x1;
const ECODE_PIS: usize = 0x2;
const ECODE_PIF: usize = 0x3;
const ECODE_PME: usize = 0x4;
const ECODE_ADE: usize = 0x8;
const ECODE_SYS: usize = 0xb;
const ECODE_INE: usize = 0xd;

/// LoongArch64 implementation of [`InterruptControl`](crate::hal::traits::InterruptControl).
pub struct LoongArchInterruptControl;

/// LoongArch64 implementation of trap decoding and user-return operations.
pub struct LoongArchTrapMachine;

/// LoongArch64 register-layout helpers for the common trap context.
pub struct LoongArchTrapContextAbi;

/// LoongArch64 Linux signal ABI implementation.
pub struct LoongArchSignalAbi;

/// LoongArch64 legacy Linux syscall ABI implementation.
pub struct LoongArchSyscallAbi;

/// LoongArch64 trap frame layout shared with `trap.S`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LoongArchTrapContextFrame {
    pub r: [usize; 32],
    pub prmd: usize,
    pub era: usize,
    pub kernel_hartid: usize,
    pub kernel_pgdl: usize,
    pub kernel_sp: usize,
    pub trap_handler: usize,
    pub f: [u64; 32],
    pub fcsr: usize,
}

/// LoongArch musl raw `rt_sigaction` syscall layout:
/// handler, flags, and the low 64 bits of sigset_t.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct LoongArchUserSigAction {
    pub handler: usize,
    pub sa_flags: usize,
    pub sa_mask: u64,
}

impl Pod for LoongArchUserSigAction {}

impl SyscallAbi for LoongArchSyscallAbi {
    fn decode_clone_args(args: [usize; 6]) -> CloneArgs {
        // Linux LoongArch does not select CONFIG_CLONE_BACKWARDS:
        // clone(flags, stack, parent_tidptr, child_tidptr, tls).
        CloneArgs {
            flags: args[0],
            stack: args[1],
            parent_tid: args[2],
            tls: args[4],
            child_tid: args[3],
        }
    }
}

/// LoongArch musl `mcontext_t`.
///
/// Verified against the local LoongArch musl headers:
/// size = 272, align = 16, pc at 0, gregs at 8, flags at 264.
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub struct LoongArchMContext {
    pub pc: usize,
    pub gregs: [usize; 32],
    pub flags: u32,
    pub _pad: u32,
}

impl Pod for LoongArchMContext {}

const _: [(); 272] = [(); core::mem::size_of::<LoongArchMContext>()];
const _: [(); 16] = [(); core::mem::align_of::<LoongArchMContext>()];

#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub struct LoongArchUContext {
    pub uc_flags: usize,
    pub uc_link: usize,
    pub uc_stack: StackT,
    pub uc_sigmask: SigSetT,
    pub uc_pad: isize,
    pub uc_mcontext: LoongArchMContext,
}

impl Pod for LoongArchUContext {}

const _: [(); 448] = [(); core::mem::size_of::<LoongArchUContext>()];
const _: [(); 16] = [(); core::mem::align_of::<LoongArchUContext>()];

/// 用户态 `rt_sigreturn` trampoline 机器码。
///
/// 等价指令序列：
///   ori     $a7, $zero, 139
///   syscall 0
const USER_VDSO_CODE: [u8; 8] = [
    0x0b, 0x2c, 0x82, 0x03, // ori $a7, $zero, 139
    0x00, 0x00, 0x2b, 0x00, // syscall 0
];

impl InterruptControl for LoongArchInterruptControl {
    unsafe fn enable_timer() {
        update_ecfg(ECFG_TIMER, true);
    }

    unsafe fn disable_timer() {
        update_ecfg(ECFG_TIMER, false);
    }

    unsafe fn enable_external() {
        update_ecfg(ECFG_EXTERNAL, true);
    }

    unsafe fn disable_external() {
        update_ecfg(ECFG_EXTERNAL, false);
    }

    unsafe fn enable_software() {
        update_ecfg(ECFG_SIP, true);
        update_ecfg(ECFG_IPI, true);
    }

    unsafe fn clear_software_pending() {
        asm!(
            "csrrd $t0, {estat}",
            "and $t0, $t0, {mask}",
            "csrwr $t0, {estat}",
            estat = const CSR_ESTAT,
            mask = in(reg) (!(1usize << 1)),
            out("$t0") _,
        );
    }

    unsafe fn set_kernel_trap_entry() {
        extern "C" {
            fn __trap_from_kernel();
            fn __tlb_refill();
        }
        // PWCL: PTbase=12, PTwidth=9, Dir1base=21, Dir1width=9, Dir2base=30, Dir2width=9
        // PWCH: Dir3=unused (3-level paging: root=Dir2, middle=Dir1, leaf=PT)
        const PWCL: usize = 12 | (9 << 5) | (21 << 10) | (9 << 15) | (30 << 20) | (9 << 25);
        const PWCH: usize = 0;
        asm!(
            "csrwr {eentry}, {eentry_csr}",
            "csrwr {tlbr}, {tlbr_csr}",
            "csrwr {pwcl}, {pwcl_csr}",
            "csrwr {pwch}, {pwch_csr}",
            // STLBPS / TLBREHI.PS: page size = 12 (4KB) for software-managed
            // refill entries as well as the shared TLB configuration.
            "ori   $t0, $zero, 12",
            "csrwr $t0, {stlbps_csr}",
            "csrwr $t0, {tlbrehi_csr}",
            eentry     = in(reg) (__trap_from_kernel as usize),
            eentry_csr = const CSR_EENTRY,
            tlbr       = in(reg) (__tlb_refill as usize),
            tlbr_csr   = const CSR_TLBRENTRY,
            pwcl       = in(reg) PWCL,
            pwcl_csr   = const CSR_PWCL,
            pwch       = in(reg) PWCH,
            pwch_csr   = const CSR_PWCH,
            stlbps_csr = const CSR_STLBPS,
            tlbrehi_csr = const CSR_TLBREHI,
            out("$t0") _,
        );
    }

    unsafe fn set_user_trap_entry() {
        extern "C" {
            fn __alltraps();
            fn strampoline();
        }
        let trap_entry = __alltraps as usize - strampoline as usize + TRAMPOLINE;
        asm!(
            "csrwr {entry}, {eentry}",
            entry = in(reg) trap_entry,
            eentry = const CSR_EENTRY,
        );
    }
}

impl TrapMachine for LoongArchTrapMachine {
    fn read_trap_info() -> TrapInfo {
        let estat = read_estat();
        let ecfg = read_ecfg();
        let badv = read_badv();
        let ecode = (estat >> 16) & 0x3f;
        let cause = match ecode {
            ECODE_SYS => TrapCause::UserSyscall,
            ECODE_PIS | ECODE_PME => TrapCause::StorePageFault,
            ECODE_PIL => TrapCause::LoadPageFault,
            ECODE_PIF => TrapCause::InstructionPageFault,
            ECODE_INE => TrapCause::IllegalInstruction,
            ECODE_ADE => TrapCause::InstructionFault,
            ECODE_INT => {
                decode_interrupt_cause(estat, ecfg)
            }
            _ => TrapCause::Unknown,
        };
        TrapInfo { cause, fault_addr: badv }
    }

    unsafe fn return_to_user(trap_cx_user_va: usize, user_token: usize) -> ! {
        extern "C" {
            fn __restore();
            fn strampoline();
        }
        let restore_va = __restore as usize - strampoline as usize + TRAMPOLINE;
        asm!(
            "ibar 0",
            "jirl $zero, {restore}, 0",
            restore = in(reg) restore_va,
            in("$a0") trap_cx_user_va,
            in("$a1") user_token,
            options(noreturn)
        );
    }

    fn syscall_instruction_len() -> usize {
        4
    }

    fn rt_sigreturn_trampoline() -> &'static [u8] {
        &USER_VDSO_CODE
    }
}

impl SignalAbi for LoongArchSignalAbi {
    type UserSigAction = LoongArchUserSigAction;
    type UContext = LoongArchUContext;

    fn decode_user_sigaction(action: Self::UserSigAction) -> SignalAction {
        SignalAction {
            handler: action.handler,
            sa_flags: action.sa_flags as u32,
            sa_restorer: 0,
            sa_mask: SignalBit::from_user_bits(action.sa_mask).bits(),
        }
    }

    fn encode_user_sigaction(action: SignalAction) -> Self::UserSigAction {
        Self::UserSigAction {
            handler: action.handler,
            sa_flags: action.sa_flags as usize,
            sa_mask: SignalBit::from_bits(action.sa_mask)
                .unwrap_or(SignalBit::empty())
                .user_bits(),
        }
    }

    fn user_sigaction_parts(action: &Self::UserSigAction) -> (usize, usize, usize, u64) {
        (action.handler, action.sa_flags, 0, action.sa_mask)
    }

    fn build_ucontext(trap_cx: &TrapContext, old_mask: u64) -> Self::UContext {
        let mut gregs = [0usize; 32];
        for (idx, reg) in gregs.iter_mut().enumerate() {
            *reg = trap_cx.reg(idx);
        }
        gregs[0] = 0;

        Self::UContext {
            uc_flags: 0,
            uc_link: 0,
            uc_stack: StackT {
                ss_sp: 0,
                ss_flags: 0,
                ss_size: 0,
            },
            uc_sigmask: SigSetT::from_signal_bits(old_mask),
            uc_pad: 0,
            uc_mcontext: LoongArchMContext {
                pc: trap_cx.user_pc(),
                gregs,
                flags: 0,
                _pad: 0,
            },
        }
    }

    fn signal_mask(ucontext: &Self::UContext) -> u64 {
        ucontext.uc_sigmask.low_bits()
    }

    fn restore_ucontext(ucontext: &Self::UContext, trap_cx: &mut TrapContext) {
        trap_cx.set_user_pc(ucontext.uc_mcontext.pc);
        for idx in 1..32 {
            trap_cx.set_reg(idx, ucontext.uc_mcontext.gregs[idx]);
        }
    }

    fn saved_pc(ucontext: &Self::UContext) -> usize {
        ucontext.uc_mcontext.pc
    }

    fn set_saved_pc(ucontext: &mut Self::UContext, pc: usize) {
        ucontext.uc_mcontext.pc = pc;
    }

    fn saved_arg0(ucontext: &Self::UContext) -> usize {
        let index = <LoongArchTrapContextAbi as TrapContextAbi>::signal_gpr_arg0_index();
        ucontext.uc_mcontext.gregs[index]
    }

    fn set_saved_arg0(ucontext: &mut Self::UContext, value: usize) {
        let index = <LoongArchTrapContextAbi as TrapContextAbi>::signal_gpr_arg0_index();
        ucontext.uc_mcontext.gregs[index] = value;
    }
}

impl TrapContextAbi for LoongArchTrapContextAbi {
    type Frame = LoongArchTrapContextFrame;

    fn new_user_frame(
        entry: usize,
        sp: usize,
        kernel_token: usize,
        kernel_sp: usize,
        trap_handler: usize,
    ) -> Self::Frame {
        let mut frame = LoongArchTrapContextFrame {
            r: [0; 32],
            // PPLV=3 (user) and PIE=1 so `ertn` returns to PLV3 with
            // interrupts restored according to the saved user context.
            prmd: 0b0111,
            era: entry,
            kernel_hartid: 0,
            kernel_pgdl: kernel_token,
            kernel_sp,
            trap_handler,
            f: [0; 32],
            fcsr: 0,
        };
        frame.r[3] = sp;
        frame
    }

    fn reg(frame: &Self::Frame, index: usize) -> usize {
        frame.r[index]
    }

    fn set_reg(frame: &mut Self::Frame, index: usize, value: usize) {
        if index != 0 {
            frame.r[index] = value;
        }
    }

    fn user_pc(frame: &Self::Frame) -> usize {
        frame.era
    }

    fn set_user_pc(frame: &mut Self::Frame, pc: usize) {
        frame.era = pc;
    }

    fn user_sp(frame: &Self::Frame) -> usize {
        frame.r[3]
    }

    fn set_user_sp(frame: &mut Self::Frame, sp: usize) {
        frame.r[3] = sp;
    }

    fn ra(frame: &Self::Frame) -> usize {
        frame.r[1]
    }

    fn set_ra(frame: &mut Self::Frame, ra: usize) {
        frame.r[1] = ra;
    }

    fn tls(frame: &Self::Frame) -> usize {
        frame.r[2]
    }

    fn set_tls(frame: &mut Self::Frame, tls: usize) {
        frame.r[2] = tls;
    }

    fn syscall_nr(frame: &Self::Frame) -> usize {
        frame.r[11]
    }

    fn syscall_args(frame: &Self::Frame) -> [usize; 6] {
        [frame.r[4], frame.r[5], frame.r[6], frame.r[7], frame.r[8], frame.r[9]]
    }

    fn syscall_ret(frame: &Self::Frame) -> usize {
        frame.r[4]
    }

    fn set_syscall_ret(frame: &mut Self::Frame, ret: usize) {
        frame.r[4] = ret;
    }

    fn set_user_arg(frame: &mut Self::Frame, index: usize, value: usize) {
        frame.r[4 + index] = value;
    }

    fn set_kernel_hartid(frame: &mut Self::Frame, hartid: usize) {
        frame.kernel_hartid = hartid;
    }

    fn set_kernel_sp(frame: &mut Self::Frame, kernel_sp: usize) {
        frame.kernel_sp = kernel_sp;
    }

    fn export_signal_gprs(frame: &Self::Frame) -> [usize; 32] {
        let mut exported = frame.r;
        exported[0] = frame.era;
        exported
    }

    fn import_signal_gprs(frame: &mut Self::Frame, signal_gprs: &[usize; 32]) {
        frame.r.copy_from_slice(signal_gprs);
        frame.r[0] = 0;
        frame.era = signal_gprs[0];
    }

    fn signal_gpr_arg0_index() -> usize {
        4 // LoongArch: r4 = a0
    }

    fn copy_fp_state_to(frame: &Self::Frame, fpregs: &mut [u64; 32], fcsr: &mut u32) {
        fpregs.copy_from_slice(&frame.f);
        *fcsr = frame.fcsr as u32;
    }

    fn restore_fp_state(frame: &mut Self::Frame, fpregs: &[u64; 32], fcsr: u32) {
        frame.f.copy_from_slice(fpregs);
        frame.fcsr = fcsr as usize;
    }

    fn fault_dump_summary(frame: &Self::Frame) -> [NamedReg; 7] {
        [
            NamedReg { name: "ra", value: frame.r[1] },
            NamedReg { name: "sp", value: frame.r[3] },
            NamedReg { name: "fp", value: frame.r[22] },
            NamedReg { name: "tp", value: frame.r[2] },
            NamedReg { name: "a0", value: frame.r[4] },
            NamedReg { name: "a1", value: frame.r[5] },
            NamedReg { name: "a7", value: frame.r[11] },
        ]
    }

    fn fault_dump_detail(frame: &Self::Frame) -> [NamedReg; 19] {
        [
            NamedReg { name: "a2", value: frame.r[6] },
            NamedReg { name: "a3", value: frame.r[7] },
            NamedReg { name: "a4", value: frame.r[8] },
            NamedReg { name: "a5", value: frame.r[9] },
            NamedReg { name: "a6", value: frame.r[10] },
            NamedReg { name: "t0", value: frame.r[12] },
            NamedReg { name: "t1", value: frame.r[13] },
            NamedReg { name: "t2", value: frame.r[14] },
            NamedReg { name: "t3", value: frame.r[15] },
            NamedReg { name: "t4", value: frame.r[16] },
            NamedReg { name: "t5", value: frame.r[17] },
            NamedReg { name: "t6", value: frame.r[18] },
            NamedReg { name: "t7", value: frame.r[19] },
            NamedReg { name: "t8", value: frame.r[20] },
            NamedReg { name: "u0", value: frame.r[21] },
            NamedReg { name: "s0", value: frame.r[23] },
            NamedReg { name: "s1", value: frame.r[24] },
            NamedReg { name: "s2", value: frame.r[25] },
            NamedReg { name: "s3", value: frame.r[26] },
        ]
    }
}

#[inline]
fn read_estat() -> usize {
    let value: usize;
    unsafe { asm!("csrrd {}, {}", out(reg) value, const CSR_ESTAT) };
    value
}

#[inline]
fn read_ecfg() -> usize {
    let value: usize;
    unsafe { asm!("csrrd {}, {}", out(reg) value, const CSR_ECFG) };
    value
}

#[inline]
fn read_badv() -> usize {
    let value: usize;
    unsafe { asm!("csrrd {}, {}", out(reg) value, const CSR_BADV) };
    value
}

#[inline]
fn read_badi() -> usize {
    let value: usize;
    unsafe { asm!("csrrd {}, {}", out(reg) value, const CSR_BADI) };
    value
}

#[inline]
fn decode_interrupt_cause(estat: usize, ecfg: usize) -> TrapCause {
    let int_vec = (estat & ecfg) & 0x1fff;
    let highest = (0..=12).rev().find(|bit| int_vec & (1usize << bit) != 0);
    match highest {
        Some(12) => TrapCause::SoftwareInterrupt,
        Some(11) => TrapCause::TimerInterrupt,
        Some(1) | Some(0) => TrapCause::SoftwareInterrupt,
        Some(2..=9) => TrapCause::ExternalInterrupt,
        Some(10) => TrapCause::Unknown,
        None => TrapCause::Unknown,
        Some(_) => TrapCause::Unknown,
    }
}

#[inline]
unsafe fn update_ecfg(mask: usize, enable: bool) {
    let mut ecfg: usize;
    asm!("csrrd {}, {}", out(reg) ecfg, const CSR_ECFG);
    if enable {
        ecfg |= mask;
    } else {
        ecfg &= !mask;
    }
    asm!("csrwr {}, {}", in(reg) ecfg, const CSR_ECFG);
}
