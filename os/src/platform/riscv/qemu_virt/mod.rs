//! QEMU `virt` platform for RISC-V.

mod board;
pub mod sbi;

pub use board::{
    BlockDeviceImpl, CharDeviceImpl, CLOCK_FREQ, MMIO, QEMUExit, QEMU_EXIT_HANDLE, VIRT_RTC,
    VIRT_UART, VIRTIO_MMIO_BASE, VIRTIO_MMIO_IRQ_BASE, VIRTIO_MMIO_SLOTS, VIRTIO_MMIO_STRIDE,
};
pub use sbi::SbiPlatform;

pub const KERNEL_HEAP_BASE: usize = 0xffff_ffc0_0000_0000;
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
    true
}

/// Probe platform-specific devices after generic driver init.
pub fn probe_platform_devices() {
    crate::drivers::block::probe_block_devices();
    crate::drivers::net::probe_net_devices();
}

/// RISC-V always uses the normal UART path once the console layer is up.
pub fn use_early_console() -> bool {
    false
}

/// Write one string through the earliest available console path.
pub fn early_console_write(s: &str) {
    for byte in s.bytes() {
        sbi::console_putchar(byte as usize);
    }
}

/// Write one character to the platform console.
pub fn console_putchar(c: usize) {
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

/// Discover stopped harts via SBI HSM and start them on QEMU `virt`.
pub fn start_secondary_harts(bootstrap_hart_id: usize) {
    extern "C" {
        fn _start();
    }

    const SBI_SUCCESS: isize = 0;
    const SBI_ERR_INVALID_PARAM: isize = -3;
    const SBI_ERR_ALREADY_AVAILABLE: isize = -6;

    info!(
        "hart {} entering HSM probe/start loop",
        bootstrap_hart_id
    );

    for target_hart in 0..crate::config::MAX_HARTS {
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
            let ret = sbi::hart_start(target_hart, _start as usize, 0);
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
    pa
}

/// Translate one direct-mapped kernel VA back into a physical address.
pub fn direct_map_virt_to_phys(va: usize) -> usize {
    va
}

/// Translate a direct-mapped kernel VA into a physical address when applicable.
pub fn translate_direct_mapped_kernel_va(_va: usize) -> Option<usize> {
    None
}

/// Translate one MMIO physical address into the VA used by drivers.
pub fn mmio_phys_to_virt(paddr: usize) -> usize {
    paddr
}

/// Whether the Goldfish RTC is supported on this platform.
pub fn rtc_is_supported() -> bool {
    true
}

/// Whether the kernel heap may grow inside its dedicated virtual window.
pub fn kernel_heap_virtual_window_supported() -> bool {
    true
}

/// Whether extra heap bring-up debugging is enabled for this platform.
pub fn heap_debug_enabled() -> bool {
    false
}
