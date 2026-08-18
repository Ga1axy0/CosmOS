//! FDT-selected LoongArch64 platform support.

mod board;
mod irq;
mod pci;
pub mod rtc;

pub use board::{
    BlockDeviceImpl, CharDeviceImpl, QEMUExit, INTERP_BASE, IO_ADDR_OFFSET, KERNEL_ADDR_OFFSET,
    QEMU_EXIT_HANDLE, USER_MMAP_BASE, USER_STACK_BASE, VIRT_UART,
};
pub use irq::{
    console_rx_irq_ready, handle_external_irq, init_external_irq, init_external_irq_hart,
};

use crate::drivers::chardev::CharDevice;
use crate::hal::traits::{HartCtrl, Timer};

pub const KERNEL_HEAP_BASE: usize = 0x0000_0038_0000_0000;
pub const TRAMPOLINE: usize = 0x0000_003f_ffff_f000;

/// QEMU's direct loader passes the generated FDT address as argument 1.
pub const fn boot_fdt_ptr(raw: usize) -> usize {
    raw
}

fn is_qemu_virt() -> bool {
    crate::boot::context::try_get().is_some_and(|info| {
        info.devices().pci_host().is_some()
            && info.devices().pch_pic().is_some()
            && info.devices().eiointc().is_some()
    })
}

/// The common early initialization path is supported on every described board.
pub const fn continue_full_boot() -> bool {
    true
}

/// Continue to the root filesystem only after a block device was registered.
pub fn continue_storage_boot() -> bool {
    !crate::drivers::block::BLOCK_DEVICES.lock().is_empty()
}

/// Probe the storage/network transports exposed by the selected FDT.
pub fn probe_platform_devices() {
    #[cfg(feature = "platform-ls2k1000-nebula")]
    {
        if let Some(irq) = crate::drivers::net::probe_loongson_gmac() {
            if !irq::enable_device_irq(irq) {
                warn!("[kernel] LS2K1000 GMAC IRQ {} could not be routed", irq);
            }
        }
        if crate::boot::context::get().devices().ahci().is_some() {
            crate::drivers::block::probe_ahci();
            return;
        }
    }
    pci::probe_platform_devices();
}

/// Stop after the common early bring-up stages when requested by a backend.
pub fn halt_early_bringup() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

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
        crate::boot::context::timer_frequency()
    }
}

const IOCSR_IPI_SEND: usize = 0x1040;
const IOCSR_IPI_EN: usize = 0x1004;
const IOCSR_IPI_CLEAR: usize = 0x100c;
const IOCSR_MBUF_SEND: usize = 0x1048;

const IOCSR_IPI_SEND_CPU_SHIFT: u32 = 16;
const IOCSR_MBUF_SEND_CPU_SHIFT: u64 = 16;
const IOCSR_MBUF_SEND_DATA_SHIFT: u64 = 32;
const IOCSR_IPI_SEND_BLOCKING: u32 = 1 << 31;
const IOCSR_MBUF_SEND_BLOCKING: u64 = 1 << 31;

// IOCSR_IPI_SEND[4:0] selects a vector; the local enable/clear registers use
// the corresponding bit. Vector 0 is the firmware boot action, while vector 1
// is reserved for CosmOS wake/reschedule requests after a hart is online.
const IPI_VECTOR_BOOT: u32 = 0;
const IPI_VECTOR_WAKEUP: u32 = 1;

#[inline]
unsafe fn iocsr_write32(addr: usize, val: u32) {
    core::arch::asm!("iocsrwr.w {v}, {a}", v = in(reg) val, a = in(reg) addr);
}

#[inline]
unsafe fn iocsr_write64(addr: usize, val: u64) {
    core::arch::asm!("iocsrwr.d {v}, {a}", v = in(reg) val, a = in(reg) addr);
}

fn ipi_send(hart_id: usize, vector: u32) {
    let val = IOCSR_IPI_SEND_BLOCKING | vector | (hart_id as u32) << IOCSR_IPI_SEND_CPU_SHIFT;
    unsafe { iocsr_write32(IOCSR_IPI_SEND, val) };
}

