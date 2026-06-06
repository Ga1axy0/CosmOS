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

/// Read and write the current hart id.
pub trait HartId {
    /// Return current hart id from arch register.
    fn current() -> usize;
    /// Write hart id to arch register at boot.
    unsafe fn init(id: usize);
}

/// Arch-level paging operations (SV39 on RISC-V).
pub trait PagingArch {
    /// Page-table entry type.
    type Entry: Copy;
    /// Value written to satp MODE field.
    const SATP_MODE: usize;
    /// Number of page-table levels.
    const LEVELS: usize;
    /// Activate the given root PPN and flush TLB.
    unsafe fn activate(root_ppn: usize);
    /// Read current satp token.
    unsafe fn current_token() -> usize;
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
