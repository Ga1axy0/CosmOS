#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::ptr;

use user_lib::{exec, exit, fork, mkdir, waitpid};

const EEXIST: isize = -17;

const BIN_DIR: &str = "/bin";
const BIN_DIR_CSTR: &str = "/bin\0";
const BIN_SH_CSTR: &str = "/bin/sh\0";
const BUSYBOX_PATH_CSTR: &str = "/musl/busybox\0";
const BUSYBOX_ARGV0_CSTR: &str = "/musl/busybox\0";
const INSTALL_ARG_CSTR: &str = "--install\0";

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

/// 创建 `/bin`，若目录已存在则视为成功。
fn ensure_bin_dir() -> bool {
    let ret = mkdir(BIN_DIR, 0o755);
    if ret == 0 || ret == EEXIST {
        return true;
    }
    println!("[setupsh] mkdir {} failed: {}", BIN_DIR, ret);
    false
}

#[no_mangle]
fn main() -> i32 {
    if !ensure_bin_dir() {
        return 1;
    }

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

    let shell_argv = [BIN_SH_CSTR.as_ptr(), ptr::null()];
    let shell_exit = spawn_and_wait(BIN_SH_CSTR, &shell_argv);
    println!("[setupsh] /bin/sh exited with {}", shell_exit);
    shell_exit
}
