//! Bootstrap firmware discovery and boot-context publication.

use crate::boot::context::{self, BootContext};
use crate::boot::source::{BootSource, FdtSource};
pub use crate::boot::context::BootError;

/// Build and publish the global boot context from a firmware FDT pointer.
pub fn early_init(fdt_ptr: usize) -> Result<(), BootError> {
    let mut context = BootContext::empty();
    let direct = FdtSource::from_ptr(crate::platform::boot_fdt_ptr(fdt_ptr)).map(BootSource::Fdt);
    let found_direct = direct.is_some_and(|source| crate::of::scan::scan_fdt(source.fdt(), &mut context));

    #[cfg(target_arch = "loongarch64")]
    let found_fdt = found_direct
        || crate::boot::uboot::fdt_source().is_some_and(|source| crate::of::scan::scan_fdt(source, &mut context))
        || crate::boot::efi::fdt_source().is_some_and(|source| crate::of::scan::scan_fdt(source, &mut context));
    #[cfg(not(target_arch = "loongarch64"))]
    let found_fdt = found_direct;

    if !found_fdt { return Err(BootError::MissingFdt); }
    if context.memblock().memory().is_empty() { return Err(BootError::MissingMemory); }
    if context.hart_count() == 0 { return Err(BootError::MissingCpu); }
    if context.timer_frequency() == 0 { return Err(BootError::MissingTimebase); }
    if context.devices().uart().is_none() { return Err(BootError::MissingConsole); }
    context::publish(context);
    Ok(())
}

/// Compatibility name for fallible early initialization.
pub fn try_init(fdt_ptr: usize) -> Result<(), BootError> {
    early_init(fdt_ptr)
}

/// Initialize firmware discovery or halt with a precise early-boot error.
pub fn init(fdt_ptr: usize) {
    if let Err(error) = early_init(fdt_ptr) {
        crate::platform::early_console_write("[boot] firmware discovery failed\r\n");
        panic!("firmware discovery failed: {error:?}");
    }
}
