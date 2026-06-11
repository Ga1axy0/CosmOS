//! QEMU `virt` platform for LoongArch64.

mod board;
mod irq;
mod pci;

pub use board::{
    BlockDeviceImpl, CharDeviceImpl, USER_STACK_BASE, USER_MMAP_BASE, INTERP_BASE, CLOCK_FREQ, IO_ADDR_OFFSET, KERNEL_ADDR_OFFSET, MMIO,
    QEMUExit, QEMU_EXIT_HANDLE, VIRT_RTC, VIRT_UART, VIRTIO_MMIO_BASE, VIRTIO_MMIO_IRQ_BASE,
    VIRTIO_MMIO_SLOTS, VIRTIO_MMIO_STRIDE,
};
pub use irq::{
    console_rx_irq_ready, handle_external_irq, init_external_irq, init_external_irq_hart,
};
pub use pci::probe_platform_devices;

use crate::drivers::chardev::CharDevice;
use crate::hal::traits::{HartCtrl, Timer};

pub const KERNEL_HEAP_BASE: usize = 0x0000_0038_0000_0000;
pub const TRAMPOLINE: usize = 0x0000_003f_ffff_f000;

/// LoongArch64 platform implementation used by the generic HAL facade.
pub struct LoongArchPlatform;

impl Timer for LoongArchPlatform {
    fn read_time() -> usize {
        crate::arch::loongarch64::read_time()
    }

    fn set_next(deadline: usize) {
        unsafe { crate::arch::loongarch64::set_timer_deadline(deadline) };
    }

    fn clock_freq() -> usize {
        crate::config::CLOCK_FREQ
    }
}

impl HartCtrl for LoongArchPlatform {
    fn start_hart(_hart_id: usize, _start_addr: usize, _opaque: usize) -> Result<(), ()> {
        Err(())
    }

    fn send_ipi(_hart_mask: usize) {
        // Single-core bring-up only for now.
    }
}

/// Whether console output should still use the earliest UART path.
pub fn use_early_console() -> bool {
    !crate::drivers::chardev::uart_ready()
}

/// Write one string through the earliest available console path.
pub fn early_console_write(s: &str) {
    for b in s.bytes() {
        unsafe {
            while core::ptr::read_volatile((VIRT_UART + 5) as *const u8) & 0x20 == 0 {}
            core::ptr::write_volatile(VIRT_UART as *mut u8, b);
        }
    }
}

/// Write one character to the platform console.
pub fn console_putchar(c: usize) {
    crate::drivers::chardev::UART.write(c as u8);
}

/// Read one character from the platform console.
pub fn console_getchar() -> usize {
    crate::drivers::chardev::UART.read() as usize
}

/// Power off the virtual machine.
pub fn shutdown() -> ! {
    QEMU_EXIT_HANDLE.exit_success()
}

/// Return the uname-style machine string.
pub fn machine_name() -> &'static str {
    "loongarch64"
}

/// Secondary-hart start-up is not wired up on this platform yet.
pub fn start_secondary_harts(_bootstrap_hart_id: usize) {}

/// Translate one direct-mapped physical address into the kernel VA used on this platform.
pub fn direct_map_phys_to_virt(pa: usize) -> usize {
    pa | KERNEL_ADDR_OFFSET
}

/// Translate one direct-mapped kernel VA back into a physical address.
pub fn direct_map_virt_to_phys(va: usize) -> usize {
    va & !KERNEL_ADDR_OFFSET
}

/// Translate a direct-mapped kernel VA into a physical address when applicable.
pub fn translate_direct_mapped_kernel_va(va: usize) -> Option<usize> {
    if va & KERNEL_ADDR_OFFSET == KERNEL_ADDR_OFFSET {
        return Some(va & !KERNEL_ADDR_OFFSET);
    }
    if va & IO_ADDR_OFFSET == IO_ADDR_OFFSET {
        return Some(va & !IO_ADDR_OFFSET);
    }
    None
}

/// Translate one MMIO physical address into the VA used by drivers.
pub fn mmio_phys_to_virt(paddr: usize) -> usize {
    paddr | IO_ADDR_OFFSET
}

/// Whether the Goldfish RTC is supported on this platform.
pub fn rtc_is_supported() -> bool {
    false
}

/// Whether the kernel heap may grow inside its dedicated virtual window.
pub fn kernel_heap_virtual_window_supported() -> bool {
    false
}

/// Whether extra heap bring-up debugging is enabled for this platform.
pub fn heap_debug_enabled() -> bool {
    true
}
