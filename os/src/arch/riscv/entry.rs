//! RISC-V kernel entry assembly.

use core::arch::global_asm;

global_asm!(include_str!("entry.asm"));
