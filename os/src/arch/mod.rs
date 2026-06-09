//! Architecture-specific CPU and privilege-architecture support.
//!
//! This layer owns ISA-defined mechanisms such as traps, CSR access, paging
//! formats, context-switch ABI, and hart-local interrupt control. It does not
//! know which board or machine model wires devices onto that CPU.

#[cfg(target_arch = "riscv64")]
pub mod riscv;

#[cfg(target_arch = "loongarch64")]
pub mod loongarch64;
