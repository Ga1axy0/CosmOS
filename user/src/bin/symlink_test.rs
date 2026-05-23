#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use user_lib::{
    close, open, read, readlink, symlink, unlink, write, OpenFlags, Stat, StatMode, AT_FDCWD,
    AT_SYMLINK_NOFOLLOW, sys_newfstatat,
};

#[no_mangle]
pub fn main() -> i32 {
    println!("[symlink_test] begin");

    let _ = unlink("SL_SRC");
    let _ = unlink("SL_LINK");

    let fd = open("SL_SRC", OpenFlags::CREATE | OpenFlags::WRONLY);
    assert!(fd >= 0, "create source file");
    assert_eq!(write(fd as usize, b"hello-link"), 10);
    assert_eq!(close(fd as usize), 0);

    assert_eq!(symlink("SL_SRC", "SL_LINK"), 0, "symlinkat");

    let mut link_buf = [0u8; 64];
    let n = readlink("SL_LINK", &mut link_buf);
    assert_eq!(n, 6, "readlink length");
    assert_eq!(&link_buf[..n as usize], b"SL_SRC", "readlink target");

    let fd = open("SL_LINK", OpenFlags::RDONLY);
    assert!(fd >= 0, "open follows symlink");
    let mut data = [0u8; 16];
    let n = read(fd as usize, &mut data);
    assert_eq!(n, 10);
    assert_eq!(&data[..10], b"hello-link");
    assert_eq!(close(fd as usize), 0);

    let nofollow = open("SL_LINK", OpenFlags::RDONLY | OpenFlags::NOFOLLOW);
    assert_eq!(nofollow, -40, "O_NOFOLLOW on final symlink should ELOOP");

    let mut st = Stat::new();
    assert_eq!(
        sys_newfstatat(
            AT_FDCWD as usize,
            "SL_LINK\0",
            &mut st,
            AT_SYMLINK_NOFOLLOW as i32,
        ),
        0
    );
    assert!(st.mode.contains(StatMode::LINK), "lstat should report S_IFLNK");

    assert_eq!(unlink("SL_LINK"), 0);
    assert_eq!(unlink("SL_SRC"), 0);

    println!("[symlink_test] pass");
    0
}
