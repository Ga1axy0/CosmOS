#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{close, exit, open, read, write, OpenFlags, STDOUT};

const ETC_PASSWD: &str = "/etc/passwd";
const ETC_GROUP: &str = "/etc/group";
const PROC_KEY_USERS: &str = "/proc/key-users";

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

fn split_lines(bytes: &[u8]) -> Vec<String> {
    let text = core::str::from_utf8(bytes).unwrap_or("");
    text.lines().map(String::from).collect()
}

fn next_id(passwd_lines: &[String], group_lines: &[String], key_user_lines: &[String]) -> u32 {
    let mut next = 1000u32;
    for line in passwd_lines {
        let mut parts = line.split(':');
        let _name = parts.next();
        let _passwd = parts.next();
        if let Some(uid) = parts.next() {
            if let Ok(uid) = uid.parse::<u32>() {
                next = next.max(uid.saturating_add(1));
            }
        }
    }
    for line in group_lines {
        let mut parts = line.split(':');
        let _name = parts.next();
        let _passwd = parts.next();
        if let Some(gid) = parts.next() {
            if let Ok(gid) = gid.parse::<u32>() {
                next = next.max(gid.saturating_add(1));
            }
        }
    }
    for line in key_user_lines {
        if let Some((uid, _rest)) = line.split_once(':') {
            if let Ok(uid) = uid.trim().parse::<u32>() {
                next = next.max(uid.saturating_add(1));
            }
        }
    }
    next.max(1000)
}

#[no_mangle]
pub fn main(argc: usize, argv: &[&str]) -> i32 {
    if argc < 2 {
        let _ = write(STDOUT, b"Usage: useradd <name>\n");
        exit(1);
    }

    let username = argv[1];
    let passwd_bytes = read_file(ETC_PASSWD).unwrap_or_default();
    let group_bytes = read_file(ETC_GROUP).unwrap_or_default();
    let key_user_bytes = read_file(PROC_KEY_USERS).unwrap_or_default();
    let mut passwd_lines = split_lines(&passwd_bytes);
    let mut group_lines = split_lines(&group_bytes);
    let key_user_lines = split_lines(&key_user_bytes);

    if passwd_lines.iter().any(|line| line.starts_with(&format!("{username}:"))) {
        return 0;
    }

    let id = next_id(&passwd_lines, &group_lines, &key_user_lines);
    passwd_lines.push(format!("{username}:x:{id}:{id}:{username}:/tmp:/bin/sh"));
    group_lines.push(format!("{username}:x:{id}:"));

    let mut passwd_out = passwd_lines.join("\n");
    passwd_out.push('\n');
    let mut group_out = group_lines.join("\n");
    group_out.push('\n');

    if write_file(ETC_PASSWD, passwd_out.as_bytes()).is_err()
        || write_file(ETC_GROUP, group_out.as_bytes()).is_err()
    {
        exit(1);
    }

    0
}
