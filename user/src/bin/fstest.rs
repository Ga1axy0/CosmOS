#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{
	chdir, close, getcwd, getdents64, getpid, get_time, mkdir, open, read, write, OpenFlags,
};

const DT_DIR: u8 = 4;

fn cwd_string() -> String {
	let mut buf = [0u8; 256];
	let ret = getcwd(&mut buf);
	if ret == 0 {
		return String::from("?");
	}
	let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
	core::str::from_utf8(&buf[..len])
		.map(|s| {
			let mut out = String::new();
			out.push_str(s);
			out
		})
		.unwrap_or_else(|_| String::from("?"))
}

fn parse_dirents(buf: &[u8], nread: usize, names: &mut Vec<String>) {
	let mut pos = 0usize;
	while pos + 19 <= nread {
		// d_reclen: u16 @ +16
		let reclen = u16::from_le_bytes([buf[pos + 16], buf[pos + 17]]) as usize;
		if reclen == 0 || pos + reclen > nread {
			break;
		}

		// d_name: starts @ +19, NUL-terminated within this record
		let name_field = &buf[pos + 19..pos + reclen];
		let name_len = name_field
			.iter()
			.position(|&b| b == 0)
			.unwrap_or(name_field.len());
		if name_len > 0 {
			if let Ok(name) = core::str::from_utf8(&name_field[..name_len]) {
				let mut s = String::new();
				s.push_str(name);
				names.push(s);
			}
		}

		pos += reclen;
	}
}

fn contains_name(names: &[String], needle: &str) -> bool {
	names.iter().any(|s| s.as_str() == needle)
}

fn make_unique_root_dir() -> String {
	let pid = getpid();
	let pid = if pid < 0 { 0usize } else { pid as usize };
	let t = get_time();
	let t = if t < 0 { 0usize } else { t as usize };
	// Keep it FAT-friendly: 8 chars, uppercase+digits.
	// Example: T002A123
	format!("T{:03}A{:03}", pid % 1000, t % 1000)
}

