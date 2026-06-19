//! HAL trait definitions — pure interfaces, no implementations.

bitflags! {
    /// Architecture-neutral page-table entry semantics.
    pub struct PTEFlags: u16 {
        /// Entry is present/valid.
        const V = 1 << 0;
        /// Entry permits reads.
        const R = 1 << 1;
        /// Entry permits writes.
        const W = 1 << 2;
        /// Entry permits instruction fetches.
        const X = 1 << 3;
        /// Entry is user-accessible.
        const U = 1 << 4;
        /// Entry is global across address spaces.
        const G = 1 << 5;
        /// Entry has been accessed.
        const A = 1 << 6;
        /// Entry has been dirtied by writes.
        const D = 1 << 7;
    }
}

/// Per-hart interrupt control (enable/disable, trap entry setup).
pub trait InterruptControl {
    /// Enable supervisor timer interrupt.
    unsafe fn enable_timer();
    /// Disable supervisor timer interrupt.
    unsafe fn disable_timer();
    /// Enable supervisor external interrupt.
    unsafe fn enable_external();
    /// Disable supervisor external interrupt.
    unsafe fn disable_external();
    /// Enable supervisor software interrupt.
    unsafe fn enable_software();
    /// Clear pending supervisor software interrupt.
    unsafe fn clear_software_pending();
    /// Set trap entry for kernel-mode traps.
    unsafe fn set_kernel_trap_entry();
    /// Set trap entry for user-mode traps.
    unsafe fn set_user_trap_entry();
}

/// Architecture-normalized trap causes observed by common kernel logic.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum TrapCause {
    UserSyscall,
    StorePageFault,
    LoadPageFault,
    InstructionPageFault,
    StoreFault,
    InstructionFault,
    LoadFault,
    /// Data-memory address error whose direction (load/store) is not encoded.
    DataAddressFault,
    IllegalInstruction,
    TimerInterrupt,
    SoftwareInterrupt,
    ExternalInterrupt,
    Unknown,
}

/// Trap metadata passed from arch-specific code into common trap handling.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct TrapInfo {
    /// Decoded trap cause.
    pub cause: TrapCause,
    /// Fault address or trap-value register contents when applicable.
    pub fault_addr: usize,
}

/// One architecture-labeled general-purpose register used in fault dumps.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct NamedReg {
    pub name: &'static str,
    pub value: usize,
}

/// Architecture-normalized arguments for the legacy Linux `clone` syscall.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CloneArgs {
    pub flags: usize,
    pub stack: usize,
    pub parent_tid: usize,
    pub tls: usize,
    pub child_tid: usize,
}

/// Architecture-specific syscall ABI details that are visible above traps.
pub trait SyscallAbi {
    /// Decode raw syscall argument registers into the common legacy `clone` layout.
    fn decode_clone_args(args: [usize; 6]) -> CloneArgs;
}

/// Arch-specific trap/syscall machine operations used by common code.
pub trait TrapMachine {
    /// Read the current trap cause and associated fault address.
    fn read_trap_info() -> TrapInfo;
    /// Return to user mode using the given trap-context VA and address-space token.
    unsafe fn return_to_user(trap_cx_user_va: usize, user_token: usize) -> !;
    /// Size in bytes of the userspace syscall instruction.
    fn syscall_instruction_len() -> usize;
    /// Machine-code trampoline used for `rt_sigreturn`.
    fn rt_sigreturn_trampoline() -> &'static [u8];
}

/// Read and write the current hart id.
pub trait HartId {
    /// Return current hart id from arch register.
    fn current() -> usize;
    /// Write hart id to arch register at boot.
    unsafe fn init(id: usize);
    /// Enable the local floating-point unit for subsequent kernel/user execution.
    unsafe fn enable_fp();
    /// Return whether local interrupts are currently enabled on this hart.
    fn irqs_enabled() -> bool;
    /// Disable local interrupts on this hart.
    unsafe fn disable_irqs();
    /// Enable local interrupts on this hart.
    unsafe fn enable_irqs();
    /// Wait for the next interrupt/event while keeping the current interrupt state.
    unsafe fn wait_for_interrupt();
    /// Enter the architecture-specific idle wait sequence used by the scheduler.
    unsafe fn enable_irqs_and_wait() {
        Self::enable_irqs();
        Self::wait_for_interrupt();
        Self::disable_irqs();
    }
}

