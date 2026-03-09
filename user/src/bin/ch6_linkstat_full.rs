#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use alloc::format;
use alloc::string::String;
use user_lib::{
    close, fstat, get_time, getpid, link, open, read, unlink, write, OpenFlags, Stat, StatMode,
};

fn unique_name(base: &str) -> String {
    let pid = getpid();
    let pid = if pid < 0 { 0 } else { pid as usize };
    let t = get_time();
    let t = if t < 0 { 0 } else { t as usize };
    format!("{}_p{}_t{}", base, pid % 1000, t % 100000)
}

#[no_mangle]
pub fn main() -> i32 {
    // ---------- setup ----------
    let base = unique_name("fstat_link_base");
    let l1 = unique_name("fstat_link_1");
    let l2 = unique_name("fstat_link_2");
    let payload = b"fstat-link-unlinkat-full-test";

    // Best-effort cleanup from previous abnormal exits.
    let _ = unlink(base.as_str());
    let _ = unlink(l1.as_str());
    let _ = unlink(l2.as_str());

    // ---------- negative path ----------
    let mut bad = Stat::new();
    assert_eq!(fstat(4096, &mut bad), -1, "fstat on invalid fd should fail");
    assert!(link("no_such_file_xxx", "no_such_link_xxx") < 0, "link on missing source should fail");
    assert!(unlink("no_such_file_xxx") < 0, "unlink on missing file should fail");

    // ---------- create + fstat ----------
    let fd = open(base.as_str(), OpenFlags::CREATE | OpenFlags::WRONLY);
    assert!(fd >= 0, "open(create) should succeed");
    let fd = fd as usize;

    let n = write(fd, payload);
    assert_eq!(n as usize, payload.len(), "write size mismatch");

    let mut st0 = Stat::new();
    assert_eq!(fstat(fd, &mut st0), 0, "fstat on created file should succeed");
    assert_eq!(st0.mode, StatMode::FILE, "mode should be regular file");
    assert_eq!(st0.nlink, 1, "new file nlink should be 1");

    // ---------- link twice ----------
    assert_eq!(link(base.as_str(), l1.as_str()), 0, "first link should succeed");
    assert_eq!(link(base.as_str(), l2.as_str()), 0, "second link should succeed");

    let mut st1 = Stat::new();
    assert_eq!(fstat(fd, &mut st1), 0);
    assert_eq!(st1.dev, st0.dev, "dev should be stable");
    assert_eq!(st1.ino, st0.ino, "ino should be stable");
    assert_eq!(st1.nlink, 3, "nlink after two links should be 3");
    assert_eq!(close(fd), 0);

    // ---------- validate link target data + inode identity ----------
    let fd2 = open(l2.as_str(), OpenFlags::RDONLY);
    assert!(fd2 >= 0, "open(link) should succeed");
    let fd2 = fd2 as usize;

    let mut buf = [0u8; 128];
    let rn = read(fd2, &mut buf);
    assert_eq!(rn as usize, payload.len(), "read size mismatch");
    assert_eq!(&buf[..payload.len()], payload, "link should share same content");

    let mut st2 = Stat::new();
    assert_eq!(fstat(fd2, &mut st2), 0);
    assert_eq!(st2.dev, st0.dev, "linked file dev should match");
    assert_eq!(st2.ino, st0.ino, "linked file ino should match");
    assert_eq!(st2.nlink, 3, "linked file nlink should be 3");
    assert_eq!(close(fd2), 0);

    // ---------- unlink one name ----------
    assert_eq!(unlink(l1.as_str()), 0, "unlink first link should succeed");

    let fd3 = open(base.as_str(), OpenFlags::RDONLY);
    assert!(fd3 >= 0);
    let fd3 = fd3 as usize;
    let mut st3 = Stat::new();
    assert_eq!(fstat(fd3, &mut st3), 0);
    assert_eq!(st3.nlink, 2, "nlink after removing one name should be 2");
    assert_eq!(close(fd3), 0);

    // ---------- remove original, keep one link ----------
    assert_eq!(unlink(base.as_str()), 0, "unlink original should succeed");
    assert!(open(base.as_str(), OpenFlags::RDONLY) < 0, "original name should disappear");

    let fd4 = open(l2.as_str(), OpenFlags::RDONLY);
    assert!(fd4 >= 0, "remaining link should still be accessible");
    let fd4 = fd4 as usize;
    let mut st4 = Stat::new();
    assert_eq!(fstat(fd4, &mut st4), 0);
    assert_eq!(st4.nlink, 1, "final single-link nlink should be 1");
    assert_eq!(close(fd4), 0);

    // ---------- final cleanup ----------
    assert_eq!(unlink(l2.as_str()), 0, "unlink final link should succeed");
    assert!(open(l2.as_str(), OpenFlags::RDONLY) < 0, "all names removed: open should fail");

    println!("Test fstat/linkat/unlinkat full OK!");
    0
}
