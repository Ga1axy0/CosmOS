//! SBI call wrappers

#![allow(unused)]

use core::arch::asm;

/// SBI v0.2+ 调用返回值。
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SbiRet {
    /// SBI 规范定义的错误码。
    pub error: isize,
    /// SBI 调用的附加返回值。
    pub value: usize,
}

/// SBI HSM hart 状态。
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HartState {
    /// hart 已经启动。
    Started,
    /// hart 当前处于 stopped，可由 HSM 启动。
    Stopped,
    /// hart 正在启动流程中。
    StartPending,
    /// hart 正在停止流程中。
    StopPending,
    /// hart 已挂起。
    Suspended,
    /// hart 正在进入挂起。
    SuspendPending,
    /// hart 正在恢复。
    ResumePending,
    /// 非标准或当前未知状态。
    Unknown(usize),
}

/// set timer sbi call id (legacy SBI, qemu7)
#[cfg(qemu7)]
const SBI_SET_TIMER: usize = 0;
/// shutdown sbi call id (legacy SBI, qemu7)
#[cfg(qemu7)]
const SBI_SHUTDOWN: usize = 8;
/// console putchar sbi call id
const SBI_CONSOLE_PUTCHAR: usize = 1;
/// console getchar sbi call id
const SBI_CONSOLE_GETCHAR: usize = 2;

#[cfg(not(qemu7))]
const SBI_SET_TIMER: usize = 0x5449_4D45; // "TIME"
#[cfg(not(qemu7))]
const SBI_SHUTDOWN: usize = 0x5352_5354; // "SRST"
const SBI_HSM: usize = 0x0048_534D; // "HSM"
#[cfg(qemu7)]
const SBI_SEND_IPI: usize = 4;
#[cfg(not(qemu7))]
const SBI_IPI: usize = 0x0073_5049; // "sPI"

/// general sbi call
#[cfg(qemu7)]
#[inline(always)]
fn sbi_call_legacy(which: usize, arg0: usize, arg1: usize, arg2: usize) -> usize {
    let mut ret;
    unsafe {
        asm!(
            "ecall",     // sbi call
            inlateout("x10") arg0 => ret, // sbi call arg0 and return value
            in("x11") arg1, // sbi call arg1
            in("x12") arg2, // sbi call arg2
            in("x16") 0, // for sbi call id args need 2 reg (x16, x17)
            in("x17") which,// sbi call id
        );
    }
    ret
}

/// 通用 SBI v0.2+ 扩展调用。
#[inline(always)]
fn sbi_call(extension: usize, function: usize, arg0: usize, arg1: usize, arg2: usize) -> SbiRet {
    let error: usize;
    let value: usize;
    unsafe {
        asm!(
            "ecall",
            inlateout("x10") arg0 => error,
            inlateout("x11") arg1 => value,
            in("x12") arg2,
            in("x16") function,
            in("x17") extension,
        );
    }
    SbiRet {
        error: error as isize,
        value,
    }
}

/// use sbi call to set timer
#[cfg(qemu7)]
pub fn set_timer(timer: usize) {
    sbi_call_legacy(SBI_SET_TIMER, timer, 0, 0);
}

/// use sbi call to putchar in console (qemu uart handler)
#[cfg(not(qemu7))]
pub fn set_timer(timer: usize) {
    let _ = sbi_call(SBI_SET_TIMER, 0, timer, 0, 0);
}

/// use sbi call to putchar in console (qemu uart handler)
#[cfg(qemu7)]
pub fn console_putchar(c: usize) {
    sbi_call_legacy(SBI_CONSOLE_PUTCHAR, c, 0, 0);
}

/// use sbi call to getchar from console (qemu uart handler)
#[cfg(not(qemu7))]
pub fn console_putchar(c: usize) {
    let _ = sbi_call(SBI_CONSOLE_PUTCHAR, 0, c, 0, 0);
}

/// use sbi call to getchar from console (qemu uart handler)
#[cfg(qemu7)]
pub fn console_getchar() -> usize {
    sbi_call_legacy(SBI_CONSOLE_GETCHAR, 0, 0, 0)
}

/// use sbi call to shutdown the kernel
#[cfg(not(qemu7))]
pub fn console_getchar() -> usize {
    sbi_call(SBI_CONSOLE_GETCHAR, 0, 0, 0, 0).value
}

/// use sbi call to shutdown the kernel
#[cfg(qemu7)]
pub fn shutdown() -> ! {
    sbi_call_legacy(SBI_SHUTDOWN, 0, 0, 0);
    panic!("It should shutdown!");
}

/// use sbi call to shutdown the kernel
#[cfg(not(qemu7))]
pub fn shutdown() -> ! {
    let _ = sbi_call(SBI_SHUTDOWN, 0, 0, 0, 0);
    panic!("It should shutdown!");
}

/// 发送 IPI 到给定 hart mask。
#[cfg(qemu7)]
pub fn send_ipi_mask(hart_mask: usize) {
    let hart_mask_ptr = &hart_mask as *const usize as usize;
    sbi_call_legacy(SBI_SEND_IPI, hart_mask_ptr, 0, 0);
}

/// 发送 IPI 到给定 hart mask。
#[cfg(not(qemu7))]
pub fn send_ipi_mask(hart_mask: usize) {
    let _ = sbi_call(SBI_IPI, 0, hart_mask, 0, 0);
}

/// 查询指定 hart 的 HSM 状态。
pub fn hart_get_status(hart_id: usize) -> SbiRet {
    sbi_call(SBI_HSM, 2, hart_id, 0, 0)
}

/// 请求启动指定 hart，并让它从 `start_addr` 开始执行。
pub fn hart_start(hart_id: usize, start_addr: usize, opaque: usize) -> SbiRet {
    sbi_call(SBI_HSM, 0, hart_id, start_addr, opaque)
}

/// 将 HSM 原始状态值转换为可读枚举。
pub fn hart_state(raw: usize) -> HartState {
    match raw {
        0 => HartState::Started,
        1 => HartState::Stopped,
        2 => HartState::StartPending,
        3 => HartState::StopPending,
        4 => HartState::Suspended,
        5 => HartState::SuspendPending,
        6 => HartState::ResumePending,
        other => HartState::Unknown(other),
    }
}
