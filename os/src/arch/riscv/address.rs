//! SV39 address-space constants for RISC-V.

/// Physical address width under Sv39.
pub const PA_WIDTH_SV39: usize = 56;
/// Virtual address width under Sv39.
pub const VA_WIDTH_SV39: usize = 39;
/// Physical page number width under Sv39.
pub const PPN_WIDTH_SV39: usize = PA_WIDTH_SV39 - 12; // PAGE_SIZE_BITS = 12
