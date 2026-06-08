//! HAL trait definitions — pure interfaces, no implementations.

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
}

/// Opaque address-space activation token used by the current architecture.
pub type AddressSpaceToken = usize;

/// Arch-level paging operations.
pub trait PagingArch {
    /// Page-table entry type.
    type Entry: Copy;
    /// Architecture-specific mode bits embedded in the root token.
    const ROOT_TOKEN_MODE: usize;
    /// Number of page-table levels.
    const LEVELS: usize;
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
