#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::String;
use core::ptr;

use user_lib::{
    close, exec, exit, fork, fstatat, getdents64, link, mkdir, open, unlink, waitpid, write,
    OpenFlags, Stat,
};

const EEXIST: isize = -17;
const ENOENT: isize = -2;

const BIN_DIR: &str = "/bin";
const BIN_DIR_CSTR: &str = "/bin\0";
const BIN_BASH: &str = "/bin/bash";
const BIN_BUSYBOX: &str = "/bin/busybox";
const BIN_BUSYBOX_CSTR: &str = "/bin/busybox\0";
const LIB_DIR: &str = "/lib";
const ETC_DIR: &str = "/etc";
const USR_DIR: &str = "/usr";
const USR_BIN_DIR: &str = "/usr/bin";
const USR_BIN_DIR_CSTR: &str = "/usr/bin\0";
const USR_LIB_DIR: &str = "/usr/lib";
const BIN_SH_CSTR: &str = "/bin/sh\0";
const BUSYBOX_ARG0_CSTR: &str = "busybox\0";
const ROOT_BASH: &str = "/bash";
const ROOT_BUSYBOX: &str = "/busybox";
const MUSL_BUSYBOX_PATH: &str = "/musl/busybox";
const MUSL_LEGACY_LIB_DIR: &str = "/musl/lib";
const MUSL_LIB_DIR: &str = "/usr/lib/riscv64-linux-musl";
const MUSL_LIBC_PATH: &str = "/usr/lib/riscv64-linux-musl/libc.so";
const MUSL_LD_PATH: &str = "/lib/ld-musl-riscv64-sf.so.1";
const MUSL_LD_CONFIG_PATH: &str = "/etc/ld-musl-riscv64-sf.path";
const MUSL_LD_CONFIG_CONTENT: &[u8] = b"/usr/lib/riscv64-linux-musl\n/lib\n";
const GLIBC_BUSYBOX_PATH: &str = "/glibc/busybox";
const GLIBC_BUSYBOX_TARGET: &str = "/usr/bin/glibc-busybox";
const GLIBC_BUSYBOX_TARGET_CSTR: &str = "/usr/bin/glibc-busybox\0";
const GLIBC_LEGACY_LIB_DIR: &str = "/glibc/lib";
const GLIBC_LIB_DIR: &str = "/lib/riscv64-linux-gnu";
const GLIBC_USR_LIB_DIR: &str = "/usr/lib/riscv64-linux-gnu";
const GLIBC_LD_NAME: &str = "ld-linux-riscv64-lp64d.so.1";
const GLIBC_LD_TARGET: &str = "/lib/riscv64-linux-gnu/ld-linux-riscv64-lp64d.so.1";
const GLIBC_LD_PATH: &str = "/lib/ld-linux-riscv64-lp64d.so.1";
const INSTALL_ARG_CSTR: &str = "--install\0";
const DENTS_BUF_SIZE: usize = 4096;
const DT_DIR: u8 = 4;

fn path_exists(path: &str) -> bool {
    let mut st = Stat::new();
    fstatat(user_lib::AT_FDCWD, path, &mut st, 0) == 0
}

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
    if path_exists(path) {
        return true;
    }
    let ret = mkdir(path, 0o755);
    if ret == 0 || ret == EEXIST {
        return true;
    }
    println!("[setupsh] mkdir {} failed: {}", path, ret);
    false
}

/// 用硬链接创建一个稳定入口；若目标已存在则先删除再重建。
fn ensure_hard_link(src: &str, dst: &str) -> bool {
    if src == dst {
        return true;
    }
    if path_exists(dst) {
        let unlink_ret = unlink(dst);
        if unlink_ret != 0 && unlink_ret != ENOENT {
            println!("[setupsh] unlink {} failed: {}", dst, unlink_ret);
            return false;
        }
    }
    let link_ret = link(src, dst);
    if link_ret == 0 || link_ret == EEXIST {
        return true;
    }
    println!("[setupsh] link {} -> {} failed: {}", dst, src, link_ret);
    false
}

/// 创建一个可选兼容入口；已存在则保持原状。
fn optional_hard_link(src: &str, dst: &str) {
    if src == dst || path_exists(dst) {
        return;
    }
    let link_ret = link(src, dst);
    if link_ret == 0 || link_ret == EEXIST || link_ret == ENOENT {
        return;
    }
    println!(
        "[setupsh] optional link {} -> {} failed: {}",
        dst, src, link_ret
    );
}

fn remove_if_exists(path: &str) -> bool {
    if !path_exists(path) {
        return true;
    }
    let unlink_ret = unlink(path);
    if unlink_ret != 0 && unlink_ret != ENOENT {
        println!("[setupsh] unlink {} failed: {}", path, unlink_ret);
        return false;
    }
    true
}

/// 拼接目录与文件名，返回完整路径。
fn join_path(dir: &str, name: &str) -> String {
    let mut path = String::from(dir);
    if !path.ends_with('/') {
        path.push('/');
    }
    path.push_str(name);
    path
}

/// 安装一条同名运行库硬链接。
fn install_lib_link(src: &str, dst: &str) -> bool {
    ensure_hard_link(src, dst)
}

