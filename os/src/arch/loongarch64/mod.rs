//! LoongArch64 arch implementation of HAL traits.
#![allow(missing_docs)]

mod entry;
pub mod hart;
pub mod paging;
mod switch;
pub mod trap;
pub(crate) mod unaligned;

pub use entry::{boot_execution_state, firmware_boot_args, BootExecutionState, FirmwareBootArgs};
pub use hart::{read_time, set_timer_deadline, LoongArchHartId};
pub use paging::LoongArchPaging;
pub use trap::{
    LoongArchInterruptControl, LoongArchSignalAbi, LoongArchSyscallAbi, LoongArchTrapContextAbi,
    LoongArchTrapMachine,
};
