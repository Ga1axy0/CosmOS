//! Runtime-selected LoongArch platform support.
//!
//! Both build feature names select this same implementation. Hardware
//! resources and capabilities are selected from the firmware FDT at runtime,
//! so changing between QEMU `virt` and an LS2K1000 board does not select a
//! hard-coded device backend. The feature may still select the firmware-facing
//! ELF address representation required by the corresponding boot loader.

#[cfg(feature = "platform-ls2k1000-nebula")]
mod ls2k1000_nebula;
mod qemu_virt;

#[cfg(not(any(feature = "platform-qemu-virt", feature = "platform-ls2k1000-nebula")))]
compile_error!("select a LoongArch platform backend");

pub use qemu_virt::*;
