//! RISC-V task-switch assembly entrypoints.

use core::arch::global_asm;

global_asm!(include_str!("switch.S"));
