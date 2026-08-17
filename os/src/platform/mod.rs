//! Platform-specific machine composition.
//!
//! `arch` describes ISA and privilege-architecture behavior.
//! `drivers` describe reusable device-IP drivers.
//! `platform` binds one concrete machine model to those two layers: MMIO
//! layout, interrupt routing, device probing, poweroff, SMP bring-up, and
//! early console policy all belong here.
#![allow(missing_docs)]

#[cfg(target_arch = "riscv64")]
pub mod riscv;

#[cfg(target_arch = "loongarch64")]
pub mod loongarch;

#[cfg(target_arch = "riscv64")]
pub use riscv::qemu_virt::rtc;

#[cfg(target_arch = "loongarch64")]
pub use loongarch::rtc;

#[cfg(target_arch = "riscv64")]
pub use riscv::qemu_virt::{
    console_getchar, console_putchar, console_rx_irq_ready, direct_map_phys_to_virt,
    direct_map_virt_to_phys, early_console_write, handle_external_irq, heap_debug_enabled,
    init_external_irq, init_external_irq_hart, kernel_heap_virtual_window_supported, machine_name,
    mmio_phys_to_virt, platform_name, probe_platform_devices, rtc_is_supported, shutdown,
    start_secondary_harts, translate_direct_mapped_kernel_va, use_early_console, BlockDeviceImpl,
    CharDeviceImpl, QEMUExit, SbiPlatform as PlatformImpl, INTERP_BASE, KERNEL_ADDR_OFFSET,
    KERNEL_HEAP_BASE, QEMU_EXIT_HANDLE, TRAMPOLINE, USER_MMAP_BASE, USER_STACK_BASE,
};

#[cfg(target_arch = "loongarch64")]
pub use loongarch::{
    boot_fdt_ptr, clear_ipi_vector, console_getchar, console_putchar, console_rx_irq_ready,
    continue_full_boot, continue_storage_boot, direct_map_phys_to_virt, direct_map_virt_to_phys,
    early_console_write, early_runtime_diagnostics, halt_early_bringup, handle_external_irq,
    heap_debug_enabled, init_external_irq, init_external_irq_hart, init_ipi_hart,
    kernel_heap_virtual_window_supported, machine_name, mmio_phys_to_virt, platform_name,
    probe_platform_devices, rtc_is_supported, shutdown, start_secondary_harts,
    translate_direct_mapped_kernel_va, use_early_console, BlockDeviceImpl, CharDeviceImpl,
    LoongArchPlatform as PlatformImpl, QEMUExit, INTERP_BASE, IO_ADDR_OFFSET, KERNEL_ADDR_OFFSET,
    KERNEL_HEAP_BASE, QEMU_EXIT_HANDLE, TRAMPOLINE, USER_MMAP_BASE, USER_STACK_BASE, VIRT_UART,
};

#[cfg(target_arch = "riscv64")]
pub const fn boot_fdt_ptr(raw: usize) -> usize {
    if raw < KERNEL_ADDR_OFFSET {
        KERNEL_ADDR_OFFSET.wrapping_add(raw)
    } else {
        raw
    }
}

/// Normalize the two registers supplied to the RISC-V kernel entry.
///
/// `bootm` follows the RISC-V OS ABI and supplies `(hart_id, fdt)`.  The
/// VisionFive 2 U-Boot 2021.10 `bootelf` command instead calls an ELF like a
/// standalone C application and supplies `(argc, argv)`.  Its boot hart is 1,
/// so the RAM-only bootelf workflow deliberately passes exactly one trailing
/// `fdt=<hex>` argument: argc remains 1 (the real hart ID), while this helper
/// replaces argv with the parsed FDT address before normal initialization.
#[cfg(target_arch = "riscv64")]
pub fn normalize_firmware_boot_args(arg0: usize, arg1: usize) -> (usize, usize) {
    #[cfg(feature = "platform-visionfive2")]
    {
        if arg1 != 0 && !visionfive2_fdt_magic_at(arg1) && arg0 == 1 {
            if let Some(fdt) = visionfive2_bootelf_fdt_arg(arg1) {
                return (1, fdt);
            }
        }
    }
    (arg0, arg1)
}