#[no_mangle]
pub fn main() -> i32 {
	println!("[fstest] begin");

	// ---- getcwd basic + boundary (too-small buffer) ----
	let cwd0 = cwd_string();
	println!("[fstest] cwd(start)={}", cwd0);
	assert!(!cwd0.is_empty() && cwd0.as_bytes()[0] == b'/');

	let mut tiny = [0u8; 1];
	let ret = getcwd(&mut tiny);
	assert_eq!(ret, -34, "getcwd should fail with too-small buffer");

	// ---- mkdirat/chdir basics ----
	// Create a unique directory at root to avoid collisions with previous runs.
	let base = make_unique_root_dir();
	let mut created = false;
	for off in 0..5usize {
		let name = if off == 0 {
			base.clone()
		} else {
			// tweak the last 3 digits
			let pid = getpid();
			let pid = if pid < 0 { 0usize } else { pid as usize };
			let t = get_time();
			let t = if t < 0 { 0usize } else { t as usize };
			format!("T{:03}A{:03}", pid % 1000, (t + off) % 1000)
		};
		if mkdir(name.as_str(), 0o755) == 0 {
			println!("[fstest] mkdir /{} ok", name);
			// Verify mkdir fails when exists.
			assert!(mkdir(name.as_str(), 0o755) < 0, "mkdir should fail when exists");
			assert_eq!(chdir(name.as_str()), 0, "chdir into test dir");
			created = true;
			break;
		}
	}
	assert!(created, "unable to create a unique test directory");

	let cwd1 = cwd_string();
	println!("[fstest] cwd(testdir)={}", cwd1);
	assert!(cwd1.len() >= 2 && cwd1.as_bytes()[0] == b'/');

	// create subdir and test cd . / ..
	assert_eq!(mkdir("SUB", 0o755), 0, "mkdir SUB");
	assert!(mkdir("SUB", 0o755) < 0, "mkdir SUB twice should fail");
	assert_eq!(chdir("SUB"), 0);
	let cwd_sub = cwd_string();
	assert!(cwd_sub.ends_with("/SUB"), "cwd should end with /SUB");
	assert_eq!(chdir("."), 0);
	assert_eq!(cwd_string(), cwd_sub, "chdir('.') should not change cwd");
	assert_eq!(chdir(".."), 0);
	let cwd_back = cwd_string();
	assert!(!cwd_back.ends_with("/SUB"), "chdir('..') should go to parent");

	// ---- open/write/read/close basics ----
	// Create file and write pattern.
	let fd = open("F1", OpenFlags::CREATE | OpenFlags::WRONLY);
	assert!(fd >= 0, "open(CREATE|WRONLY) should succeed");
	let fd = fd as usize;
	let mut data: Vec<u8> = Vec::new();
	for i in 0..1024usize {
		data.push((i % 251) as u8);
	}
	let w = write(fd, data.as_slice());
	assert_eq!(w as usize, data.len(), "write should write all bytes");
	assert_eq!(close(fd), 0);

	// Reopen and read in small chunks.
	let fd = open("F1", OpenFlags::RDONLY);
	assert!(fd >= 0);
	let fd = fd as usize;
	let mut got: Vec<u8> = Vec::new();
	let mut buf = [0u8; 17];
	loop {
		let n = read(fd, &mut buf);
		assert!(n >= 0, "read should not error");
		let n = n as usize;
		if n == 0 {
			break;
		}
		got.extend_from_slice(&buf[..n]);
	}
	// EOF read should keep returning 0.
	let n2 = read(fd, &mut buf);
	assert_eq!(n2, 0, "read at EOF should return 0");
	assert_eq!(got.as_slice(), data.as_slice(), "read-back should equal written");
	assert_eq!(close(fd), 0);

	// ---- TRUNC behavior ----
	let fd = open("F1", OpenFlags::WRONLY | OpenFlags::TRUNC);
	assert!(fd >= 0);
	let fd = fd as usize;
	let w = write(fd, b"abc");
	assert_eq!(w, 3);
	assert_eq!(close(fd), 0);

	let fd = open("F1", OpenFlags::RDONLY);
	assert!(fd >= 0);
	let fd = fd as usize;
	let mut buf2 = [0u8; 16];
	let n = read(fd, &mut buf2);
	assert_eq!(n, 3, "after TRUNC, should read 3 bytes, actually read {}", n);
	assert_eq!(&buf2[..3], b"abc");
	assert_eq!(close(fd), 0);

	// ---- CREATE existing should not truncate ----
	let fd = open("F1", OpenFlags::CREATE | OpenFlags::WRONLY);
	assert!(fd >= 0);
	let fd = fd as usize;
	assert_eq!(write(fd, b"zz"), 2);
	assert_eq!(close(fd), 0);
	let fd = open("F1", OpenFlags::CREATE | OpenFlags::WRONLY);
	assert!(fd >= 0);
	let fd = fd as usize;
	assert_eq!(close(fd), 0);
	let fd = open("F1", OpenFlags::RDONLY);
	assert!(fd >= 0);
	let fd = fd as usize;
	let n = read(fd, &mut buf2);
	assert_eq!(n, 2, "CREATE on existing should preserve existing file content");
	assert_eq!(&buf2[..2], b"zz");
	assert_eq!(close(fd), 0);

	// ---- open error + close invalid ----
	assert!(open("NOFILE", OpenFlags::RDONLY) < 0, "open non-existent should fail");
	assert!(close(9999) < 0, "close invalid fd should fail");

	// ---- getdents64 tests ----
	// Create some extra files to force multiple getdents64 iterations.
	for i in 0..20usize {
		let name = format!("X{:02}", i);
		let fd = open(name.as_str(), OpenFlags::CREATE | OpenFlags::WRONLY);
		assert!(fd >= 0);
		let fd = fd as usize;
		assert_eq!(write(fd, &[i as u8]), 1);
		assert_eq!(close(fd), 0);
	}

	// (1) getdents64 on a regular file should return 0 (current impl)
	let fd = open("X00", OpenFlags::RDONLY);
	assert!(fd >= 0);
	let fd = fd as usize;
	let mut dtmp = [0u8; 256];
	let n = getdents64(fd, &mut dtmp);
	assert_eq!(n, 0, "getdents64 on non-dir should return 0");
	assert_eq!(close(fd), 0);

	// (2) boundary: too-small buffer may return 0 even when NOT exhausted.
	let dirfd = open(".", OpenFlags::RDONLY);
	assert!(dirfd >= 0);
	let dirfd = dirfd as usize;
	let mut small = [0u8; 18];
	let n_small = getdents64(dirfd, &mut small);
	assert_eq!(n_small, 0, "too-small buffer returns 0 (ambiguous end/toosmall)");

	// Now a normal-sized buffer should still be able to read entries.
	let mut names: Vec<String> = Vec::new();
	let mut buf3 = [0u8; 128];
	loop {
		let n = getdents64(dirfd, &mut buf3);
		assert!(n >= 0, "getdents64 should not error");
		let n = n as usize;
		if n == 0 {
			break;
		}
		parse_dirents(&buf3, n, &mut names);
	}
	assert_eq!(close(dirfd), 0);

	println!("[fstest] dir entries read: {}", names.len());
	assert!(contains_name(&names, "SUB"), "dir should contain SUB");
	assert!(contains_name(&names, "F1"), "dir should contain F1");
	assert!(contains_name(&names, "X00"), "dir should contain X00");

	// Check that DT_DIR appears for SUB at least once in a fresh read.
	let dirfd = open(".", OpenFlags::RDONLY);
	assert!(dirfd >= 0);
	let dirfd = dirfd as usize;
	let mut buf4 = [0u8; 512];
	let mut saw_sub_dir = false;
	let n = getdents64(dirfd, &mut buf4);
	assert!(n >= 0);
	let n = n as usize;
	let mut pos = 0usize;
	while pos + 19 <= n {
		let reclen = u16::from_le_bytes([buf4[pos + 16], buf4[pos + 17]]) as usize;
		if reclen == 0 || pos + reclen > n {
			break;
		}
		let dtype = buf4[pos + 18];
		let name_field = &buf4[pos + 19..pos + reclen];
		let name_len = name_field
			.iter()
			.position(|&b| b == 0)
			.unwrap_or(name_field.len());
		if name_len > 0 {
			if let Ok(name) = core::str::from_utf8(&name_field[..name_len]) {
				if name == "SUB" && dtype == DT_DIR {
					saw_sub_dir = true;
					break;
				}
			}
		}
		pos += reclen;
	}
	assert_eq!(close(dirfd), 0);
	assert!(saw_sub_dir, "SUB should be reported as directory (d_type=DT_DIR)");

	println!("[fstest] PASS");
	0
}
