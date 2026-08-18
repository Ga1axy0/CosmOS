//! Initialization and interrpt handling for plic.

use core::ptr::{read_volatile, write_volatile};

use crate::bootstrap_hart_id;
use crate::config::MAX_HARTS;
use crate::drivers::chardev::{CharDevice, UART};
use crate::hal::hartid;
use crate::sync::SpinNoIrqLock;
use lazy_static::*;

#[inline(always)]
fn plic_base() -> usize {
    let resource = crate::boot::context::get()
        .devices()
        .plic()
        .expect("FDT has no enabled RISC-V PLIC");
    crate::platform::mmio_phys_to_virt(resource.start)
}

const MAX_IRQ_ID: usize = 256;

fn uart_irq() -> u32 {
    crate::boot::context::get()
        .devices()
        .uart()
        .and_then(|resource| resource.irq)
        .expect("FDT console UART has no interrupt")
}

#[inline(always)]
fn priority_ptr(irq: u32) -> *mut u32 {
    (plic_base() + (irq as usize) * 4) as *mut u32
}

#[inline(always)]
fn enable_ptr(context: usize, irq: u32) -> *mut u32 {
    // enable bits start at 0x2000, each context has 0x80 bytes
    let base = plic_base() + 0x2000 + context * 0x80;
    (base + ((irq as usize) / 32) * 4) as *mut u32
}

#[inline(always)]
fn threshold_ptr(context: usize) -> *mut u32 {
    (plic_base() + 0x200000 + context * 0x1000) as *mut u32
}

#[inline(always)]
fn claim_complete_ptr(context: usize) -> *mut u32 {
    // claim/complete is at threshold + 4
    (plic_base() + 0x200000 + context * 0x1000 + 4) as *mut u32
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
    #[cfg(feature = "platform-visionfive2")]
    {
        // JH7110 hart 0 is the E24 monitor core and has no S-mode context.
        // U74 hart 1 therefore uses context 2, hart 2 context 4, and so on.
        hart_id * 2
    }
    #[cfg(not(feature = "platform-visionfive2"))]
    {
        hart_id * 2 + 1
    }
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

/// Register one platform device interrupt with the bootstrap housekeeping hart.
///
/// Platform devices such as the JH7110 EQoS controller are discovered after
/// the PLIC global setup, but before per-hart contexts are enabled.
pub fn register_irq(irq: u32) {
    if irq == 0 || irq as usize >= MAX_IRQ_ID {
        warn!("ignoring out-of-range PLIC IRQ {}", irq);
        return;
    }
    set_irq_affinity_internal(irq, bootstrap_hart_id());
    set_priority(irq, 1);
}

/// 初始化 PLIC 的全局优先级配置。
///
/// 这部分只需要由 bootstrap hart 执行一次，不依赖具体 hart context。
pub fn init() {
    debug!("[kernel] Initializing PLIC...");
    let housekeeping_hart = bootstrap_hart_id();
    let uart_irq = uart_irq();
    set_irq_affinity_internal(uart_irq, housekeeping_hart);
    set_priority(uart_irq, 1);
    for irq in crate::boot::context::get()
        .devices()
        .virtio_mmio_devices()
        .iter()
        .filter_map(|resource| resource.irq)
    {
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
    let affinity = IRQ_AFFINITY.lock();
    for (irq, target_hart) in affinity.iter().copied().enumerate().skip(1) {
        if target_hart == usize::MAX {
            continue;
        }
        if target_hart == hart_id {
            enable_irq(context, irq as u32);
        } else {
            disable_irq(context, irq as u32);
        }
    }
    drop(affinity);
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
    if irq == uart_irq() {
        UART.handle_irq();
        // 把刚到达的输入立刻喂入控制台行规程：这样即便当前没有进程在 read，
        // Ctrl+C 等信号字符也能在到达瞬间生成信号投递给前台进程组。
        crate::fs::console_receive();
    } else if irq != 0 {
        let block_handled = crate::drivers::block::handle_irq(irq);
        let net_handled = crate::drivers::net::handle_irq(irq);
        if !block_handled && !net_handled {
            warn!("unhandled PLIC IRQ {}", irq);
        }
    }
    if irq != 0 {
        complete(context, irq);
    }
}
