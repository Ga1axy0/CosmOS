//! RISC-V interrupt control, implementing [`InterruptControl`](crate::hal::traits::InterruptControl).

use crate::config::TRAMPOLINE;
use crate::hal::traits::{
    CloneArgs, InterruptControl, NamedReg, SyscallAbi, TrapCause, TrapContextAbi, TrapInfo,
    TrapMachine,
};
use crate::signal::{SigSetT, SignalAbi, SignalAction, SignalBit, StackT};
use crate::syscall::Pod;
use crate::trap::TrapContext;
use core::arch::{asm, global_asm};
use riscv::register::{
    mtvec::TrapMode,
    scause::{self, Exception, Interrupt, Trap},
    sie,
    sstatus::{self, Sstatus, FS, SPP},
    stval, stvec,
};

#[cfg(feature = "kernel_trap_diagnostics")]
use riscv::register::{satp, sepc};

#[cfg(all(feature = "getpid_asm_probe", feature = "getpid_asm_satp_probe"))]
compile_error!("getpid assembly probe modes are mutually exclusive");

const GETPID_ASM_PROBE_MODE: usize = if cfg!(feature = "getpid_asm_satp_probe") {
    2
} else if cfg!(feature = "getpid_asm_probe") {
    1
} else {
    0
};

global_asm!(
    concat!(
        ".equ KERNEL_TRAP_DIAGNOSTICS, {kernel_trap_diagnostics}\n",
        ".equ GETPID_ASM_PROBE, {getpid_asm_probe}\n",
        include_str!("trap.S")
    ),
    kernel_trap_diagnostics = const cfg!(feature = "kernel_trap_diagnostics") as usize,
    getpid_asm_probe = const GETPID_ASM_PROBE_MODE,
);

/// RISC-V implementation of [`InterruptControl`](crate::hal::traits::InterruptControl).
pub struct RiscvInterruptControl;

/// RISC-V implementation of trap decoding and user-return operations.
pub struct RiscvTrapMachine;

/// RISC-V register-layout helpers for the common [`TrapContext`](crate::trap::TrapContext).
pub struct RiscvTrapContextAbi;

/// RISC-V Linux signal ABI implementation.
pub struct RiscvSignalAbi;

/// RISC-V legacy Linux syscall ABI implementation.
pub struct RiscvSyscallAbi;

/// RISC-V trap frame layout shared with `trap.S`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RiscvTrapContextFrame {
    /// General-purpose registers x0..x31.
    pub x: [usize; 32],
    /// Saved supervisor status register.
    pub sstatus: Sstatus,
    /// Saved exception PC.
    pub sepc: usize,
    /// Kernel hart id restored into `tp` on trap entry.
    pub kernel_hartid: usize,
    /// Kernel address-space token installed on trap entry.
    pub kernel_satp: usize,
    /// Kernel stack pointer used on trap entry.
    pub kernel_sp: usize,
    /// Common Rust trap handler entry.
    pub trap_handler: usize,
    /// Floating-point registers f0..f31.
    pub f: [u64; 32],
    /// Floating-point CSR.
    pub fcsr: usize,
}

/// Return whether an illegal instruction accesses scalar floating-point state
/// and can therefore fault solely because `sstatus.FS` is Off.
fn is_fp_state_instruction(instruction: u32) -> bool {
    if instruction & 0b11 != 0b11 {
        // RV64 compressed floating-point instructions are C.FLD/C.FSD and
        // their stack-pointer variants.  The encodings that share funct3 with
        // C.LD/C.SD on RV64 must not be treated as floating-point operations.
        let quadrant = instruction & 0b11;
        let funct3 = (instruction >> 13) & 0b111;
        return matches!(quadrant, 0b00 | 0b10) && matches!(funct3, 0b001 | 0b101);
    }

    let opcode = instruction & 0x7f;
    match opcode {
        // LOAD-FP / STORE-FP.  Widths 001..100 cover the standard half,
        // single, double and quad encodings; unsupported extensions will trap
        // again with FS enabled and follow the normal SIGILL path.
        0x07 | 0x27 => matches!((instruction >> 12) & 0b111, 0b001..=0b100),
        // FMADD, FMSUB, FNMSUB, FNMADD and OP-FP.
        0x43 | 0x47 | 0x4b | 0x4f | 0x53 => true,
        // CSR operations on fflags, frm or fcsr also require FS to be enabled.
        0x73 => {
            let funct3 = (instruction >> 12) & 0b111;
            let csr = (instruction >> 20) & 0xfff;
            matches!(funct3, 0b001..=0b011 | 0b101..=0b111) && matches!(csr, 0x001..=0x003)
        }
        _ => false,
    }
}