/// Architecture-specific user trap-context ABI helpers.
pub trait TrapContextAbi {
    /// Architecture-owned trap-frame layout stored at the front of `TrapContext`.
    type Frame: Copy;

    /// Construct a trap frame prepared for first return to user mode.
    fn new_user_frame(
        entry: usize,
        sp: usize,
        kernel_token: AddressSpaceToken,
        kernel_sp: usize,
        trap_handler: usize,
    ) -> Self::Frame;
    /// Return the raw general-purpose register value at `index`.
    fn reg(frame: &Self::Frame, index: usize) -> usize;
    /// Update the raw general-purpose register value at `index`.
    fn set_reg(frame: &mut Self::Frame, index: usize, value: usize);
    /// Return the saved user PC from the trap context.
    fn user_pc(frame: &Self::Frame) -> usize;
    /// Set the saved user PC in the trap context.
    fn set_user_pc(frame: &mut Self::Frame, pc: usize);
    /// Return the saved user SP from the trap context.
    fn user_sp(frame: &Self::Frame) -> usize;
    /// Set the saved user SP in the trap context.
    fn set_user_sp(frame: &mut Self::Frame, sp: usize);
    /// Return the saved return-address register.
    fn ra(frame: &Self::Frame) -> usize;
    /// Set the saved return-address register.
    fn set_ra(frame: &mut Self::Frame, ra: usize);
    /// Return the saved TLS/thread-pointer register.
    fn tls(frame: &Self::Frame) -> usize;
    /// Set the saved TLS/thread-pointer register.
    fn set_tls(frame: &mut Self::Frame, tls: usize);
    /// Return the saved syscall number.
    fn syscall_nr(frame: &Self::Frame) -> usize;
    /// Return the saved syscall arguments.
    fn syscall_args(frame: &Self::Frame) -> [usize; 6];
    /// Return the saved syscall return value.
    fn syscall_ret(frame: &Self::Frame) -> usize;
    /// Set the saved syscall return value.
    fn set_syscall_ret(frame: &mut Self::Frame, ret: usize);
    /// Set one syscall/user argument register.
    fn set_user_arg(frame: &mut Self::Frame, index: usize, value: usize);
    /// Set the kernel hart id restored by the trap trampoline.
    fn set_kernel_hartid(frame: &mut Self::Frame, hartid: usize);
    /// Set the kernel stack pointer restored on the next trap entry.
    fn set_kernel_sp(frame: &mut Self::Frame, kernel_sp: usize);
    /// Export the Linux-compatible signal GPR layout.
    fn export_signal_gprs(frame: &Self::Frame) -> [usize; 32];
    /// Import the Linux-compatible signal GPR layout back into the trap context.
    fn import_signal_gprs(frame: &mut Self::Frame, signal_gprs: &[usize; 32]);
    /// Return the index of the a0 register within the 32-entry signal GPR array
    fn signal_gpr_arg0_index() -> usize;
    /// Copy floating-point state into an external signal frame.
    fn copy_fp_state_to(frame: &Self::Frame, fpregs: &mut [u64; 32], fcsr: &mut u32);
    /// Restore floating-point state from an external signal frame.
    fn restore_fp_state(frame: &mut Self::Frame, fpregs: &[u64; 32], fcsr: u32);
    /// Export a compact architecture-labeled summary used by common fault logs.
    fn fault_dump_summary(frame: &Self::Frame) -> [NamedReg; 7];
    /// Export a wider architecture-labeled register snapshot used by fault logs.
    fn fault_dump_detail(frame: &Self::Frame) -> [NamedReg; 19];
}

/// Opaque address-space activation token used by the current architecture.
pub type AddressSpaceToken = usize;

