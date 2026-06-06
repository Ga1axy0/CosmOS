//! The main module and entrypoint
//!
//! Various facilities of the kernels are implemented as submodules. The most
//! important ones are:
//!
//! - [`trap`]: Handles all cases of switching from userspace to the kernel
//! - [`task`]: Task management
//! - [`syscall`]: System call handling and implementation
//! - [`mm`]: Address map using SV39
//! - [`sync`]: Wrap a static data structure inside it so that we are able to access it without any `unsafe`.
//! - [`fs`]: Separate user from file system with some structures
//!
//! The operating system also starts in this module. Kernel code starts
//! executing from `entry.asm`, after which [`rust_main()`] is called to
//! initialize various pieces of functionality. (See its source code for
//! details.)
//!
//! We then call [`sched::run_tasks()`] and for the first time go to
//! userspace.

#![deny(missing_docs)]
// #![deny(warnings)]
#![no_std]
#![no_main]
#![feature(panic_info_message)]
#![feature(alloc_error_handler)]

#[macro_use]
extern crate log;

extern crate alloc;

#[macro_use]
extern crate bitflags;

#[path = "boards/qemu.rs"]
mod board;

#[macro_use]
mod console;
pub mod config;
pub mod drivers;
pub mod fs;
pub mod hart;
pub mod ipc;
pub mod keys;
pub mod lang_items;
pub mod klog;
pub mod mm;
pub mod net;
pub mod signal;
mod poll;
pub mod sbi;
pub mod sync;
pub mod syscall;
pub mod task;
pub mod timer;
pub mod random;
pub mod sched;
pub mod trap;

use core::arch::global_asm;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

global_asm!(include_str!("entry.asm"));

/// secondary hart 在访问 `.bss` 中的全局对象前，必须先等 bootstrap hart 完成 `clear_bss()`。
///
/// 这里故意使用非零初值，把它放进 `.data` 而不是 `.bss`，这样 secondary hart
/// 在 bootstrap hart 清空 `.bss` 之前也能安全地轮询它。
static BOOT_BSS_READY: AtomicUsize = AtomicUsize::new(usize::MAX);
static BOOTSTRAP_HART_ID: AtomicUsize = AtomicUsize::new(usize::MAX);
static BOOT_DONE: AtomicBool = AtomicBool::new(false);

/// 返回负责一次性全局初始化的 bootstrap hart id。
///
/// 在 bootstrap hart 选举完成前返回 `usize::MAX`；正常调度阶段调用时，
/// 该值已经稳定，可作为 housekeeping hart 的选择依据。
pub fn bootstrap_hart_id() -> usize {
    BOOTSTRAP_HART_ID.load(Ordering::Acquire)
}

/// 清空 `.bss` 段，保证未初始化的全局/静态数据从 0 开始。
fn clear_bss() {
    extern "C" {
        fn sbss();
        fn ebss();
    }
    unsafe {
        core::slice::from_raw_parts_mut(sbss as usize as *mut u8, ebss as usize - sbss as usize)
            .fill(0);
    }
}

/// 完成当前 hart 的本地初始化。
///
/// 这里只放“每个 hart 都需要各自执行一次”的初始化项，
/// 不包含内存、文件系统、驱动探测这类全局一次性初始化。
fn init_local_hart(hart_id: usize) {
    trap::init_hart();
    timer::init_hart();
    drivers::plic::init_hart(hart_id);
    mm::mark_online(hart_id);
    debug!("hart {} local init done", hart_id);
}

