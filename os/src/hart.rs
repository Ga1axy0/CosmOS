//! hart-local 辅助接口。
//!
//! 在 SMP A 阶段，内核态约定将 `tp` 用于保存当前 hart 的本地状态。
//! 用户态可能会把 `tp` 当作 TLS 指针使用，因此 trap 边界会负责在
//! 用户/内核切换时保存和恢复两边各自的 `tp` 语义。后续模块统一通过
//! 这里提供的接口获取当前 hart 信息，而不是在各处直接读取 CSR。

use core::arch::asm;

pub use crate::task::current_processor;

/// 使用当前已经保存在 `tp` 中的 hart id 完成初始化接口兼容。
pub fn init() -> usize {
    hartid()
}

/// 使用启动阶段已经得到的 hart id 初始化 hart-local 寄存器。
///
/// 这用于 RustSBI / HSM 已经通过 `a0` 把 hart id 传给 Rust 入口的场景，
/// 避免在 Rust 中再额外依赖某个特定 CSR 读取路径。
pub fn init_with_hartid(hart_id: usize) -> usize {
    unsafe { write_tp(hart_id) };
    hart_id
}

/// 从 `tp` 中读取当前 hart id。
#[inline]
pub fn hartid() -> usize {
    let hart_id;
    unsafe {
        asm!("mv {}, tp", out(reg) hart_id);
    }
    hart_id
}

#[inline]
unsafe fn write_tp(hart_id: usize) {
    asm!("mv tp, {}", in(reg) hart_id);
}
