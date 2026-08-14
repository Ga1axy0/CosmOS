//! LoongArch64 QEMU `virt` external IRQ routing.
//!
//! This is platform glue rather than generic architecture code: QEMU wires the
//! console UART into the LS7A PCH PIC, then forwards it through EXTIOI onto a
//! CPU hardware interrupt line.

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
#[cfg(feature = "platform-ls2k1000-nebula")]
use core::sync::atomic::AtomicU32;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::bootstrap_hart_id;
use crate::drivers::chardev::{CharDevice, UART};
#[cfg(feature = "platform-ls2k1000-nebula")]
use crate::println;

static UART_IRQ_READY: AtomicBool = AtomicBool::new(false);

#[cfg(feature = "platform-ls2k1000-nebula")]
static LIOINTC_READY: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "platform-ls2k1000-nebula")]
static LIOINTC_ENABLED: AtomicU32 = AtomicU32::new(0);

// LS2K1000 LIOINTC defaults match the board HAL. Some U-Boot control FDTs do
// not expose this internal interrupt controller, so the fixed SoC addresses
// are deliberately used as the fallback platform description.
#[cfg(feature = "platform-ls2k1000-nebula")]
const LIOINTC_REG_PADDR: usize = 0x1fe0_1400;
#[cfg(feature = "platform-ls2k1000-nebula")]
const LIOINTC_ISR_PADDR: usize = 0x1fe0_1040;
#[cfg(feature = "platform-ls2k1000-nebula")]
const LIOINTC_INPUTS: u32 = 32;
#[cfg(feature = "platform-ls2k1000-nebula")]
const LIOINTC_ROUTE_CPU0_INT0: u8 = 0x11;
#[cfg(feature = "platform-ls2k1000-nebula")]
const LIOINTC_ENABLE: usize = 0x28;
#[cfg(feature = "platform-ls2k1000-nebula")]
const LIOINTC_DISABLE: usize = 0x2c;
#[cfg(feature = "platform-ls2k1000-nebula")]
const LIOINTC_POLARITY: usize = 0x30;
#[cfg(feature = "platform-ls2k1000-nebula")]
const LIOINTC_EDGE: usize = 0x34;

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
const PCH_PIC_INT_MASK: usize = 0x20;
#[cfg(not(feature = "platform-ls2k1000-nebula"))]
const PCH_PIC_HTMSI_VEC: usize = 0x200;
#[cfg(not(feature = "platform-ls2k1000-nebula"))]
const PCH_PIC_IRQS: u32 = 32;

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
const EXTIOI_IPMAP_START: usize = 0x0c0;
#[cfg(not(feature = "platform-ls2k1000-nebula"))]
const EXTIOI_ENABLE_START: usize = 0x200;
#[cfg(not(feature = "platform-ls2k1000-nebula"))]
const EXTIOI_COREISR_START: usize = 0x400;
#[cfg(not(feature = "platform-ls2k1000-nebula"))]
const EXTIOI_COREMAP_START: usize = 0x800;

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
const EXTIOI_ROUTE_IP3: u32 = 0x0808_0808;

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
fn pch_pic_base() -> usize {
    let resource = crate::bootinfo::get()
        .pch_pic()
        .expect("FDT has no Loongson PCH PIC");
    crate::platform::mmio_phys_to_virt(resource.start)
}

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
fn extioi_base() -> usize {
    crate::bootinfo::get()
        .eiointc()
        .expect("FDT has no Loongson EIOINTC")
        .start
}

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
fn uart_irq() -> u32 {
    crate::bootinfo::get()
        .uart()
        .and_then(|resource| resource.irq)
        .expect("FDT console UART has no interrupt")
}

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
#[inline]
fn mmio_read64(addr: usize) -> u64 {
    unsafe { read_volatile(addr as *const u64) }
}

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
#[inline]
fn mmio_write64(addr: usize, value: u64) {
    unsafe { write_volatile(addr as *mut u64, value) }
}

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
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

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
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

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
fn enable_pch_pic_irq(irq: u32) {
    let irq = irq as usize;
    let base = pch_pic_base();
    let vec_reg = base + PCH_PIC_HTMSI_VEC + (irq & !7);
    let vec_shift = (irq & 7) * 8;
    let mut vectors = mmio_read64(vec_reg);
    vectors &= !(0xffu64 << vec_shift);
    vectors |= (irq as u64) << vec_shift;
    mmio_write64(vec_reg, vectors);

    let mask = mmio_read64(base + PCH_PIC_INT_MASK);
    mmio_write64(base + PCH_PIC_INT_MASK, mask & !(1u64 << irq));
}

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
fn init_extioi_routing() {
    let target_hart = bootstrap_hart_id().min(3);
    let cpu_bit = 1u32 << target_hart;

    for reg in 0..2 {
        iocsr_write32(
            extioi_base() + EXTIOI_IPMAP_START + reg * 4,
            EXTIOI_ROUTE_IP3,
        );
    }

    let coremap_word = cpu_bit | (cpu_bit << 8) | (cpu_bit << 16) | (cpu_bit << 24);
    for reg in 0..64 {
        iocsr_write32(extioi_base() + EXTIOI_COREMAP_START + reg * 4, coremap_word);
    }
}