/// Lazily initialize a user's floating-point context after its first
/// floating-point-state instruction traps with `sstatus.FS=Off`.
///
/// The trap frame remains the canonical backing store in this first version.
/// Trap entry saves it only when hardware reports Dirty, while trap return
/// restores it only for tasks that have crossed this first-use point.
pub fn try_enable_user_fp(frame: &mut RiscvTrapContextFrame, instruction: u32) -> bool {
    if frame.sstatus.fs() != FS::Off || !is_fp_state_instruction(instruction) {
        return false;
    }
    frame.f.fill(0);
    frame.fcsr = 0;
    frame.sstatus.set_fs(FS::Initial);
    true
}

/// Complete GPR snapshot captured by `__trap_from_kernel` before entering Rust.
///
/// Slots use architectural register numbers (`x[1]` is `ra`, `x[2]` is the
/// interrupted stack pointer, and so on), which keeps the assembly and the
/// diagnostic formatter mechanically checkable against one another.
#[cfg(feature = "kernel_trap_diagnostics")]
#[repr(C)]
pub struct RiscvKernelTrapFrame {
    pub x: [usize; 32],
}

#[cfg(feature = "kernel_trap_diagnostics")]
const _: () = assert!(core::mem::size_of::<RiscvKernelTrapFrame>() == 256);

/// Emit a fault-time register dump for an unexpected kernel trap.
///
/// # Safety
///
/// `frame_ptr` must point to the 256-byte frame currently owned by
/// `__trap_from_kernel` on this hart's kernel stack.
#[cfg(feature = "kernel_trap_diagnostics")]
pub unsafe fn log_kernel_trap_frame(frame_ptr: *const RiscvKernelTrapFrame) {
    if frame_ptr.is_null() {
        log::error!("[kernel-trap][gpr] missing RISC-V kernel trap frame");
        return;
    }
    let frame = &*frame_ptr;
    let x = &frame.x;
    log::error!(
        "[kernel-trap][csr] hart={} sepc={:#018x} scause={:#018x} stval={:#018x} sstatus={:#018x} satp={:#018x}",
        crate::hal::hartid(),
        sepc::read(),
        scause::read().bits(),
        stval::read(),
        sstatus::read().bits(),
        satp::read().bits(),
    );
    log::error!(
        "[kernel-trap][gpr] ra={:#018x} sp={:#018x} gp={:#018x} tp={:#018x}",
        x[1],
        x[2],
        x[3],
        x[4],
    );
    log::error!(
        "[kernel-trap][arg] a0={:#018x} a1={:#018x} a2={:#018x} a3={:#018x} a4={:#018x} a5={:#018x} a6={:#018x} a7={:#018x}",
        x[10],
        x[11],
        x[12],
        x[13],
        x[14],
        x[15],
        x[16],
        x[17],
    );
    log::error!(
        "[kernel-trap][tmp] t0={:#018x} t1={:#018x} t2={:#018x} t3={:#018x} t4={:#018x} t5={:#018x} t6={:#018x}",
        x[5],
        x[6],
        x[7],
        x[28],
        x[29],
        x[30],
        x[31],
    );
    log::error!(
        "[kernel-trap][saved] s0={:#018x} s1={:#018x} s2={:#018x} s3={:#018x} s4={:#018x} s5={:#018x}",
        x[8],
        x[9],
        x[18],
        x[19],
        x[20],
        x[21],
    );
    log::error!(
        "[kernel-trap][saved] s6={:#018x} s7={:#018x} s8={:#018x} s9={:#018x} s10={:#018x} s11={:#018x}",
        x[22],
        x[23],
        x[24],
        x[25],
        x[26],
        x[27],
    );
}

/// RISC-V musl raw `rt_sigaction` syscall layout used by this kernel.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RiscvUserSigAction {
    pub handler: usize,
    pub sa_flags: usize,
    pub sa_mask: u64,
}

impl Pod for RiscvUserSigAction {}