/// 记录当前环境下各 hart 的 HSM 状态，并尝试拉起处于 stopped 状态的 hart。
///
/// 这里的目标不是“盲目对所有 hart 重复 `hart_start`”，而是先看清固件报告的
/// 状态，再只对明确处于 `Stopped` 的 hart 发起启动请求。
fn probe_and_start_other_harts(bootstrap_hart_id: usize) {
    extern "C" {
        fn _start();
    }

    const SBI_SUCCESS: isize = 0;
    const SBI_ERR_INVALID_PARAM: isize = -3;
    const SBI_ERR_ALREADY_AVAILABLE: isize = -6;

    info!(
        "hart {} entering HSM probe/start loop",
        bootstrap_hart_id
    );

    for target_hart in 0..config::MAX_HARTS {
        let status = sbi::hart_get_status(target_hart);
        if status.error == SBI_ERR_INVALID_PARAM {
            info!(
                "hart {} got invalid hart id while probing hart {}, stop scan",
                bootstrap_hart_id, target_hart
            );
            break;
        }
        if status.error != SBI_SUCCESS {
            info!(
                "hart {} HSM status query for hart {} failed: error={}, value={}",
                bootstrap_hart_id, target_hart, status.error, status.value
            );
            continue;
        }

        let state = sbi::hart_state(status.value);
        info!(
            "hart {} sees hart {} in HSM state {:?}",
            bootstrap_hart_id, target_hart, state
        );

        if target_hart == bootstrap_hart_id {
            continue;
        }

        if let sbi::HartState::Stopped = state {
            let ret = sbi::hart_start(target_hart, _start as usize, 0);
            match ret.error {
                SBI_SUCCESS => info!(
                    "hart {} requested startup for hart {}",
                    bootstrap_hart_id, target_hart
                ),
                SBI_ERR_ALREADY_AVAILABLE => info!(
                    "hart {} found hart {} already available while starting",
                    bootstrap_hart_id, target_hart
                ),
                error => info!(
                    "hart {} failed to start hart {}: error={}, value={}",
                    bootstrap_hart_id, target_hart, error, ret.value
                ),
            }
        }
    }
}

/// 竞争并记录负责一次性全局初始化的 bootstrap hart。
///
/// 返回值为 `true` 表示当前 hart 抢到了 bootstrap 角色；返回 `false`
/// 表示 bootstrap 角色已经被其他 hart 占用，当前 hart 应按 secondary
/// 路径继续执行。
fn try_claim_bootstrap_hart(hart_id: usize) -> bool {
    BOOTSTRAP_HART_ID
        .compare_exchange(usize::MAX, hart_id, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// 等待 bootstrap hart 完成一次性全局初始化。
///
/// secondary hart 只有在 `BOOT_DONE` 置位后才能继续访问全局对象，
/// 以避免在内存管理、文件系统等尚未完成时过早上线。
fn wait_for_bootstrap() {
    while BOOT_BSS_READY.load(Ordering::Acquire) != 0 {
        spin_loop();
    }
    while !BOOT_DONE.load(Ordering::Acquire) {
        spin_loop();
    }
}

/// bootstrap hart 的主入口
fn first_hart_main(hart_id: usize) -> ! {
    clear_bss();
    BOOT_BSS_READY.store(0, Ordering::Release);
    mm::init();
    mm::remap_test();
    klog::init();
    info!("hart {} boot", hart_id);
    info!("hart {} elected as bootstrap hart", hart_id);
    drivers::init();
    net::init();
    fs::init();
    timer::init_realtime_offset_from_rtc();
    probe_and_start_other_harts(hart_id);
    init_local_hart(hart_id);
    task::add_initproc();
    BOOT_DONE.store(true, Ordering::Release);
    println!("[kernel] Hello, world!");
    info!("hart {} entered scheduler", hart_id);
    sched::run_tasks();
    panic!("Unreachable in rust_main!");
}

/// secondary hart 的主入口。
///
/// 在 bootstrap hart 完成全局初始化后，secondary hart 完成本地初始化
/// 并加入全局调度器，参与任务执行。
fn secondary_hart_main(hart_id: usize) -> ! {
    wait_for_bootstrap();
    mm::activate_kernel_space();    // 激活内核页表：但 satp 是 per-hart 寄存器
    info!("hart {} boot", hart_id);
    init_local_hart(hart_id);
    debug!("hart {} entered scheduler", hart_id);
    sched::run_tasks();
    panic!("Unreachable in secondary_hart_main!");
}

#[no_mangle]
/// 内核的 Rust 入口。
///
/// 第一个进入该入口的 hart 会成为 bootstrap hart，负责一次性全局初始化
/// 并进入调度器；其他 hart 等待 bootstrap 完成后只做本地初始化并进入 idle。
pub fn rust_main(hart_id: usize) -> ! {
    let _hart_id = hart::init_with_hartid(hart_id);
    unsafe { 
        riscv::register::sstatus::set_fs(riscv::register::mstatus::FS::Initial); 
    }
    if !try_claim_bootstrap_hart(hart_id) {
        secondary_hart_main(hart_id);
    } else {
        first_hart_main(hart_id);
    }
}
