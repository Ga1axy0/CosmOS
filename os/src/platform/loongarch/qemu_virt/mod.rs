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
const IOCSR_IPI_EN: usize = 0x1004;
const IOCSR_IPI_CLEAR: usize = 0x100c;
const IOCSR_MBUF_SEND: usize = 0x1048;

const IOCSR_IPI_SEND_CPU_SHIFT: u32 = 16;
const IOCSR_MBUF_SEND_CPU_SHIFT: u64 = 16;
const IOCSR_MBUF_SEND_DATA_SHIFT: u64 = 32;

// QEMU's LoongArch IPI model treats IOCSR_IPI_SEND[4:0] as a vector number,
// and the per-core enable/clear registers as vector bitmasks.
const IPI_VECTOR_WAKEUP: u32 = 1;
const IPI_VECTOR_MASK: u32 = 1 << IPI_VECTOR_WAKEUP;

#[inline]
unsafe fn iocsr_write32(addr: usize, val: u32) {
    core::arch::asm!("iocsrwr.w {v}, {a}", v = in(reg) val, a = in(reg) addr);
}

#[inline]
unsafe fn iocsr_write64(addr: usize, val: u64) {
    core::arch::asm!("iocsrwr.d {v}, {a}", v = in(reg) val, a = in(reg) addr);
}

fn ipi_send(hart_id: usize, vector: u32) {
    let val = vector | (hart_id as u32) << IOCSR_IPI_SEND_CPU_SHIFT;
    unsafe { iocsr_write32(IOCSR_IPI_SEND, val) };
}

fn enable_ipi() {
    unsafe { iocsr_write32(IOCSR_IPI_EN, IPI_VECTOR_MASK) };
}

fn clear_ipi(vector: u32) {
    unsafe { iocsr_write32(IOCSR_IPI_CLEAR, 1 << vector) };
}

#[inline]
fn mailbox_word_slot(mailbox: u64, upper_half: bool) -> u64 {
    debug_assert!(mailbox < 4);
    (mailbox << 3) | ((upper_half as u64) << 2)
}

#[inline]
fn mail_send_word(word: u32, hart_id: usize, slot: u64) {
    let val = ((word as u64) << IOCSR_MBUF_SEND_DATA_SHIFT)
        | ((hart_id as u64) << IOCSR_MBUF_SEND_CPU_SHIFT)
        | slot;
    unsafe { iocsr_write64(IOCSR_MBUF_SEND, val) };
}

// QEMU decodes MAIL_SEND as one 32-bit mailbox-slot write per request. To
// publish a 64-bit entry address in MBUF0, we must write its low/high halves
// into CORE_BUF_20 and CORE_BUF_24 separately.
fn mail_send(data: u64, hart_id: usize, mailbox: u64) {
    mail_send_word(data as u32, hart_id, mailbox_word_slot(mailbox, false));
    mail_send_word((data >> 32) as u32, hart_id, mailbox_word_slot(mailbox, true));
}

impl HartCtrl for LoongArchPlatform {
    fn start_hart(hart_id: usize, start_addr: usize, _opaque: usize) -> Result<(), ()> {
        mail_send(start_addr as u64, hart_id, 0);
        ipi_send(hart_id, IPI_VECTOR_WAKEUP);
        Ok(())
    }

    fn send_ipi(hart_mask: usize) {
        for hart_id in 0..usize::BITS as usize {
            if hart_mask & (1 << hart_id) != 0 {
                ipi_send(hart_id, IPI_VECTOR_WAKEUP);
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
    extern "C" {
        fn _start();
    }

    // Under direct boot, CPU0 reaches the kernel through our tiny bootloader,
    // which jumps to the high-half DMW alias of `_start`. QEMU's secondary
    // slave stub simply `jirl`s to the mailbox value, so feeding it the raw
    // physical address would drop APs outside the cached DMW window right
    // after wakeup.
    let entry = _start as usize;
    enable_ipi();
    for hart_id in 0..crate::config::MAX_HARTS {
        if hart_id == bootstrap_hart_id {
            continue;
        }
        let _ = <LoongArchPlatform as HartCtrl>::start_hart(hart_id, entry, 0);
        warn!(
            "hart {} requested startup for hart {} at {:#x}",
            bootstrap_hart_id, hart_id, entry
        );
    }
}

/// Initialize per-hart IPI receive state.
pub fn init_ipi_hart() {
    enable_ipi();
}

/// Clear the wake/reschedule IPI vector on the current hart.
pub fn clear_ipi_vector() {
    clear_ipi(IPI_VECTOR_WAKEUP);
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
