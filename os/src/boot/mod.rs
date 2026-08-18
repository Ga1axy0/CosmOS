//! Linux-style early boot services.
//!
//! This module deliberately contains only information that must exist before
//! the page allocator and drivers are available.  Firmware decoding lives in
//! `crate::of`; device drivers must not add binding-specific state here.

pub mod context;
pub mod init;
pub mod memblock;
pub mod source;
#[cfg(target_arch = "loongarch64")]
pub mod efi;
#[cfg(target_arch = "loongarch64")]
pub mod uboot;
