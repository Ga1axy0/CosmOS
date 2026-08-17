//! QEMU `virt` platform for RISC-V.

mod board;
pub mod rtc;
pub mod sbi;

pub use board::{
    BlockDeviceImpl, CharDeviceImpl, QEMUExit, INTERP_BASE, QEMU_EXIT_HANDLE, USER_MMAP_BASE,
    USER_STACK_BASE,
};
pub use sbi::SbiPlatform;

/// Sv39 high-half base.  Normal RAM is directly mapped at
/// `KERNEL_ADDR_OFFSET + physical_address`.
pub const KERNEL_ADDR_OFFSET: usize = 0xffff_ffc0_0000_0000;
const PHYSICAL_RAM_BASE: usize = 0x8000_0000;
/// Dedicated high-half MMIO aperture.  Keeping MMIO out of the low half lets
/// every process reserve all low root entries for its private user mappings.
pub const KERNEL_MMIO_OFFSET: usize = 0xffff_ffe0_0000_0000;
const KERNEL_MMIO_SIZE: usize = 0x4000_0000;
pub const KERNEL_HEAP_BASE: usize = KERNEL_ADDR_OFFSET;
pub const TRAMPOLINE: usize = usize::MAX - 0x1000 + 1;

/// Initialize platform external interrupt routing on the bootstrap hart.
pub fn init_external_irq() {
    crate::drivers::plic::init();
}

/// Initialize per-hart external interrupt state.
pub fn init_external_irq_hart(hart_id: usize) {
    crate::drivers::plic::init_hart(hart_id);
}

/// Dispatch one platform external interrupt.
pub fn handle_external_irq() {
    crate::drivers::plic::handle_supervisor_external();
}

/// Whether the console RX interrupt path is ready for blocking reads.
pub fn console_rx_irq_ready() -> bool {
    // The current JH7110 UART/PLIC path does not yet deliver RX promptly
    // enough for terminal-generated signals while userspace is sleeping.
    // Keep the scheduler and TTY on their cooperative polling path until the
    // board-specific interrupt routing is fully validated.
    !cfg!(feature = "platform-visionfive2")
}

/// Probe platform-specific devices after generic driver init.
pub fn probe_platform_devices() {
    crate::drivers::block::probe_block_devices();
    crate::drivers::net::probe_net_devices();
}

/// RISC-V always uses the normal UART path once the console layer is up.
pub fn use_early_console() -> bool {
    cfg!(feature = "platform-visionfive2")
}

/// Write one string through the earliest available console path.
pub fn early_console_write(s: &str) {
    #[cfg(feature = "platform-visionfive2")]
    {
        for byte in s.bytes() {
            visionfive2_uart_putchar(byte);
        }
    }
    #[cfg(not(feature = "platform-visionfive2"))]
    for byte in s.bytes() {
        sbi::console_putchar(byte as usize);
    }
}

#[cfg(feature = "platform-visionfive2")]
fn visionfive2_uart_putchar(byte: u8) {
    const UART0_PA: usize = 0x1000_0000;
    const REG_SHIFT: usize = 2;
    const THR: usize = 0;
    const LSR: usize = 5;
    const THR_EMPTY: u32 = 1 << 5;
    let base = KERNEL_MMIO_OFFSET + UART0_PA;
    unsafe {
        while core::ptr::read_volatile((base + (LSR << REG_SHIFT)) as *const u32) & THR_EMPTY == 0 {
            core::hint::spin_loop();
        }
        core::ptr::write_volatile((base + (THR << REG_SHIFT)) as *mut u32, byte as u32);
    }
}

/// Write one character to the platform console.
pub fn console_putchar(c: usize) {
    #[cfg(feature = "platform-visionfive2")]
    {
        visionfive2_uart_putchar(c as u8);
    }
    #[cfg(not(feature = "platform-visionfive2"))]
    sbi::console_putchar(c);
}

/// Read one character from the platform console.
pub fn console_getchar() -> usize {
    sbi::console_getchar()
}

/// Power off the virtual machine.
pub fn shutdown() -> ! {
    sbi::shutdown()
}

/// Return the uname-style machine string.
pub fn machine_name() -> &'static str {
    "riscv64"
}

/// Return the platform name for display purposes.
pub fn platform_name() -> &'static str {
    #[cfg(feature = "platform-visionfive2")]
    {
        "StarFive VisionFive 2"
    }
    #[cfg(not(feature = "platform-visionfive2"))]
    {
        "qemu virt"
    }
}