impl SyscallAbi for RiscvSyscallAbi {
    fn decode_clone_args(args: [usize; 6]) -> CloneArgs {
        // Linux RISC-V selects CONFIG_CLONE_BACKWARDS:
        // clone(flags, stack, parent_tidptr, tls, child_tidptr).
        CloneArgs {
            flags: args[0],
            stack: args[1],
            parent_tid: args[2],
            tls: args[3],
            child_tid: args[4],
        }
    }
}

/// Floating-point state area embedded in riscv64 Linux `mcontext_t`.
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub struct RiscvFpState {
    pub fpregs: [u64; 32],
    pub fcsr: u32,
    pub reserved: [u32; 67],
}

impl Default for RiscvFpState {
    fn default() -> Self {
        Self {
            fpregs: [0; 32],
            fcsr: 0,
            reserved: [0; 67],
        }
    }
}

impl Pod for RiscvFpState {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RiscvMContext {
    /// Slot 0 stores the saved PC on riscv64 Linux.
    pub gregs: [usize; 32],
    pub fpstate: RiscvFpState,
}

impl Pod for RiscvMContext {}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RiscvUContext {
    pub uc_flags: usize,
    pub uc_link: usize,
    pub uc_stack: StackT,
    pub uc_sigmask: SigSetT,
    pub uc_mcontext: RiscvMContext,
}

impl Pod for RiscvUContext {}

/// 用户态 `rt_sigreturn` trampoline 机器码。
const USER_VDSO_CODE: [u8; 8] = [
    0x93, 0x08, 0xb0, 0x08, // addi a7, zero, 139
    0x73, 0x00, 0x00, 0x00, // ecall
];

impl InterruptControl for RiscvInterruptControl {
    unsafe fn enable_timer() {
        sie::set_stimer();
    }
    unsafe fn disable_timer() {
        sie::clear_stimer();
    }
    unsafe fn enable_external() {
        sie::set_sext();
    }
    unsafe fn disable_external() {
        sie::clear_sext();
    }
    unsafe fn enable_software() {
        sie::set_ssoft();
    }

    unsafe fn clear_software_pending() {
        asm!("csrc sip, {}", in(reg) 1usize << 1);
    }

    unsafe fn set_kernel_trap_entry() {
        extern "C" {
            fn __trap_from_kernel();
        }
        stvec::write(__trap_from_kernel as usize, TrapMode::Direct);
    }

    unsafe fn set_user_trap_entry() {
        stvec::write(TRAMPOLINE, TrapMode::Direct);
    }
}

impl TrapMachine for RiscvTrapMachine {
    fn read_trap_info() -> TrapInfo {
        let cause = match scause::read().cause() {
            Trap::Exception(Exception::UserEnvCall) => TrapCause::UserSyscall,
            Trap::Exception(Exception::StorePageFault) => TrapCause::StorePageFault,
            Trap::Exception(Exception::LoadPageFault) => TrapCause::LoadPageFault,
            Trap::Exception(Exception::InstructionPageFault) => TrapCause::InstructionPageFault,
            Trap::Exception(Exception::StoreFault) => TrapCause::StoreFault,
            Trap::Exception(Exception::InstructionFault) => TrapCause::InstructionFault,
            Trap::Exception(Exception::LoadFault) => TrapCause::LoadFault,
            Trap::Exception(Exception::IllegalInstruction) => TrapCause::IllegalInstruction,
            Trap::Interrupt(Interrupt::SupervisorTimer) => TrapCause::TimerInterrupt,
            Trap::Interrupt(Interrupt::SupervisorSoft) => TrapCause::SoftwareInterrupt,
            Trap::Interrupt(Interrupt::SupervisorExternal) => TrapCause::ExternalInterrupt,
            _ => TrapCause::Unknown,
        };
        TrapInfo {
            cause,
            fault_addr: stval::read(),
        }
    }

