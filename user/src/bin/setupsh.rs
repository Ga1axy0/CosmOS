#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::String;
use core::ptr;

use user_lib::{OpenFlags, close, exec, exit, fork, getdents64, link, mkdir, open, unlink, waitpid};

const EEXIST: isize = -17;

const BIN_DIR: &str = "/bin";
const BIN_DIR_CSTR: &str = "/bin\0";
const BIN_BUSYBOX: &str = "/bin/busybox";
const LIB_DIR: &str = "/lib";
const BIN_SH_CSTR: &str = "/bin/sh\0";
const ROOT_BUSYBOX: &str = "/busybox";
const MUSL_BUSYBOX_PATH: &str = "/musl/busybox";
const MUSL_BUSYBOX_PATH_CSTR: &str = "/musl/busybox\0";
const MUSL_LIBC_PATH: &str = "/musl/lib/libc.so";
const MUSL_LD_PATH: &str = "/lib/ld-musl-riscv64-sf.so.1";
const GLIBC_BUSYBOX_PATH: &str = "/glibc/busybox";
const GLIBC_BUSYBOX_PATH_CSTR: &str = "/glibc/busybox\0";
const INSTALL_ARG_CSTR: &str = "--install\0";
const DENTS_BUF_SIZE: usize = 4096;
const DT_DIR: u8 = 4;

/// BusyBox 所属的 libc 版本。
#[derive(Copy, Clone)]
enum BusyBoxLibc {
    Musl,
    Glibc,
}

impl BusyBoxLibc {
    /// 从 `setupsh` 参数中选择 BusyBox 版本。
    fn from_args(argv: &[&str]) -> Option<Self> {
        match argv.get(1) {
            None => {
                println!("[setupsh] missing busybox libc, usage: setupsh [musl|glibc]");
                None
            }
            Some(&"musl") => Some(Self::Musl),
            Some(&"glibc") => Some(Self::Glibc),
            Some(arg) => {
                println!(
                    "[setupsh] unknown busybox libc '{}', usage: setupsh [musl|glibc]",
                    arg
                );
                None
            }
        }
    }

    /// 返回 BusyBox 的普通路径，供硬链接创建使用。
    fn busybox_path(self) -> &'static str {
        match self {
            Self::Musl => MUSL_BUSYBOX_PATH,
            Self::Glibc => GLIBC_BUSYBOX_PATH,
        }
    }

    /// 返回 BusyBox 的 C 字符串路径，供 exec 参数使用。
    fn busybox_path_cstr(self) -> &'static str {
        match self {
            Self::Musl => MUSL_BUSYBOX_PATH_CSTR,
            Self::Glibc => GLIBC_BUSYBOX_PATH_CSTR,
        }
    }

    /// 返回用于日志输出的 libc 名称。
    fn name(self) -> &'static str {
        match self {
            Self::Musl => "musl",
            Self::Glibc => "glibc",
        }
    }

    /// 返回运行库所在目录。
    fn lib_dir(self) -> &'static str {
        match self {
            Self::Musl => "/musl/lib",
            Self::Glibc => "/glibc/lib",
        }
    }
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
    let ret = mkdir(path, 0o755);
    if ret == 0 || ret == EEXIST {
        return true;
    }
    println!("[setupsh] mkdir {} failed: {}", path, ret);
    false
}

/// 用硬链接创建一个稳定入口；若目标已存在则先删除再重建。
fn ensure_hard_link(src: &str, dst: &str) -> bool {
    let link_ret = link(src, dst);
    if link_ret == 0 {
        return true;
    }
    if link_ret != EEXIST {
        println!("[setupsh] link {} -> {} failed: {}", dst, src, link_ret);
        return false;
    }

    let unlink_ret = unlink(dst);
    if unlink_ret != 0 {
        println!("[setupsh] unlink {} failed: {}", dst, unlink_ret);
        return false;
    }
    let relink_ret = link(src, dst);
    if relink_ret != 0 {
        println!("[setupsh] link {} -> {} failed: {}", dst, src, relink_ret);
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
    let link_ret = link(src, dst);
    if link_ret == 0 {
        return true;
    }
    if link_ret != EEXIST {
        println!("[setupsh] link {} -> {} failed: {}", dst, src, link_ret);
        return false;
    }

    let unlink_ret = unlink(dst);
    if unlink_ret != 0 {
        println!("[setupsh] unlink {} failed: {}", dst, unlink_ret);
        return false;
    }
    let relink_ret = link(src, dst);
    if relink_ret != 0 {
        println!("[setupsh] link {} -> {} failed: {}", dst, src, relink_ret);
        return false;
    }
    true
}

/// 按所选 libc 扫描运行库目录，并在 `/lib` 下创建同名硬链接。
fn install_runtime_libs(libc: BusyBoxLibc) -> bool {
    if !ensure_dir(LIB_DIR) {
        return false;
    }
    let src_dir = libc.lib_dir();
    let fd = open(src_dir, OpenFlags::RDONLY | OpenFlags::DIRECTORY);
    if fd < 0 {
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
            let name_len = name_field.iter().position(|&b| b == 0).unwrap_or(name_field.len());
            if name_len > 0 {
                if let Ok(name) = core::str::from_utf8(&name_field[..name_len]) {
                    if name != "." && name != ".." && dtype != DT_DIR {
                        let src = join_path(src_dir, name);
                        let dst = join_path(LIB_DIR, name);
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
    if let BusyBoxLibc::Musl = libc {
        // musl 动态程序的 PT_INTERP 可能直接指向该 loader 名称。
        if !install_lib_link(MUSL_LIBC_PATH, MUSL_LD_PATH) {
            return false;
        }
    }
    true
}

#[no_mangle]
fn main(_argc: usize, argv: &[&str]) -> i32 {
    const TOTAL_STEPS: usize = 5;
    let busybox_libc = match BusyBoxLibc::from_args(argv) {
        Some(libc) => libc,
        None => return 1,
    };
    let busybox_path = busybox_libc.busybox_path();
    let busybox_path_cstr = busybox_libc.busybox_path_cstr();

    println!("[setupsh] start with {} busybox", busybox_libc.name());
    print_step(1, TOTAL_STEPS, "prepare /bin");
    if !ensure_dir(BIN_DIR) {
        return 1;
    }

    print_step(2, TOTAL_STEPS, "install runtime libraries into /lib");
    if !install_runtime_libs(busybox_libc) {
        return 1;
    }

    print_step(3, TOTAL_STEPS, "install busybox applets into /bin");
    let install_argv = [
        busybox_path_cstr.as_ptr(),
        INSTALL_ARG_CSTR.as_ptr(),
        BIN_DIR_CSTR.as_ptr(),
        ptr::null(),
    ];
    let install_exit = spawn_and_wait(busybox_path_cstr, &install_argv);
    if install_exit != 0 {
        // TODO: 若后续需要兼容 `--install` 的部分成功场景，可在这里补充更细的降级策略。
        println!("[setupsh] busybox --install failed: {}", install_exit);
        return install_exit;
    }

    print_step(4, TOTAL_STEPS, "create /busybox hard link");
    if !ensure_hard_link(busybox_path, ROOT_BUSYBOX) {
        return 1;
    }
    if !ensure_hard_link(busybox_path, BIN_BUSYBOX) {
        return 1;
    }

    print_step(5, TOTAL_STEPS, "launch /bin/sh");
    let shell_argv = [BIN_SH_CSTR.as_ptr(), ptr::null()];
    let shell_exit = spawn_and_wait(BIN_SH_CSTR, &shell_argv);
    println!("[setupsh] /bin/sh exited with {}", shell_exit);
    shell_exit
}
