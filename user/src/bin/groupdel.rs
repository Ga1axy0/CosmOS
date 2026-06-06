#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{close, exit, open, read, write, OpenFlags, STDOUT};

const ETC_GROUP: &str = "/etc/group";

fn read_file(path: &str) -> Result<Vec<u8>, isize> {
    let fd = open(path, OpenFlags::RDONLY);
    if fd < 0 {
        return Err(fd);
    }
    let fd = fd as usize;
    let mut out = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        let n = read(fd, &mut buf);
        if n < 0 {
            close(fd);
            return Err(n);
        }
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    close(fd);
    Ok(out)
}

fn write_file(path: &str, data: &[u8]) -> Result<(), isize> {
    let fd = open(path, OpenFlags::CREATE | OpenFlags::TRUNC | OpenFlags::WRONLY);
    if fd < 0 {
        return Err(fd);
    }
    let fd = fd as usize;
    let mut written = 0usize;
    while written < data.len() {
        let n = write(fd, &data[written..]);
        if n < 0 {
            close(fd);
            return Err(n);
        }
        written += n as usize;
    }
    close(fd);
    Ok(())
}

#[no_mangle]
pub fn main(argc: usize, argv: &[&str]) -> i32 {
    if argc < 2 {
        let _ = write(STDOUT, b"Usage: groupdel <name>\n");
        exit(1);
    }

    let username = argv[1];
    let prefix = format!("{username}:");
    let mut out = String::new();
    let group_bytes = read_file(ETC_GROUP).unwrap_or_default();
    for line in core::str::from_utf8(&group_bytes).unwrap_or("").lines() {
        if line.starts_with(&prefix) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }

    if write_file(ETC_GROUP, out.as_bytes()).is_err() {
        exit(1);
    }

    0
}
