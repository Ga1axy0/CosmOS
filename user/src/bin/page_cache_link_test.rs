#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use alloc::format;
use alloc::vec::Vec;
use user_lib::{
    chdir, close, get_time, getpid, link, mkdir, open, read, sys_unlinkat, write, OpenFlags,
};

fn enter_unique_directory() {
    let pid = getpid().max(0) as usize;
    let time = get_time().max(0) as usize;
    for offset in 0..8 {
        let name = format!("P{:03}L{:03}", pid % 1000, (time + offset) % 1000);
        if mkdir(name.as_str(), 0o755) == 0 {
            assert_eq!(chdir(name.as_str()), 0, "enter regression directory");
            return;
        }
    }
    panic!("unable to create regression directory");
}

#[no_mangle]
pub fn main() -> i32 {
    println!("[page_cache_link_test] begin");
    enter_unique_directory();

    let fd = open("SOURCE", OpenFlags::CREATE | OpenFlags::WRONLY);
    assert!(fd >= 0, "create source");
    let fd = fd as usize;
    let mut expected = Vec::new();
    for index in 0..(3 * 4096 + 37) {
        expected.push(((index * 17 + 29) % 251) as u8);
    }
    assert_eq!(
        write(fd, expected.as_slice()),
        expected.len() as isize,
        "write dirty source"
    );
    assert_eq!(close(fd), 0);
    assert_eq!(link("SOURCE", "SURVIVE"), 0, "create hard link");

    // A real directory fd selects sys_unlinkat's direct-inode fast path.
    let dirfd = open(".", OpenFlags::RDONLY | OpenFlags::DIRECTORY);
    assert!(dirfd >= 0, "open parent directory");
    let dirfd = dirfd as usize;
    assert_eq!(
        sys_unlinkat(dirfd, "SOURCE\0", 0),
        0,
        "remove one hard-link name"
    );
    assert_eq!(close(dirfd), 0);

    let fd = open("SURVIVE", OpenFlags::RDONLY);
    assert!(fd >= 0, "open surviving hard link");
    let fd = fd as usize;
    let mut actual = Vec::new();
    let mut buffer = [0u8; 521];
    loop {
        let count = read(fd, &mut buffer);
        assert!(count >= 0, "read surviving hard link");
        if count == 0 {
            break;
        }
        actual.extend_from_slice(&buffer[..count as usize]);
    }
    assert_eq!(close(fd), 0);
    assert_eq!(
        actual.as_slice(),
        expected.as_slice(),
        "surviving hard link lost dirty bytes or logical length"
    );

    println!("[page_cache_link_test] PASS");
    0
}
