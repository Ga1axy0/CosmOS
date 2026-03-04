use core::ptr::{read_volatile, write_volatile};

use crate::drivers::chardev::{CharDevice, UART};

/// QEMU virt PLIC base.
const PLIC_BASE: usize = 0x0C00_0000;

/// hart0 S-mode context id on QEMU virt.
/// (Each hart has M-context then S-context, so for hart0: M=0, S=1)
const S_CONTEXT: usize = 1;

/// QEMU virt UART0 interrupt source id.
const UART0_IRQ: u32 = 10;

// QEMU virt exposes VirtIO MMIO interrupts as sources starting from 1.
// Each virtio-mmio slot corresponds to one interrupt source.

const VIRTIO_MMIO_IRQ_BASE: u32 = 1;

const VIRTIO_MMIO_IRQ_COUNT: u32 = 8;

#[inline(always)]
fn priority_ptr(irq: u32) -> *mut u32 {
    (PLIC_BASE + (irq as usize) * 4) as *mut u32
}

#[inline(always)]
fn enable_ptr(context: usize, irq: u32) -> *mut u32 {
    // enable bits start at 0x2000, each context has 0x80 bytes
    let base = PLIC_BASE + 0x2000 + context * 0x80;
    (base + ((irq as usize) / 32) * 4) as *mut u32
}

#[inline(always)]
fn threshold_ptr(context: usize) -> *mut u32 {
    (PLIC_BASE + 0x200000 + context * 0x1000) as *mut u32
}

#[inline(always)]
fn claim_complete_ptr(context: usize) -> *mut u32 {
    // claim/complete is at threshold + 4
    (PLIC_BASE + 0x200000 + context * 0x1000 + 4) as *mut u32
}

fn enable_irq(context: usize, irq: u32) {
    unsafe {
        let p = enable_ptr(context, irq);
        let mut v = read_volatile(p);
        v |= 1u32 << (irq % 32);
        write_volatile(p, v);
    }
}

fn set_priority(irq: u32, prio: u32) {
    debug!("Set IRQ {} priority to {}", irq, prio);
    unsafe { write_volatile(priority_ptr(irq), prio) }
}

fn set_threshold(context: usize, th: u32) {
    unsafe { write_volatile(threshold_ptr(context), th) }
}

fn claim(context: usize) -> u32 {
    unsafe { read_volatile(claim_complete_ptr(context)) }
}

fn complete(context: usize, irq: u32) {
    unsafe { write_volatile(claim_complete_ptr(context), irq) }
}

pub fn init() {
    debug!("[kernel] Initializing PLIC...");
    // allow UART0 to interrupt S-mode
    set_priority(UART0_IRQ, 1);
    debug!("[kernel] Set UART0 IRQ priority to 1");
    enable_irq(S_CONTEXT, UART0_IRQ);

    // allow VirtIO MMIO devices to interrupt S-mode
    {
        for irq in VIRTIO_MMIO_IRQ_BASE..(VIRTIO_MMIO_IRQ_BASE + VIRTIO_MMIO_IRQ_COUNT) {
            set_priority(irq, 1);
            enable_irq(S_CONTEXT, irq);
        }
    }
    set_threshold(S_CONTEXT, 0);
    debug!("[kernel] PLIC initialized.");
}

/// Called from trap handler on SupervisorExternal interrupt.
pub fn handle_supervisor_external() {
    let irq = claim(S_CONTEXT);
    match irq {
        UART0_IRQ => {
            UART.handle_irq();
        }
        /*  Not supported yet.

        irq if (VIRTIO_MMIO_IRQ_BASE..(VIRTIO_MMIO_IRQ_BASE + VIRTIO_MMIO_IRQ_COUNT))
            .contains(&irq) =>
        {
            // Dispatch to virtio devices (net, blk, etc.).
            crate::drivers::net::handle_irq(irq);
            crate::net::notify_irq();
        }
        */
        0 => {
            // spurious
        }
        _ => {
            // ignore other IRQs for now
        }
    }
    if irq != 0 {
        complete(S_CONTEXT, irq);
    }
}