/// Discover stopped harts via SBI HSM and start them on QEMU `virt`.
pub fn start_secondary_harts(bootstrap_hart_id: usize) {
    const PHYSICAL_ENTRY: usize = 0x8020_0000;
    const SBI_SUCCESS: isize = 0;
    const SBI_ERR_INVALID_PARAM: isize = -3;
    const SBI_ERR_ALREADY_AVAILABLE: isize = -6;

    info!("hart {} entering HSM probe/start loop", bootstrap_hart_id);

    for target_hart in 0..crate::config::MAX_HARTS {
        #[cfg(feature = "platform-visionfive2")]
        if target_hart == 0 {
            // JH7110 hart0 is the E24 management core, not one of the U74
            // application harts CosmOS runs on. Starting it at the U74 kernel
            // entry makes OpenSBI trap before S-mode is reached.
            info!("hart {} skips JH7110 E24 hart0", bootstrap_hart_id);
            continue;
        }
        let status = sbi::hart_get_status(target_hart);
        if status.error == SBI_ERR_INVALID_PARAM {
            info!(
                "hart {} got invalid hart id while probing hart {}, stop scan",
                bootstrap_hart_id, target_hart
            );
            break;
        }
        if status.error != SBI_SUCCESS {
            info!(
                "hart {} HSM status query for hart {} failed: error={}, value={}",
                bootstrap_hart_id, target_hart, status.error, status.value
            );
            continue;
        }

        let state = sbi::hart_state(status.value);
        info!(
            "hart {} sees hart {} in HSM state {:?}",
            bootstrap_hart_id, target_hart, state
        );

        if target_hart == bootstrap_hart_id {
            continue;
        }

        if let sbi::HartState::Stopped = state {
            let ret = sbi::hart_start(target_hart, PHYSICAL_ENTRY, 0);
            match ret.error {
                SBI_SUCCESS => info!(
                    "hart {} requested startup for hart {}",
                    bootstrap_hart_id, target_hart
                ),
                SBI_ERR_ALREADY_AVAILABLE => info!(
                    "hart {} found hart {} already available while starting",
                    bootstrap_hart_id, target_hart
                ),
                error => info!(
                    "hart {} failed to start hart {}: error={}, value={}",
                    bootstrap_hart_id, target_hart, error, ret.value
                ),
            }
        }
    }
}

/// Translate one direct-mapped physical address into the kernel VA used on this platform.
pub fn direct_map_phys_to_virt(pa: usize) -> usize {
    KERNEL_ADDR_OFFSET.wrapping_add(pa)
}

/// Translate one direct-mapped kernel VA back into a physical address.
pub fn direct_map_virt_to_phys(va: usize) -> usize {
    if (KERNEL_MMIO_OFFSET..KERNEL_MMIO_OFFSET + KERNEL_MMIO_SIZE).contains(&va) {
        va.wrapping_sub(KERNEL_MMIO_OFFSET)
    } else if va >= KERNEL_ADDR_OFFSET {
        va.wrapping_sub(KERNEL_ADDR_OFFSET)
    } else {
        // Early firmware pointers are physical until the high-half bootstrap
        // mapping is installed.
        va
    }
}

/// Translate a direct-mapped kernel VA into a physical address when applicable.
pub fn translate_direct_mapped_kernel_va(va: usize) -> Option<usize> {
    /*
     * Root entry 256 is the separately-backed virtual kernel heap, not a
     * linear physical alias.  The QEMU RAM alias starts at PA 0x8000_0000
     * (root entry 258), so heap buffers must fall through to a page-table walk
     * when a device asks for their physical address.
     */
    #[cfg(feature = "platform-visionfive2")]
    let ram_alias_start = KERNEL_ADDR_OFFSET + 0x4000_0000;
    #[cfg(not(feature = "platform-visionfive2"))]
    let ram_alias_start = KERNEL_ADDR_OFFSET + PHYSICAL_RAM_BASE;
    if (ram_alias_start..KERNEL_MMIO_OFFSET).contains(&va) {
        return Some(va - KERNEL_ADDR_OFFSET);
    }
    if (KERNEL_MMIO_OFFSET..KERNEL_MMIO_OFFSET + KERNEL_MMIO_SIZE).contains(&va) {
        return Some(va - KERNEL_MMIO_OFFSET);
    }
    None
}

/// Translate one MMIO physical address into the VA used by drivers.
pub fn mmio_phys_to_virt(paddr: usize) -> usize {
    KERNEL_MMIO_OFFSET.wrapping_add(paddr)
}

/// Whether the Goldfish RTC is supported on this platform.
pub fn rtc_is_supported() -> bool {
    crate::bootinfo::get().rtc().is_some()
}

/// Whether the kernel heap may grow inside its dedicated virtual window.
pub fn kernel_heap_virtual_window_supported() -> bool {
    true
}

/// Whether extra heap bring-up debugging is enabled for this platform.
pub fn heap_debug_enabled() -> bool {
    false
}
