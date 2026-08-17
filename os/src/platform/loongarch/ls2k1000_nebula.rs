//! Loongson 2K1000 Nebula board-specific SMP startup.

const IOCSR_IPI_SEND: usize = 0x1040;
const IOCSR_MBUF_SEND: usize = 0x1048;
const IOCSR_IPI_SEND_CPU_SHIFT: u32 = 16;
const IOCSR_MBUF_SEND_CPU_SHIFT: u64 = 16;
const IOCSR_MBUF_SEND_DATA_SHIFT: u64 = 32;
const IOCSR_IPI_SEND_BLOCKING: u32 = 1 << 31;
const IOCSR_MBUF_SEND_BLOCKING: u64 = 1 << 31;

const ACTION_BOOT_CPU: u32 = 0;
const CORE0_IPI_PHYS: usize = 0x1fe0_1000;
const CORE_STRIDE: usize = 0x100;
const FUNCTION_MAILBOX_OFFSET: usize = 0x20;

#[inline]
unsafe fn iocsr_write32(addr: usize, value: u32) {
    core::arch::asm!(
        "iocsrwr.w {value}, {addr}",
        value = in(reg) value,
        addr = in(reg) addr,
    );
}

#[inline]
unsafe fn iocsr_write64(addr: usize, value: u64) {
    core::arch::asm!(
        "iocsrwr.d {value}, {addr}",
        value = in(reg) value,
        addr = in(reg) addr,
    );
}

#[inline]
fn mailbox_word_slot(mailbox: u64, upper_half: bool) -> u64 {
    (mailbox << 3) | ((upper_half as u64) << 2)
}

fn mail_send_word(word: u32, hart_id: usize, slot: u64) {
    let value = ((word as u64) << IOCSR_MBUF_SEND_DATA_SHIFT)
        | ((hart_id as u64) << IOCSR_MBUF_SEND_CPU_SHIFT)
        | slot
        | IOCSR_MBUF_SEND_BLOCKING;
    unsafe { iocsr_write64(IOCSR_MBUF_SEND, value) };
}

fn mail_send(data: u64, hart_id: usize, mailbox: u64) {
    mail_send_word(
        (data >> 32) as u32,
        hart_id,
        mailbox_word_slot(mailbox, true),
    );
    mail_send_word(data as u32, hart_id, mailbox_word_slot(mailbox, false));
}

/// Start a core parked by the LS2K1000 U-Boot BSP.
///
/// This firmware jumps directly to mailbox 0, so it requires the cached DMW
/// entry rather than the physical address used by generic LoongArch firmware.
pub(crate) fn start_secondary_hart(hart_id: usize, cached_entry: usize) {
    let function_mailbox = crate::platform::mmio_phys_to_virt(
        CORE0_IPI_PHYS + hart_id * CORE_STRIDE + FUNCTION_MAILBOX_OFFSET,
    );

    unsafe {
        core::ptr::write_volatile(function_mailbox as *mut usize, 0);
        core::arch::asm!("dbar 0", options(nostack));
        core::ptr::write_volatile(function_mailbox as *mut usize, cached_entry);
        core::arch::asm!("dbar 0", options(nostack));
    }

    // IOCSR MBUF0 aliases the per-core function mailbox. Re-publish the same
    // cached entry and then wake the parked core with vector ACTION_BOOT_CPU.
    mail_send(0, hart_id, 0);
    mail_send(cached_entry as u64, hart_id, 0);
    let value =
        IOCSR_IPI_SEND_BLOCKING | ACTION_BOOT_CPU | (hart_id as u32) << IOCSR_IPI_SEND_CPU_SHIFT;
    unsafe { iocsr_write32(IOCSR_IPI_SEND, value) };
}