fn enable_ipi() {
    unsafe { iocsr_write32(IOCSR_IPI_EN, u32::MAX) };
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
        | slot
        | IOCSR_MBUF_SEND_BLOCKING;
    unsafe { iocsr_write64(IOCSR_MBUF_SEND, val) };
}

// MAIL_SEND publishes one 32-bit mailbox-slot word per request. Write the high
// half first and the low half last, matching the LoongArch SMP boot protocol.
fn mail_send(data: u64, hart_id: usize, mailbox: u64) {
    mail_send_word(
        (data >> 32) as u32,
        hart_id,
        mailbox_word_slot(mailbox, true),
    );
    mail_send_word(data as u32, hart_id, mailbox_word_slot(mailbox, false));
}

impl HartCtrl for LoongArchPlatform {
    fn start_hart(hart_id: usize, start_addr: usize, _opaque: usize) -> Result<(), ()> {
        mail_send(start_addr as u64, hart_id, 0);
        ipi_send(hart_id, IPI_VECTOR_BOOT);
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
    let uart = crate::boot::context::try_get()
        .and_then(|info| info.devices().uart())
        .map(|resource| crate::platform::mmio_phys_to_virt(resource.start))
        .unwrap_or(VIRT_UART);
    for b in s.bytes() {
        unsafe {
            while core::ptr::read_volatile((uart + 5) as *const u8) & 0x20 == 0 {}
            core::ptr::write_volatile(uart as *mut u8, b);
        }
    }
}

/// Verify the portable compiler baseline against the CPU before Rust proceeds.
pub fn early_runtime_diagnostics() {
    const CPUCFG1_UAL: usize = 1 << 20;
    const COMPILER_UAL: bool = cfg!(target_feature = "ual");
    let state = crate::arch::loongarch64::boot_execution_state();
    if COMPILER_UAL && state.cpucfg1 & CPUCFG1_UAL == 0 {
        early_console_write("[loongarch] compiler requires UAL but CPU lacks it\r\n");
        halt_early_bringup();
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

/// Return the platform name for display purposes.
pub fn platform_name() -> &'static str {
    if is_qemu_virt() {
        "qemu virt"
    } else {
        "firmware-described LoongArch board"
    }
}

/// Start all secondary harts via IOCSR mailbox + IPI.
pub fn start_secondary_harts(bootstrap_hart_id: usize) {
    extern "C" {
        fn _start();
    }

    let qemu = is_qemu_virt();
    // QEMU's slave loop runs before a cached DMW has been installed, so it must
    // jump to the physical `_start`; that entry installs DMW1 and continues at
    // `_start_high`. The LS2K1000 U-Boot slave loop is already executing from
    // the cached DMW and jumps to the mailbox value exactly as written.
    let entry = if qemu {
        direct_map_virt_to_phys(_start as usize)
    } else {
        _start as usize
    };
    let boot_info = crate::boot::context::get();
    let hart_count = if boot_info.fdt_blob().is_some() {
        boot_info.hart_count()
    } else {
        crate::config::MAX_HARTS
    };
    enable_ipi();
    for hart_id in 0..hart_count.min(crate::config::MAX_HARTS) {
        if hart_id == bootstrap_hart_id {
            continue;
        }
        if qemu {
            // Discard a stale entry left by an earlier RAM boot before
            // publishing the new one, matching Linux's CPU prepare phase.
            mail_send(0, hart_id, 0);
            let _ = <LoongArchPlatform as HartCtrl>::start_hart(hart_id, entry, 0);
        } else {
            #[cfg(feature = "platform-ls2k1000-nebula")]
            super::ls2k1000_nebula::start_secondary_hart(hart_id, entry);
        }
        warn!(
            "hart {} requested startup for hart {} at {:#x}",
            bootstrap_hart_id, hart_id, entry
        );
    }
}

/// Initialize per-hart IPI receive state.
pub fn init_ipi_hart() {
    unsafe { iocsr_write32(IOCSR_IPI_CLEAR, u32::MAX) };
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
    crate::boot::context::get().devices().rtc().is_some()
}

/// Whether the kernel heap may grow inside its dedicated virtual window.
pub fn kernel_heap_virtual_window_supported() -> bool {
    false
}

/// Whether extra heap bring-up debugging is enabled for this platform.
pub fn heap_debug_enabled() -> bool {
    true
}