#[cfg(not(feature = "platform-ls2k1000-nebula"))]
pub(crate) fn enable_device_irq(irq: u32) -> bool {
    if irq >= PCH_PIC_IRQS {
        warn!("[irq] loongarch PCH IRQ {} out of range", irq);
        return false;
    }

    let word = (irq / 32) as usize;
    let bit = 1u32 << (irq % 32);
    iocsr_write32(extioi_base() + EXTIOI_COREISR_START + word * 4, bit);
    let enable = iocsr_read32(extioi_base() + EXTIOI_ENABLE_START + word * 4);
    iocsr_write32(extioi_base() + EXTIOI_ENABLE_START + word * 4, enable | bit);
    enable_pch_pic_irq(irq);
    true
}

#[cfg(feature = "platform-ls2k1000-nebula")]
#[inline]
fn liointc_regs() -> usize {
    crate::platform::mmio_phys_to_virt(LIOINTC_REG_PADDR)
}

#[cfg(feature = "platform-ls2k1000-nebula")]
#[inline]
fn liointc_isr() -> usize {
    crate::platform::mmio_phys_to_virt(LIOINTC_ISR_PADDR)
}

#[cfg(feature = "platform-ls2k1000-nebula")]
#[inline]
fn liointc_write32(offset: usize, value: u32) {
    unsafe { write_volatile((liointc_regs() + offset) as *mut u32, value) }
}

#[cfg(feature = "platform-ls2k1000-nebula")]
#[inline]
fn liointc_pending() -> u32 {
    (unsafe { read_volatile(liointc_isr() as *const u32) })
        & LIOINTC_ENABLED.load(Ordering::Acquire)
}

#[cfg(feature = "platform-ls2k1000-nebula")]
pub(crate) fn enable_device_irq(irq: u32) -> bool {
    if irq >= LIOINTC_INPUTS {
        warn!("[irq] LS2K1000 LIOINTC input {} out of range", irq);
        return false;
    }
    if !LIOINTC_READY.load(Ordering::Acquire) {
        warn!("[irq] LS2K1000 LIOINTC is not initialized");
        return false;
    }

    let bit = 1u32 << irq;
    LIOINTC_ENABLED.fetch_or(bit, Ordering::AcqRel);
    liointc_write32(LIOINTC_ENABLE, bit);
    println!("[irq] LS2K1000 LIOINTC input {} enabled", irq);
    true
}