/// Normalize the Loongson BSP U-Boot `bootm` FDT calling convention.
///
/// The entry assembly replaces `a0` with the hardware core ID before entering
/// Rust, so the returned hart ID is already correct.  The Loongson BSP's
/// legacy Linux ABI passes `(argc, argv, boot_params, fdt)` in the original
/// `a0..a3`; recover its FDT from the preserved fourth firmware argument.
/// Its explicit-FDT ABI and QEMU already leave the FDT in the Rust `arg1`.
#[cfg(target_arch = "loongarch64")]
pub fn normalize_firmware_boot_args(hart_id: usize, arg1: usize) -> (usize, usize) {
    #[cfg(feature = "platform-ls2k1000-nebula")]
    {
        let firmware = crate::arch::loongarch64::firmware_boot_args();
        if (1..=256).contains(&firmware.arg0) && firmware.arg3 != 0 {
            return (hart_id, firmware.arg3);
        }
    }
    (hart_id, arg1)
}

#[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
fn visionfive2_early_dram_ptr(raw: usize) -> Option<usize> {
    const DRAM_START: usize = 0x4000_0000;
    const DRAM_END: usize = 0x1_4000_0000;
    (DRAM_START..DRAM_END)
        .contains(&raw)
        .then(|| direct_map_phys_to_virt(raw))
}

#[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
fn visionfive2_fdt_magic_at(raw: usize) -> bool {
    const FDT_MAGIC: u32 = 0xd00d_feed;
    let Some(ptr) = visionfive2_early_dram_ptr(raw) else {
        return false;
    };
    u32::from_be(unsafe { core::ptr::read_volatile(ptr as *const u32) }) == FDT_MAGIC
}

#[cfg(all(target_arch = "riscv64", feature = "platform-visionfive2"))]
fn visionfive2_bootelf_fdt_arg(argv_raw: usize) -> Option<usize> {
    const PREFIX: &[u8] = b"fdt=";
    let argv = visionfive2_early_dram_ptr(argv_raw)?;
    let argument_raw = unsafe { core::ptr::read_volatile(argv as *const usize) };
    let argument = visionfive2_early_dram_ptr(argument_raw)?;

    for (offset, expected) in PREFIX.iter().copied().enumerate() {
        if unsafe { core::ptr::read_volatile((argument + offset) as *const u8) } != expected {
            return None;
        }
    }

    let mut cursor = argument + PREFIX.len();
    if unsafe { core::ptr::read_volatile(cursor as *const u8) } == b'0'
        && unsafe { core::ptr::read_volatile((cursor + 1) as *const u8) } == b'x'
    {
        cursor += 2;
    }

    let mut value = 0usize;
    let mut digits = 0usize;
    while digits < usize::BITS as usize / 4 {
        let byte = unsafe { core::ptr::read_volatile((cursor + digits) as *const u8) };
        if byte == 0 {
            break;
        }
        let digit = match byte {
            b'0'..=b'9' => (byte - b'0') as usize,
            b'a'..=b'f' => (byte - b'a' + 10) as usize,
            b'A'..=b'F' => (byte - b'A' + 10) as usize,
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(digit)?;
        digits += 1;
    }

    (digits != 0 && visionfive2_fdt_magic_at(value)).then_some(value)
}

#[cfg(target_arch = "riscv64")]
pub const fn continue_full_boot() -> bool {
    true
}

#[cfg(target_arch = "riscv64")]
pub const fn continue_storage_boot() -> bool {
    true
}

#[cfg(target_arch = "riscv64")]
pub fn halt_early_bringup() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(target_arch = "riscv64")]
pub fn early_runtime_diagnostics() {
    #[cfg(feature = "platform-visionfive2")]
    early_console_write("[vf2] entered Rust high-half bootstrap\r\n");
}

/// Initialize platform-owned devices and interrupt routing.
pub fn init() {
    rtc::init();
    init_external_irq();
    probe_platform_devices();
}

/// Initialize per-hart platform-owned local interrupt/IPI state.
pub fn init_local_hart() {
    #[cfg(target_arch = "loongarch64")]
    init_ipi_hart();
}

/// Clear the current hart's pending platform IPI state when applicable.
pub fn clear_ipi() {
    #[cfg(target_arch = "loongarch64")]
    clear_ipi_vector();
}
