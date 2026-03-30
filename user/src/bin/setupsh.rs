#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::ptr;

use user_lib::{exec, exit, fork, link, mkdir, unlink, waitpid};

const EEXIST: isize = -17;
const ENOENT: isize = -2;

const BIN_DIR: &str = "/bin";
const BIN_DIR_CSTR: &str = "/bin\0";
const BIN_SH_CSTR: &str = "/bin/sh\0";
const ROOT_BUSYBOX: &str = "/busybox";
const BUSYBOX_PATH_CSTR: &str = "/musl/busybox\0";
const BUSYBOX_ARGV0_CSTR: &str = "/musl/busybox\0";
const INSTALL_ARG_CSTR: &str = "--install\0";
const PROC_DIR: &str = "/proc";
const PROC_SELF_DIR: &str = "/proc/self";
const PROC_SELF_EXE: &str = "/proc/self/exe";

/// 运行一个外部程序并等待其退出。
fn spawn_and_wait(path: &str, argv: &[*const u8]) -> i32 {
    let pid = fork();
    if pid < 0 {
        println!("[setupsh] fork failed for {}", path);
        return -1;
    }
    if pid == 0 {
        let ret = exec(path, argv);
        println!("[setupsh] exec {} failed: {}", path, ret);
        exit(127);
    }

    let mut exit_code = 0i32;
    let waited = waitpid(pid as usize, &mut exit_code);
    if waited != pid {
        println!(
            "[setupsh] waitpid mismatch: expected {}, got {}",
            pid, waited
        );
        return -1;
    }
    exit_code
}

/// 打印阶段进度，便于观察 `setupsh` 当前执行到哪一步。
fn print_step(step: usize, total: usize, message: &str) {
    println!("[setupsh {}/{}] {}", step, total, message);
}

/// 创建目录，若目录已存在则视为成功。
fn ensure_dir(path: &str) -> bool {
    let ret = mkdir(path, 0o755);
    if ret == 0 || ret == EEXIST {
        return true;
    }
    println!("[setupsh] mkdir {} failed: {}", path, ret);
    false
}

/// 用硬链接创建一个稳定入口；若目标已存在则先删除再重建。
fn ensure_hard_link(src: &str, dst: &str) -> bool {
    let unlink_ret = unlink(dst);
    if unlink_ret != 0 && unlink_ret != ENOENT {
        println!("[setupsh] unlink {} failed: {}", dst, unlink_ret);
        return false;
    }

    let link_ret = link(src, dst);
    if link_ret != 0 {
        println!("[setupsh] link {} -> {} failed: {}", dst, src, link_ret);
        return false;
    }
    true
}

/// 建立临时 `/proc/self/exe` 入口，供 BusyBox/ash 重执行自身时复用。
fn ensure_proc_self_exe() -> bool {
    if !ensure_dir(PROC_DIR) || !ensure_dir(PROC_SELF_DIR) {
        return false;
    }

    // TODO: 这里只是临时用硬链接伪装 `/proc/self/exe`，并不具备真正 procfs 的动态语义。
    ensure_hard_link(ROOT_BUSYBOX, PROC_SELF_EXE)
}

#[no_mangle]
fn main() -> i32 {
    const TOTAL_STEPS: usize = 5;

    println!("[setupsh] start");
    print_step(1, TOTAL_STEPS, "prepare /bin");
    if !ensure_dir(BIN_DIR) {
        return 1;
    }

    print_step(2, TOTAL_STEPS, "install busybox applets into /bin");
    let install_argv = [
        BUSYBOX_ARGV0_CSTR.as_ptr(),
        INSTALL_ARG_CSTR.as_ptr(),
        BIN_DIR_CSTR.as_ptr(),
        ptr::null(),
    ];
    let install_exit = spawn_and_wait(BUSYBOX_PATH_CSTR, &install_argv);
    if install_exit != 0 {
        // TODO: 若后续需要兼容 `--install` 的部分成功场景，可在这里补充更细的降级策略。
        println!("[setupsh] busybox --install failed: {}", install_exit);
        return install_exit;
    }

    print_step(3, TOTAL_STEPS, "create /busybox hard link");
    if !ensure_hard_link(BUSYBOX_PATH_CSTR.trim_end_matches('\0'), ROOT_BUSYBOX) {
        return 1;
    }

    print_step(4, TOTAL_STEPS, "prepare temporary /proc/self/exe");
    if !ensure_proc_self_exe() {
        return 1;
    }

    print_step(5, TOTAL_STEPS, "launch /bin/sh");
    let shell_argv = [BIN_SH_CSTR.as_ptr(), ptr::null()];
    let shell_exit = spawn_and_wait(BIN_SH_CSTR, &shell_argv);
    println!("[setupsh] /bin/sh exited with {}", shell_exit);
    shell_exit
}
