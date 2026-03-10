#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use alloc::format;
use alloc::string::String;
use user_lib::{
    close, get_time, getpid, read, sys_linkat, sys_mkdirat, sys_openat, sys_unlinkat, write,
    OpenFlags, AT_FDCWD, AT_REMOVEDIR,
};

fn to_cstring(s: &str) -> String {
    let mut out = String::from(s);
    out.push('\0');
    out
}

fn unique_name(prefix: &str) -> String {
    let pid = getpid();
    let pid = if pid < 0 { 0 } else { pid as usize };
    let t = get_time();
    let t = if t < 0 { 0 } else { t as usize };
    format!("{}_{}_{}", prefix, pid % 1000, t % 100000)
}

#[no_mangle]
pub fn main() -> i32 {
    let base = unique_name("dirfd");
    let base_c = to_cstring(base.as_str());
    let sub_c = to_cstring("sub");
    let a_c = to_cstring("a");
    let a2_c = to_cstring("a2");
    let b_c = to_cstring("b");
    let b2_c = to_cstring("b2");
    let payload = b"dirfd-ext4-test";

    assert_eq!(sys_mkdirat(AT_FDCWD as usize, base_c.as_str(), 0o755), 0);

    let dirfd = sys_openat(AT_FDCWD as usize, base_c.as_str(), OpenFlags::RDONLY.bits(), 0);
    assert!(dirfd >= 0, "open base dir failed: {}", dirfd);
    let dirfd = dirfd as usize;

    assert_eq!(sys_mkdirat(dirfd, sub_c.as_str(), 0o755), 0);

    let fd = sys_openat(dirfd, a_c.as_str(), (OpenFlags::CREATE | OpenFlags::WRONLY).bits(), 0);
    assert!(fd >= 0, "create a failed: {}", fd);
    let fd = fd as usize;
    assert_eq!(write(fd, payload), payload.len() as isize);
    assert_eq!(close(fd), 0);

    let subdirfd = sys_openat(dirfd, sub_c.as_str(), OpenFlags::RDONLY.bits(), 0);
    assert!(subdirfd >= 0, "open subdir failed: {}", subdirfd);
    let subdirfd = subdirfd as usize;

    let nested = sys_openat(
        subdirfd,
        b_c.as_str(),
        (OpenFlags::CREATE | OpenFlags::WRONLY).bits(),
        0,
    );
    assert!(nested >= 0, "create nested file failed: {}", nested);
    let nested = nested as usize;
    assert_eq!(write(nested, b"nested"), 6);
    assert_eq!(close(nested), 0);

    assert_eq!(sys_linkat(dirfd, a_c.as_str(), dirfd, a2_c.as_str(), 0), 0);
    assert_eq!(sys_linkat(dirfd, a_c.as_str(), subdirfd, b2_c.as_str(), 0), 0);

    let link_fd = sys_openat(subdirfd, b2_c.as_str(), OpenFlags::RDONLY.bits(), 0);
    assert!(link_fd >= 0, "open linked file failed: {}", link_fd);
    let link_fd = link_fd as usize;
    let mut buf = [0u8; 32];
    let n = read(link_fd, &mut buf);
    assert_eq!(n, payload.len() as isize);
    assert_eq!(&buf[..payload.len()], payload);
    assert_eq!(close(link_fd), 0);

    assert_eq!(sys_unlinkat(dirfd, sub_c.as_str(), 0), -21);
    assert_eq!(sys_unlinkat(subdirfd, b2_c.as_str(), 0), 0);
    assert_eq!(sys_unlinkat(subdirfd, b_c.as_str(), 0), 0);
    assert_eq!(close(subdirfd), 0);
    assert_eq!(sys_unlinkat(dirfd, sub_c.as_str(), AT_REMOVEDIR), 0);
    assert_eq!(sys_unlinkat(dirfd, a2_c.as_str(), 0), 0);
    assert_eq!(sys_unlinkat(dirfd, a_c.as_str(), 0), 0);
    assert_eq!(close(dirfd), 0);
    assert_eq!(sys_unlinkat(AT_FDCWD as usize, base_c.as_str(), AT_REMOVEDIR), 0);

    println!("Test dirfd/openat/mkdirat/linkat/unlinkat OK!");
    0
}
