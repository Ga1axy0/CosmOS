#![no_std]
#![no_main]

use user_lib::{close, open, println, read, rename, unlink, write, OpenFlags};

const SRC: &str = "/dev/shm/TMPFS_A";
const DST: &str = "/dev/shm/TMPFS_B";

#[no_mangle]
pub fn main(_argc: usize, _argv: &[&str]) -> i32 {
    let _ = unlink(SRC);
    let _ = unlink(DST);

    let fd = open(SRC, OpenFlags::CREATE | OpenFlags::RDWR);
    assert!(fd >= 0, "open src failed: {}", fd);
    assert_eq!(write(fd as usize, b"tmpfs-rename"), 12, "write failed");
    assert_eq!(close(fd as usize), 0, "close after write failed");

    assert_eq!(rename(SRC, DST), 0, "rename failed");

    let old_fd = open(SRC, OpenFlags::RDONLY);
    assert!(old_fd < 0, "old path should disappear after rename");

    let fd = open(DST, OpenFlags::RDONLY);
    assert!(fd >= 0, "open dst failed: {}", fd);
    let mut buf = [0u8; 32];
    let n = read(fd as usize, &mut buf);
    assert_eq!(n, 12, "read size mismatch");
    assert_eq!(&buf[..12], b"tmpfs-rename", "read-back mismatch");
    assert_eq!(close(fd as usize), 0, "close after read failed");

    assert_eq!(unlink(DST), 0, "cleanup unlink failed");
    println!("[tmpfs_test] pass");
    0
}