    unsafe fn return_to_user(trap_cx_user_va: usize, user_token: usize) -> ! {
        extern "C" {
            fn __alltraps();
            fn __restore();
        }
        let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;
        asm!(
            // "fence.i",
            "jr {restore_va}",
            restore_va = in(reg) restore_va,
            in("a0") trap_cx_user_va,
            in("a1") user_token,
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

impl SignalAbi for RiscvSignalAbi {
    type UserSigAction = RiscvUserSigAction;
    type UContext = RiscvUContext;

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
        let mut fpstate = RiscvFpState::default();
        trap_cx.copy_fp_state_to(&mut fpstate.fpregs, &mut fpstate.fcsr);
        Self::UContext {
            uc_flags: 0,
            uc_link: 0,
            uc_stack: StackT {
                ss_sp: 0,
                ss_flags: 0,
                ss_size: 0,
            },
            uc_sigmask: SigSetT::from_signal_bits(old_mask),
            uc_mcontext: RiscvMContext {
                gregs: trap_cx.export_signal_gprs(),
                fpstate,
            },
        }
    }

    fn signal_mask(ucontext: &Self::UContext) -> u64 {
        ucontext.uc_sigmask.low_bits()
    }

    fn restore_ucontext(ucontext: &Self::UContext, trap_cx: &mut TrapContext) {
        trap_cx.import_signal_gprs(&ucontext.uc_mcontext.gregs);
        trap_cx.restore_fp_state(
            &ucontext.uc_mcontext.fpstate.fpregs,
            ucontext.uc_mcontext.fpstate.fcsr,
        );
    }

    fn saved_pc(ucontext: &Self::UContext) -> usize {
        ucontext.uc_mcontext.gregs[0]
    }

    fn set_saved_pc(ucontext: &mut Self::UContext, pc: usize) {
        ucontext.uc_mcontext.gregs[0] = pc;
    }

    fn saved_arg0(ucontext: &Self::UContext) -> usize {
        let index = <RiscvTrapContextAbi as TrapContextAbi>::signal_gpr_arg0_index();
        ucontext.uc_mcontext.gregs[index]
    }

    fn set_saved_arg0(ucontext: &mut Self::UContext, value: usize) {
        let index = <RiscvTrapContextAbi as TrapContextAbi>::signal_gpr_arg0_index();
        ucontext.uc_mcontext.gregs[index] = value;
    }
}

impl TrapContextAbi for RiscvTrapContextAbi {
    type Frame = RiscvTrapContextFrame;

    fn new_user_frame(
        entry: usize,
        sp: usize,
        kernel_token: usize,
        kernel_sp: usize,
        trap_handler: usize,
    ) -> Self::Frame {
        let mut status = sstatus::read();
        status.set_spp(SPP::User);
        // Keep FP disabled until the task executes its first F/D instruction.
        // This lets the trampoline skip all FP loads/stores for integer-only
        // workloads while still preserving the existing per-task backing area.
        status.set_fs(FS::Off);
        let mut frame = RiscvTrapContextFrame {
            x: [0; 32],
            sstatus: status,
            sepc: entry,
            kernel_hartid: 0,
            kernel_satp: kernel_token,
            kernel_sp,
            trap_handler,
            f: [0; 32],
            fcsr: 0,
        };
        frame.x[2] = sp;
        frame
    }

    fn reg(frame: &Self::Frame, index: usize) -> usize {
        frame.x[index]
    }

    fn set_reg(frame: &mut Self::Frame, index: usize, value: usize) {
        #[cfg(any(feature = "getpid_asm_probe", feature = "getpid_asm_satp_probe"))]
        if index == 0 {
            frame.x[0] = value;
            return;
        }
        if index != 0 {
            frame.x[index] = value;
        }
    }

    fn user_pc(frame: &Self::Frame) -> usize {
        frame.sepc
    }

    fn set_user_pc(frame: &mut Self::Frame, pc: usize) {
        frame.sepc = pc;
    }

    fn user_sp(frame: &Self::Frame) -> usize {
        frame.x[2]
    }

    fn set_user_sp(frame: &mut Self::Frame, sp: usize) {
        frame.x[2] = sp;
    }

    fn ra(frame: &Self::Frame) -> usize {
        frame.x[1]
    }

    fn set_ra(frame: &mut Self::Frame, ra: usize) {
        frame.x[1] = ra;
    }

    fn tls(frame: &Self::Frame) -> usize {
        frame.x[4]
    }

    fn set_tls(frame: &mut Self::Frame, tls: usize) {
        frame.x[4] = tls;
    }

    fn syscall_nr(frame: &Self::Frame) -> usize {
        frame.x[17]
    }

    fn syscall_args(frame: &Self::Frame) -> [usize; 6] {
        [
            frame.x[10],
            frame.x[11],
            frame.x[12],
            frame.x[13],
            frame.x[14],
            frame.x[15],
        ]
    }

    fn syscall_ret(frame: &Self::Frame) -> usize {
        frame.x[10]
    }

    fn set_syscall_ret(frame: &mut Self::Frame, ret: usize) {
        frame.x[10] = ret;
    }

    fn set_user_arg(frame: &mut Self::Frame, index: usize, value: usize) {
        frame.x[10 + index] = value;
    }

    fn set_kernel_hartid(frame: &mut Self::Frame, hartid: usize) {
        frame.kernel_hartid = hartid;
    }

    fn set_kernel_sp(frame: &mut Self::Frame, kernel_sp: usize) {
        frame.kernel_sp = kernel_sp;
    }

    fn export_signal_gprs(frame: &Self::Frame) -> [usize; 32] {
        let mut exported = [0usize; 32];
        exported[0] = frame.sepc;
        exported[1..].copy_from_slice(&frame.x[1..]);
        exported
    }

    fn import_signal_gprs(frame: &mut Self::Frame, signal_gprs: &[usize; 32]) {
        frame.x[0] = 0;
        frame.x[1..].copy_from_slice(&signal_gprs[1..]);
        frame.sepc = signal_gprs[0];
    }

    fn signal_gpr_arg0_index() -> usize {
        10 // RISC-V: x10 = a0
    }

    fn copy_fp_state_to(frame: &Self::Frame, fpregs: &mut [u64; 32], fcsr: &mut u32) {
        fpregs.copy_from_slice(&frame.f);
        *fcsr = frame.fcsr as u32;
    }

    fn restore_fp_state(frame: &mut Self::Frame, fpregs: &[u64; 32], fcsr: u32) {
        frame.f.copy_from_slice(fpregs);
        frame.fcsr = fcsr as usize;
        // A signal frame explicitly supplying FP state makes that state valid,
        // even if the interrupted task had not previously used the FPU.
        frame.sstatus.set_fs(FS::Clean);
    }

    fn fault_dump_summary(frame: &Self::Frame) -> [NamedReg; 7] {
        [
            NamedReg {
                name: "ra",
                value: frame.x[1],
            },
            NamedReg {
                name: "sp",
                value: frame.x[2],
            },
            NamedReg {
                name: "gp",
                value: frame.x[3],
            },
            NamedReg {
                name: "tp",
                value: frame.x[4],
            },
            NamedReg {
                name: "a0",
                value: frame.x[10],
            },
            NamedReg {
                name: "a1",
                value: frame.x[11],
            },
            NamedReg {
                name: "a7",
                value: frame.x[17],
            },
        ]
    }

    fn fault_dump_detail(frame: &Self::Frame) -> [NamedReg; 19] {
        [
            NamedReg {
                name: "t0",
                value: frame.x[5],
            },
            NamedReg {
                name: "t1",
                value: frame.x[6],
            },
            NamedReg {
                name: "t2",
                value: frame.x[7],
            },
            NamedReg {
                name: "s0",
                value: frame.x[8],
            },
            NamedReg {
                name: "s1",
                value: frame.x[9],
            },
            NamedReg {
                name: "s2",
                value: frame.x[18],
            },
            NamedReg {
                name: "s3",
                value: frame.x[19],
            },
            NamedReg {
                name: "s4",
                value: frame.x[20],
            },
            NamedReg {
                name: "s5",
                value: frame.x[21],
            },
            NamedReg {
                name: "s6",
                value: frame.x[22],
            },
            NamedReg {
                name: "s7",
                value: frame.x[23],
            },
            NamedReg {
                name: "s8",
                value: frame.x[24],
            },
            NamedReg {
                name: "s9",
                value: frame.x[25],
            },
            NamedReg {
                name: "s10",
                value: frame.x[26],
            },
            NamedReg {
                name: "s11",
                value: frame.x[27],
            },
            NamedReg {
                name: "t3",
                value: frame.x[28],
            },
            NamedReg {
                name: "t4",
                value: frame.x[29],
            },
            NamedReg {
                name: "t5",
                value: frame.x[30],
            },
            NamedReg {
                name: "t6",
                value: frame.x[31],
            },
        ]
    }
}
