//! LoongArch64 kernel entry assembly.

use core::arch::global_asm;

global_asm!(include_str!("entry.S"));
