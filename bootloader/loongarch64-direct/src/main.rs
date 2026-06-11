#![no_std]
#![no_main]
use core::arch::{asm, naked_asm};
use core::panic::PanicInfo;

const UART_BASE: usize = 0x1fe0_01e0;
const IO_OFFSET: usize = 0x8000_0000_0000_0000;
const UART_THR: usize = 0x00;
const UART_LSR: usize = 0x05;
const UART_LSR_THRE: u8 = 1 << 5;
const KERNEL_ENTRY: usize = 0x9000_0000_9000_0000;

#[unsafe(no_mangle)]
static mut BOOT_STACK: [u8; 4096] = [0; 4096];

#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start() -> ! {
    naked_asm!(
        // Setup DMW0: UC window 0x8000xxxxxxxxxxxx → PLV0
        "ori   $t0, $zero, 0x1",
        "lu52i.d $t0, $t0, -2048",
        "csrwr $t0, 0x180",
        // Setup DMW1: CA window 0x9000xxxxxxxxxxxx → PLV0
        "ori   $t0, $zero, 0x11",
        "lu52i.d $t0, $t0, -1792",
        "csrwr $t0, 0x181",
        // Setup stack
        "la.global $t0, {stack}",
        "ori   $t1, $zero, 2048",
        "add.d $sp, $t0, $t1",
        "add.d $sp, $sp, $t1",
        "csrrd $a0, 0x20",
        "b     {main}",
        stack = sym BOOT_STACK,
        main = sym boot_main,
    )
}

#[unsafe(no_mangle)]
extern "C" fn boot_main(hart_id: usize) -> ! {
    if hart_id == 0 {
        puts("[boot] loongarch64 direct loader\r\n");
        puts("[boot] jumping to kernel @ 0x90000000\r\n");
    }
    unsafe {
        let entry: extern "C" fn(usize) -> ! = core::mem::transmute(KERNEL_ENTRY);
        entry(hart_id);
    }
}

fn puts(s: &str) {
    for byte in s.bytes() {
        putc(byte);
    }
}

fn putc(byte: u8) {
    unsafe {
        let uart = IO_OFFSET | UART_BASE;
        while (mmio_read8(uart + UART_LSR) & UART_LSR_THRE) == 0 {}
        mmio_write8(uart + UART_THR, byte);
    }
}

unsafe fn mmio_read8(addr: usize) -> u8 {
    core::ptr::read_volatile(addr as *const u8)
}

unsafe fn mmio_write8(addr: usize, value: u8) {
    core::ptr::write_volatile(addr as *mut u8, value)
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    puts("[boot] panic\r\n");
    loop {
        unsafe { asm!("idle 0", options(nomem, nostack)) };
    }
}
