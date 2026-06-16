//! LoongArch64 arch implementation of HAL traits.
#![allow(missing_docs)]

pub mod hart;
mod entry;
pub mod paging;
mod switch;
pub mod trap;

pub use hart::{read_time, set_timer_deadline, LoongArchHartId};
pub use paging::LoongArchPaging;
pub use trap::{
    LoongArchInterruptControl, LoongArchSignalAbi, LoongArchSyscallAbi, LoongArchTrapContextAbi,
    LoongArchTrapMachine,
};