/// Initialize platform external interrupt routing on the bootstrap hart.
#[cfg(not(feature = "platform-ls2k1000-nebula"))]
pub fn init_external_irq() {
    if crate::bootinfo::get().pch_pic().is_none()
        || crate::bootinfo::get().eiointc().is_none()
        || crate::bootinfo::get()
            .uart()
            .and_then(|uart| uart.irq)
            .is_none()
    {
        return;
    }
    init_extioi_routing();
    enable_device_irq(uart_irq());
    UART_IRQ_READY.store(true, Ordering::Release);
    info!(
        "[irq] loongarch uart IRQ enabled on hart {}",
        bootstrap_hart_id().min(3)
    );
}

/// Initialize the LS2K1000 ICU and route its inputs to CPU0 HWI0.
#[cfg(feature = "platform-ls2k1000-nebula")]
pub fn init_external_irq() {
    let regs = liointc_regs();
    for input in 0..LIOINTC_INPUTS as usize {
        unsafe { write_volatile((regs + input) as *mut u8, LIOINTC_ROUTE_CPU0_INT0) };
    }
    liointc_write32(LIOINTC_DISABLE, u32::MAX);
    liointc_write32(LIOINTC_EDGE, 0);
    liointc_write32(LIOINTC_POLARITY, 0);
    LIOINTC_ENABLED.store(0, Ordering::Release);
    LIOINTC_READY.store(true, Ordering::Release);
    println!(
        "[irq] LS2K1000 LIOINTC initialized at {:#x}, ISR {:#x}, cascade HWI0",
        LIOINTC_REG_PADDR, LIOINTC_ISR_PADDR
    );
}

/// Initialize per-hart external interrupt state.
pub fn init_external_irq_hart(_hart_id: usize) {}

/// Whether the console RX interrupt path is ready for blocking reads.
pub fn console_rx_irq_ready() -> bool {
    UART_IRQ_READY.load(Ordering::Acquire)
}

/// Dispatch one platform external interrupt.
#[cfg(not(feature = "platform-ls2k1000-nebula"))]
pub fn handle_external_irq() {
    if !console_rx_irq_ready() {
        return;
    }

    for word in 0..((PCH_PIC_IRQS as usize + 31) / 32) {
        let mut pending = iocsr_read32(extioi_base() + EXTIOI_COREISR_START + word * 4);

        while pending != 0 {
            let bit_idx = pending.trailing_zeros();
            let bit = 1u32 << bit_idx;
            let irq = (word as u32) * 32 + bit_idx;

            // Acknowledge (EOI) the EXTIOI source BEFORE running its handler.
            // The previous "handle-then-clear" order could drop a virtio
            // completion IRQ: a second completion landing between the device
            // ACK (inside the handler) and the trailing write-1-to-clear would
            // have its freshly-set COREISR bit wiped by `clear_mask`, leaving
            // the block worker asleep in BLOCK_WORKER_WAIT for good. Clearing
            // first means any re-assertion during the handler sets a fresh bit
            // that survives and re-triggers after `ertn`.
            iocsr_write32(extioi_base() + EXTIOI_COREISR_START + word * 4, bit);

            let mut handled = false;

            if irq == uart_irq() {
                UART.handle_irq();
                crate::fs::console_receive();
                handled = true;
            }
            handled |= crate::drivers::block::handle_irq(irq);
            handled |= crate::drivers::net::handle_irq(irq);

            if !handled {
                warn!("[irq] loongarch unexpected EXTIOI IRQ {}", irq);
            }

            pending &= !bit;
        }
    }
}

/// Dispatch LS2K1000 LIOINTC inputs. The controller is level-triggered, so
/// the device handler clears the source and no controller EOI is required.
#[cfg(feature = "platform-ls2k1000-nebula")]
pub fn handle_external_irq() {
    if !LIOINTC_READY.load(Ordering::Acquire) {
        return;
    }

    let mut pending = liointc_pending();
    while pending != 0 {
        let irq = pending.trailing_zeros();
        let bit = 1u32 << irq;
        let mut handled = crate::drivers::block::handle_irq(irq);
        handled |= crate::drivers::net::handle_irq(irq);
        if !handled {
            warn!("[irq] unexpected LS2K1000 LIOINTC input {}", irq);
        }
        pending &= !bit;
    }
}