/// Arch-level paging operations.
pub trait PagingArch {
    /// Page-table entry type.
    type Entry: Copy;
    /// Physical address width in bits.
    const PA_BITS: usize;
    /// Virtual address width in bits.
    const VA_BITS: usize;
    /// Physical page-number width in bits.
    const PPN_BITS: usize;
    /// Architecture-specific mode bits embedded in the root token.
    const ROOT_TOKEN_MODE: usize;
    /// Number of page-table levels.
    const LEVELS: usize;
    /// Bits consumed per page-table level.
    const INDEX_BITS: usize;
    /// Build an architecture token from a root page-table physical page number.
    fn make_token(root_ppn: usize) -> AddressSpaceToken;
    /// Extract the root page-table physical page number from an architecture token.
    fn root_ppn(token: AddressSpaceToken) -> usize;
    /// Activate the given address-space token and flush the local TLB.
    unsafe fn activate_token(token: AddressSpaceToken);
    /// Read current address-space token.
    unsafe fn current_token() -> AddressSpaceToken;
    /// Flush entire TLB.
    unsafe fn flush_tlb();
    /// Encode one leaf/intermediate PTE for this architecture.
    fn make_pte(ppn: usize, flags: PTEFlags) -> usize;
    /// Encode a non-leaf directory entry pointing to the next page-table level.
    /// Architectures where leaf and directory entries have different encodings
    /// (e.g. LoongArch, which must NOT set GNR/GNX in directory entries) should
    /// override this. Default: delegates to make_pte with V only.
    fn make_dir_entry(ppn: usize) -> usize {
        Self::make_pte(ppn, PTEFlags::V)
    }
    /// Extract the pointed-to physical page number from one raw PTE.
    fn pte_ppn(entry_bits: usize) -> usize;
    /// Extract the semantic flags from one raw PTE.
    fn pte_flags(entry_bits: usize) -> PTEFlags;
    /// Return whether one raw PTE/directory entry should be treated as present.
    fn pte_is_valid(entry_bits: usize) -> bool {
        (Self::pte_flags(entry_bits) & PTEFlags::V) != PTEFlags::empty()
    }
    /// Normalize leaf PTE flags for the current architecture.
    fn normalize_leaf_flags(flags: PTEFlags) -> PTEFlags {
        flags
    }
    /// Convert an arbitrary raw virtual address input into the stored form.
    fn normalize_virt_addr_input(bits: usize) -> usize {
        bits & ((1usize << Self::VA_BITS) - 1)
    }
    /// Convert an arbitrary raw virtual address input into a virtual page number.
    fn virt_page_num_from_addr(bits: usize) -> usize {
        Self::normalize_virt_addr_input(bits) >> 12
    }
    /// Return the default trap-context page permissions for this architecture.
    fn trap_context_flags() -> PTEFlags {
        PTEFlags::R | PTEFlags::W
    }
    /// Return the exclusive end of the canonical low-half user address range.
    fn user_space_end() -> usize {
        1usize << (Self::VA_BITS - 1)
    }
    /// Canonicalize a virtual address value according to the current architecture.
    fn canonicalize_vaddr(bits: usize) -> usize {
        if bits >= (1usize << (Self::VA_BITS - 1)) {
            bits | (!((1usize << Self::VA_BITS) - 1))
        } else {
            bits
        }
    }
    /// Return the page-table index at `level` for the given virtual page number.
    ///
    /// `level=0` refers to the root-most level and `level=LEVELS-1` to the leaf level.
    fn vpn_index(vpn: usize, level: usize) -> usize;
}

/// Platform timer: read monotonic time, program next interrupt.
pub trait Timer {
    /// Read raw tick counter.
    fn read_time() -> usize;
    /// Program next timer interrupt deadline (raw ticks).
    fn set_next(deadline: usize);
    /// Clock frequency in Hz.
    fn clock_freq() -> usize;
}

/// Hart lifecycle control (SMP startup, IPI).
pub trait HartCtrl {
    /// Start a hart at the given address with an opaque argument.
    fn start_hart(hart_id: usize, start_addr: usize, opaque: usize) -> Result<(), ()>;
    /// Send IPI to harts described by the mask.
    fn send_ipi(hart_mask: usize);
}