/// 目标目录中已有普通文件时，认为镜像已迁移过。
fn dir_has_runtime_files(target_dir: &str) -> bool {
    let fd = open(target_dir, OpenFlags::RDONLY | OpenFlags::DIRECTORY);
    if fd >= 0 {
        let mut buf = [0u8; DENTS_BUF_SIZE];
        let nread = getdents64(fd as usize, &mut buf);
        close(fd as usize);
        if nread <= 0 {
            return false;
        }
        let mut pos = 0usize;
        let nread = nread as usize;
        while pos + 19 <= nread {
            let reclen = u16::from_le_bytes([buf[pos + 16], buf[pos + 17]]) as usize;
            if reclen == 0 || pos + reclen > nread {
                break;
            }
            let dtype = buf[pos + 18];
            let name_field = &buf[pos + 19..pos + reclen];
            let name_len = name_field
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_field.len());
            if name_len > 0 {
                if let Ok(name) = core::str::from_utf8(&name_field[..name_len]) {
                    if name != "." && name != ".." && dtype != DT_DIR {
                        return true;
                    }
                }
            }
            pos += reclen;
        }
    }
    false
}

/// 扫描旧运行库目录，并把文件硬链接到 Linux multiarch 目标目录。
fn install_runtime_libs(src_dir: &str, dst_dir: &str) -> bool {
    let target_already_populated = dir_has_runtime_files(dst_dir);
    if !ensure_dir(dst_dir) {
        return false;
    }
    let fd = open(src_dir, OpenFlags::RDONLY | OpenFlags::DIRECTORY);
    if fd < 0 {
        if fd == ENOENT && target_already_populated {
            println!("[setupsh] {} missing, using existing {}", src_dir, dst_dir);
            return true;
        }
        println!("[setupsh] open {} failed: {}", src_dir, fd);
        return false;
    }

    let mut buf = [0u8; DENTS_BUF_SIZE];
    loop {
        let nread = getdents64(fd as usize, &mut buf);
        if nread < 0 {
            println!("[setupsh] getdents64 {} failed: {}", src_dir, nread);
            close(fd as usize);
            return false;
        }
        if nread == 0 {
            break;
        }

        let mut pos = 0usize;
        let nread = nread as usize;
        while pos + 19 <= nread {
            let reclen = u16::from_le_bytes([buf[pos + 16], buf[pos + 17]]) as usize;
            if reclen == 0 || pos + reclen > nread {
                break;
            }
            let dtype = buf[pos + 18];
            let name_field = &buf[pos + 19..pos + reclen];
            let name_len = name_field
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_field.len());
            if name_len > 0 {
                if let Ok(name) = core::str::from_utf8(&name_field[..name_len]) {
                    if name != "." && name != ".." && dtype != DT_DIR {
                        let src = join_path(src_dir, name);
                        let dst = join_path(dst_dir, name);
                        if !install_lib_link(src.as_str(), dst.as_str()) {
                            close(fd as usize);
                            return false;
                        }
                    }
                }
            }
            pos += reclen;
        }
    }
    close(fd as usize);
    true
}

fn keep_top_level_lib_entry(name: &str, dtype: u8) -> bool {
    dtype == DT_DIR
        || name == "."
        || name == ".."
        || name == MUSL_LD_PATH.trim_start_matches('/')
        || name == GLIBC_LD_PATH.trim_start_matches('/')
}

/// 清理旧版 setupsh 曾经铺到 `/lib` 顶层的普通运行库链接。
fn clean_top_level_lib() -> bool {
    let fd = open(LIB_DIR, OpenFlags::RDONLY | OpenFlags::DIRECTORY);
    if fd < 0 {
        println!("[setupsh] open {} failed: {}", LIB_DIR, fd);
        return false;
    }

    let mut buf = [0u8; DENTS_BUF_SIZE];
    loop {
        let nread = getdents64(fd as usize, &mut buf);
        if nread < 0 {
            println!("[setupsh] getdents64 {} failed: {}", LIB_DIR, nread);
            close(fd as usize);
            return false;
        }
        if nread == 0 {
            break;
        }

        let mut pos = 0usize;
        let nread = nread as usize;
        while pos + 19 <= nread {
            let reclen = u16::from_le_bytes([buf[pos + 16], buf[pos + 17]]) as usize;
            if reclen == 0 || pos + reclen > nread {
                break;
            }
            let dtype = buf[pos + 18];
            let name_field = &buf[pos + 19..pos + reclen];
            let name_len = name_field
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_field.len());
            if name_len > 0 {
                if let Ok(name) = core::str::from_utf8(&name_field[..name_len]) {
                    if !keep_top_level_lib_entry(name, dtype) {
                        let path = join_path(LIB_DIR, name);
                        if !remove_if_exists(path.as_str()) {
                            close(fd as usize);
                            return false;
                        }
                    }
                }
            }
            pos += reclen;
        }
    }
    close(fd as usize);
    true
}

