//! QEMU `virt` platform for LoongArch64.

mod board;
mod irq;
mod pci;
pub mod rtc;

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

const IOCSR_IPI_SEND: usize = 0x1040;
const IOCSR_MBUF_SEND: usize = 0x1048;

const IOCSR_IPI_SEND_BLOCKING: u32 = 1 << 31;
const IOCSR_IPI_SEND_CPU_SHIFT: u32 = 16;

const IOCSR_MBUF_SEND_BLOCKING: u64 = 1 << 31;
const IOCSR_MBUF_SEND_BOX_SHIFT: u64 = 2;
const IOCSR_MBUF_SEND_CPU_SHIFT: u64 = 16;
const IOCSR_MBUF_SEND_BUF_SHIFT: u64 = 32;
const IOCSR_MBUF_SEND_H32_MASK: u64 = 0xffff_ffff_0000_0000;

// ACTION_BOOT_CPU matches Linux's SMP_RESCHEDULE_YOURSELF bit used for boot IPI
const ACTION_BOOT_CPU: u32 = 1;
const ACTION_RESCHEDULE: u32 = 1;

#[inline]
unsafe fn iocsr_write32(addr: usize, val: u32) {
    core::arch::asm!("iocsrwr.w {v}, {a}", v = in(reg) val, a = in(reg) addr);
}

#[inline]
unsafe fn iocsr_write64(addr: usize, val: u64) {
    core::arch::asm!("iocsrwr.d {v}, {a}", v = in(reg) val, a = in(reg) addr);
}

fn ipi_send(hart_id: usize, action: u32) {
    let val = IOCSR_IPI_SEND_BLOCKING | action | (hart_id as u32) << IOCSR_IPI_SEND_CPU_SHIFT;
    unsafe { iocsr_write32(IOCSR_IPI_SEND, val) };
}

// Sends a 64-bit value to the target hart's MBUF0 mailbox, split into two 32-bit writes.
fn mail_send(data: u64, hart_id: usize, mailbox: u64) {
    let cpu = (hart_id as u64) << IOCSR_MBUF_SEND_CPU_SHIFT;
    // high 32 bits
    let hi = IOCSR_MBUF_SEND_BLOCKING
        | (((mailbox << 1) + 1) << IOCSR_MBUF_SEND_BOX_SHIFT)
        | cpu
        | (data & IOCSR_MBUF_SEND_H32_MASK);
    // low 32 bits
    let lo = IOCSR_MBUF_SEND_BLOCKING
        | ((mailbox << 1) << IOCSR_MBUF_SEND_BOX_SHIFT)
        | cpu
        | (data << IOCSR_MBUF_SEND_BUF_SHIFT);
    unsafe {
        iocsr_write64(IOCSR_MBUF_SEND, hi);
        iocsr_write64(IOCSR_MBUF_SEND, lo);
    }
}

impl HartCtrl for LoongArchPlatform {
    fn start_hart(hart_id: usize, start_addr: usize, _opaque: usize) -> Result<(), ()> {
        mail_send(start_addr as u64, hart_id, 0);
        ipi_send(hart_id, ACTION_BOOT_CPU);
        Ok(())
    }

    fn send_ipi(hart_mask: usize) {
        for hart_id in 0..usize::BITS as usize {
            if hart_mask & (1 << hart_id) != 0 {
                ipi_send(hart_id, ACTION_RESCHEDULE);
            }
        }
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

/// Start all secondary harts via IOCSR mailbox + IPI.
pub fn start_secondary_harts(bootstrap_hart_id: usize) {
    extern "C" { fn _start(); }
    // The firmware polls IOCSR_MBUF0 for a physical address, so strip the DMW offset.
    let entry_phys = (_start as usize) & !KERNEL_ADDR_OFFSET;
    for hart_id in 0..crate::config::MAX_HARTS {
        if hart_id == bootstrap_hart_id {
            continue;
        }
        let _ = <LoongArchPlatform as HartCtrl>::start_hart(hart_id, entry_phys, 0);
        info!("hart {} requested startup for hart {}", bootstrap_hart_id, hart_id);
    }
}

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

/// Whether the RTC is supported on this platform.
pub fn rtc_is_supported() -> bool {
    true
}

/// Whether the kernel heap may grow inside its dedicated virtual window.
pub fn kernel_heap_virtual_window_supported() -> bool {
    false
}

/// Whether extra heap bring-up debugging is enabled for this platform.
pub fn heap_debug_enabled() -> bool {
    true
}
