//! LoongArch64 QEMU `virt` external IRQ routing.
//!
//! This is platform glue rather than generic architecture code: QEMU wires the
//! console UART into the LS7A PCH PIC, then forwards it through EXTIOI onto a
//! CPU hardware interrupt line.

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::bootstrap_hart_id;
use crate::drivers::chardev::{CharDevice, UART};

static UART_IRQ_READY: AtomicBool = AtomicBool::new(false);

const PCH_PIC_BASE: usize = super::IO_ADDR_OFFSET | 0x1000_0000;
const PCH_PIC_INT_MASK: usize = 0x20;
const PCH_PIC_HTMSI_VEC: usize = 0x200;
const PCH_PIC_IRQS: u32 = 32;

const EXTIOI_BASE: usize = 0x1400;
const EXTIOI_IPMAP_START: usize = 0x0c0;
const EXTIOI_ENABLE_START: usize = 0x200;
const EXTIOI_COREISR_START: usize = 0x400;
const EXTIOI_COREMAP_START: usize = 0x800;

const UART0_PCH_IRQ: u32 = 2;
const EXTIOI_ROUTE_IP3: u32 = 0x0808_0808;

#[inline]
fn mmio_read64(addr: usize) -> u64 {
    unsafe { read_volatile(addr as *const u64) }
}

#[inline]
fn mmio_write64(addr: usize, value: u64) {
    unsafe { write_volatile(addr as *mut u64, value) }
}

#[inline]
fn iocsr_read32(addr: usize) -> u32 {
    let value: u32;
    unsafe {
        asm!(
            "iocsrrd.w {value}, {addr}",
            value = out(reg) value,
            addr = in(reg) addr,
        );
    }
    value
}

#[inline]
fn iocsr_write32(addr: usize, value: u32) {
    unsafe {
        asm!(
            "iocsrwr.w {value}, {addr}",
            value = in(reg) value,
            addr = in(reg) addr,
        );
    }
}

fn enable_pch_pic_irq(irq: u32) {
    let irq = irq as usize;
    let vec_reg = PCH_PIC_BASE + PCH_PIC_HTMSI_VEC + (irq & !7);
    let vec_shift = (irq & 7) * 8;
    let mut vectors = mmio_read64(vec_reg);
    vectors &= !(0xffu64 << vec_shift);
    vectors |= (irq as u64) << vec_shift;
    mmio_write64(vec_reg, vectors);

    let mask = mmio_read64(PCH_PIC_BASE + PCH_PIC_INT_MASK);
    mmio_write64(PCH_PIC_BASE + PCH_PIC_INT_MASK, mask & !(1u64 << irq));
}

fn init_extioi_routing() {
    let target_hart = bootstrap_hart_id().min(3);
    let cpu_bit = 1u32 << target_hart;

    for reg in 0..2 {
        iocsr_write32(EXTIOI_BASE + EXTIOI_IPMAP_START + reg * 4, EXTIOI_ROUTE_IP3);
    }

    let coremap_word = cpu_bit | (cpu_bit << 8) | (cpu_bit << 16) | (cpu_bit << 24);
    for reg in 0..64 {
        iocsr_write32(EXTIOI_BASE + EXTIOI_COREMAP_START + reg * 4, coremap_word);
    }
}

pub(crate) fn enable_pch_irq(irq: u32) -> bool {
    if irq >= PCH_PIC_IRQS {
        warn!("[irq] loongarch PCH IRQ {} out of range", irq);
        return false;
    }

    let word = (irq / 32) as usize;
    let bit = 1u32 << (irq % 32);
    iocsr_write32(EXTIOI_BASE + EXTIOI_COREISR_START + word * 4, bit);
    let enable = iocsr_read32(EXTIOI_BASE + EXTIOI_ENABLE_START + word * 4);
    iocsr_write32(
        EXTIOI_BASE + EXTIOI_ENABLE_START + word * 4,
        enable | bit,
    );
    enable_pch_pic_irq(irq);
    true
}

/// Initialize platform external interrupt routing on the bootstrap hart.
pub fn init_external_irq() {
    init_extioi_routing();
    enable_pch_irq(UART0_PCH_IRQ);
    UART_IRQ_READY.store(true, Ordering::Release);
    info!(
        "[irq] loongarch uart IRQ enabled on hart {}",
        bootstrap_hart_id().min(3)
    );
}

/// Initialize per-hart external interrupt state.
pub fn init_external_irq_hart(_hart_id: usize) {}

/// Whether the console RX interrupt path is ready for blocking reads.
pub fn console_rx_irq_ready() -> bool {
    UART_IRQ_READY.load(Ordering::Acquire)
}

/// Dispatch one platform external interrupt.
pub fn handle_external_irq() {
    if !console_rx_irq_ready() {
        return;
    }

    for word in 0..((PCH_PIC_IRQS as usize + 31) / 32) {
        let mut pending = iocsr_read32(EXTIOI_BASE + EXTIOI_COREISR_START + word * 4);
        let mut clear_mask = 0u32;

        while pending != 0 {
            let bit_idx = pending.trailing_zeros();
            let bit = 1u32 << bit_idx;
            let irq = (word as u32) * 32 + bit_idx;
            let mut handled = false;

            if irq == UART0_PCH_IRQ {
                UART.handle_irq();
                crate::fs::console_receive();
                handled = true;
            }
            handled |= crate::drivers::block::handle_irq(irq);
            handled |= crate::drivers::net::handle_irq(irq);

            if !handled {
                warn!("[irq] loongarch unexpected EXTIOI IRQ {}", irq);
            }

            clear_mask |= bit;
            pending &= !bit;
        }

        if clear_mask != 0 {
            iocsr_write32(EXTIOI_BASE + EXTIOI_COREISR_START + word * 4, clear_mask);
        }
    }
}