fn ensure_dirs() -> bool {
    for dir in [
        BIN_DIR,
        LIB_DIR,
        ETC_DIR,
        USR_DIR,
        USR_BIN_DIR,
        USR_LIB_DIR,
        MUSL_LIB_DIR,
        GLIBC_LIB_DIR,
        GLIBC_USR_LIB_DIR,
    ] {
        if !ensure_dir(dir) {
            return false;
        }
    }
    true
}

fn write_file(path: &str, content: &[u8]) -> bool {
    let fd = open(
        path,
        OpenFlags::WRONLY | OpenFlags::CREATE | OpenFlags::TRUNC,
    );
    if fd < 0 {
        println!("[setupsh] open {} for write failed: {}", path, fd);
        return false;
    }
    let ret = write(fd as usize, content);
    let close_ret = close(fd as usize);
    if ret != content.len() as isize {
        println!("[setupsh] write {} failed: {}", path, ret);
        return false;
    }
    if close_ret != 0 {
        println!("[setupsh] close {} failed: {}", path, close_ret);
        return false;
    }
    true
}

fn install_loader_links() -> bool {
    if !ensure_hard_link(MUSL_LIBC_PATH, MUSL_LD_PATH) {
        return false;
    }
    if !ensure_hard_link(GLIBC_LD_TARGET, GLIBC_LD_PATH) {
        println!(
            "[setupsh] glibc loader must exist as {} in {}",
            GLIBC_LD_NAME, GLIBC_LIB_DIR
        );
        return false;
    }
    true
}

fn install_busybox_entries() -> bool {
    if !ensure_hard_link(MUSL_BUSYBOX_PATH, BIN_BUSYBOX) {
        return false;
    }
    if !ensure_hard_link(GLIBC_BUSYBOX_PATH, GLIBC_BUSYBOX_TARGET) {
        return false;
    }
    optional_hard_link(BIN_BUSYBOX, ROOT_BUSYBOX);
    optional_hard_link(ROOT_BASH, BIN_BASH);
    true
}

fn install_busybox_applets() -> bool {
    if !path_exists("/bin/sh") {
        let musl_install_argv = [
            BUSYBOX_ARG0_CSTR.as_ptr(),
            INSTALL_ARG_CSTR.as_ptr(),
            BIN_DIR_CSTR.as_ptr(),
            ptr::null(),
        ];
        let install_exit = spawn_and_wait(BIN_BUSYBOX_CSTR, &musl_install_argv);
        if install_exit != 0 {
            println!("[setupsh] musl busybox --install failed: {}", install_exit);
            return false;
        }
    }

    if !path_exists("/usr/bin/sh") {
        let glibc_install_argv = [
            BUSYBOX_ARG0_CSTR.as_ptr(),
            INSTALL_ARG_CSTR.as_ptr(),
            USR_BIN_DIR_CSTR.as_ptr(),
            ptr::null(),
        ];
        let install_exit = spawn_and_wait(GLIBC_BUSYBOX_TARGET_CSTR, &glibc_install_argv);
        if install_exit != 0 {
            println!("[setupsh] glibc busybox --install failed: {}", install_exit);
            return false;
        }
    }
    true
}

#[no_mangle]
fn main(_argc: usize, argv: &[&str]) -> i32 {
    const TOTAL_STEPS: usize = 8;
    if let Some(arg) = argv.get(1) {
        println!(
            "[setupsh] ignoring legacy libc selector '{}'; installing musl and glibc",
            arg
        );
    }

    println!("[setupsh] start Linux-style libc layout setup");
    print_step(1, TOTAL_STEPS, "prepare Linux-style directories");
    if !ensure_dirs() {
        return 1;
    }

    print_step(
        2,
        TOTAL_STEPS,
        "install musl runtime into /usr/lib/riscv64-linux-musl",
    );
    if !install_runtime_libs(MUSL_LEGACY_LIB_DIR, MUSL_LIB_DIR) {
        return 1;
    }

    print_step(
        3,
        TOTAL_STEPS,
        "install glibc runtime into /lib/riscv64-linux-gnu",
    );
    if !install_runtime_libs(GLIBC_LEGACY_LIB_DIR, GLIBC_LIB_DIR) {
        return 1;
    }

    print_step(4, TOTAL_STEPS, "clean stale top-level /lib runtime links");
    if !clean_top_level_lib() {
        return 1;
    }

    print_step(5, TOTAL_STEPS, "install PT_INTERP loader links");
    if !install_loader_links() {
        return 1;
    }

    print_step(6, TOTAL_STEPS, "write musl loader search path");
    if !write_file(MUSL_LD_CONFIG_PATH, MUSL_LD_CONFIG_CONTENT) {
        return 1;
    }

    print_step(7, TOTAL_STEPS, "install busybox entries and applets");
    if !install_busybox_entries() || !install_busybox_applets() {
        return 1;
    }

    print_step(8, TOTAL_STEPS, "launch /bin/sh");
    let shell_argv = [BIN_SH_CSTR.as_ptr(), ptr::null()];
    let shell_exit = spawn_and_wait(BIN_SH_CSTR, &shell_argv);
    println!("[setupsh] /bin/sh exited with {}", shell_exit);
    shell_exit
}
