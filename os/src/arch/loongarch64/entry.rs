//! LoongArch64 kernel entry assembly.

use core::arch::global_asm;

global_asm!(include_str!("entry.S"));

/// Raw register arguments supplied by the firmware to the bootstrap hart.
#[derive(Clone, Copy)]
pub struct FirmwareBootArgs {
    pub arg0: usize,
    pub arg1: usize,
    pub arg2: usize,
    pub arg3: usize,
}

/// Return the unmodified `$a0..$a3` values observed at the ELF entry point.
pub fn firmware_boot_args() -> FirmwareBootArgs {
    extern "C" {
        static loongarch_boot_arg0: usize;
        static loongarch_boot_arg1: usize;
        static loongarch_boot_arg2: usize;
        static loongarch_boot_arg3: usize;
    }

    unsafe {
        FirmwareBootArgs {
            arg0: core::ptr::read_volatile(core::ptr::addr_of!(loongarch_boot_arg0)),
            arg1: core::ptr::read_volatile(core::ptr::addr_of!(loongarch_boot_arg1)),
            arg2: core::ptr::read_volatile(core::ptr::addr_of!(loongarch_boot_arg2)),
            arg3: core::ptr::read_volatile(core::ptr::addr_of!(loongarch_boot_arg3)),
        }
    }
}

/// Compiler-relevant CPU and firmware state captured before Rust executes.
#[derive(Clone, Copy)]
pub struct BootExecutionState {
    pub cpucfg1: usize,
    pub misc_before: usize,
    pub misc_after: usize,
}

/// Return the bootstrap hart's unaligned-access state at kernel entry.
pub fn boot_execution_state() -> BootExecutionState {
    extern "C" {
        static loongarch_boot_cpucfg1: usize;
        static loongarch_boot_misc_before: usize;
        static loongarch_boot_misc_after: usize;
    }

    unsafe {
        BootExecutionState {
            cpucfg1: core::ptr::read_volatile(core::ptr::addr_of!(loongarch_boot_cpucfg1)),
            misc_before: core::ptr::read_volatile(core::ptr::addr_of!(loongarch_boot_misc_before)),
            misc_after: core::ptr::read_volatile(core::ptr::addr_of!(loongarch_boot_misc_after)),
        }
    }
}
