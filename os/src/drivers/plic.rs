//! Initialization and interrpt handling for plic.

use core::ptr::{read_volatile, write_volatile};

use crate::bootstrap_hart_id;
use crate::config::MAX_HARTS;
use crate::drivers::chardev::{CharDevice, UART};
use crate::hart::hartid;
use crate::sync::SpinNoIrqLock;
use lazy_static::*;

/// QEMU virt PLIC base.
const PLIC_BASE: usize = 0x0C00_0000;

/// QEMU virt UART0 interrupt source id.
const UART0_IRQ: u32 = 10;

// QEMU virt exposes VirtIO MMIO interrupts as sources starting from 1.
// Each virtio-mmio slot corresponds to one interrupt source.

const VIRTIO_MMIO_IRQ_BASE: u32 = 1;

const VIRTIO_MMIO_IRQ_COUNT: u32 = 8;
const MAX_IRQ_ID: usize = 32;

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

fn disable_irq(context: usize, irq: u32) {
    unsafe {
        let p = enable_ptr(context, irq);
        let mut v = read_volatile(p);
        v &= !(1u32 << (irq % 32));
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

#[inline(always)]
fn supervisor_context(hart_id: usize) -> usize {
    hart_id * 2 + 1
}

lazy_static! {
    static ref IRQ_AFFINITY: SpinNoIrqLock<[usize; MAX_IRQ_ID]> =
        SpinNoIrqLock::new([usize::MAX; MAX_IRQ_ID]);
}

fn affinity_target(irq: u32) -> usize {
    IRQ_AFFINITY
        .lock()
        .get(irq as usize)
        .copied()
        .unwrap_or(bootstrap_hart_id())
}

fn set_irq_affinity_internal(irq: u32, hart_id: usize) {
    let target_hart = hart_id.min(MAX_HARTS.saturating_sub(1));
    if let Some(slot) = IRQ_AFFINITY.lock().get_mut(irq as usize) {
        *slot = target_hart;
    }
}

/// 初始化 PLIC 的全局优先级配置。
///
/// 这部分只需要由 bootstrap hart 执行一次，不依赖具体 hart context。
pub fn init() {
    debug!("[kernel] Initializing PLIC...");
    let housekeeping_hart = bootstrap_hart_id();
    set_irq_affinity_internal(UART0_IRQ, housekeeping_hart);
    set_priority(UART0_IRQ, 1);
    for irq in VIRTIO_MMIO_IRQ_BASE..(VIRTIO_MMIO_IRQ_BASE + VIRTIO_MMIO_IRQ_COUNT) {
        set_irq_affinity_internal(irq, housekeeping_hart);
        set_priority(irq, 1);
    }
    debug!("[kernel] PLIC global priority initialized.");
}

/// 初始化指定 hart 的 supervisor context。
///
/// 每个 hart 都需要各自执行一次，使能本地 context 的 IRQ 位图并设置 threshold。
pub fn init_hart(hart_id: usize) {
    let context = supervisor_context(hart_id);
    if affinity_target(UART0_IRQ) == hart_id {
        enable_irq(context, UART0_IRQ);
    } else {
        disable_irq(context, UART0_IRQ);
    }
    for irq in VIRTIO_MMIO_IRQ_BASE..(VIRTIO_MMIO_IRQ_BASE + VIRTIO_MMIO_IRQ_COUNT) {
        if affinity_target(irq) == hart_id {
            enable_irq(context, irq);
        } else {
            disable_irq(context, irq);
        }
    }
    set_threshold(context, 0);
    debug!("hart {} plic init done", hart_id);
}

/// Called from trap handler on SupervisorExternal interrupt.
pub fn handle_supervisor_external() {
    handle_supervisor_external_hart(hartid());
}

/// 处理指定 hart 的 supervisor external interrupt。
pub fn handle_supervisor_external_hart(hart_id: usize) {
    let context = supervisor_context(hart_id);
    let irq = claim(context);
    match irq {
        UART0_IRQ => {
            UART.handle_irq();
            // 把刚到达的输入立刻喂入控制台行规程：这样即便当前没有进程在 read，
            // Ctrl+C 等信号字符也能在到达瞬间生成信号投递给前台进程组。
            crate::fs::console_receive();
        }
        irq if (VIRTIO_MMIO_IRQ_BASE..(VIRTIO_MMIO_IRQ_BASE + VIRTIO_MMIO_IRQ_COUNT))
            .contains(&irq) =>
        {
            crate::drivers::block::handle_irq(irq);
            crate::drivers::net::handle_irq(irq);
        }
        0 => {
            // spurious
        }
        _ => {
            // ignore other IRQs for now
        }
    }
    if irq != 0 {
        complete(context, irq);
    }
}
