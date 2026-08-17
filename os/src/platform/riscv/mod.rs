//! RISC-V platform implementations.
//!
//! This layer binds one concrete machine/board model onto the generic RISC-V
//! architecture support and reusable device drivers.

pub mod qemu_virt;

#[cfg(not(any(feature = "platform-qemu-virt", feature = "platform-visionfive2")))]
compile_error!("RISC-V requires either platform-qemu-virt or platform-visionfive2");
