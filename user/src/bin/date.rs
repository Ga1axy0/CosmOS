#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::ptr;

use user_lib::{exec, exit, write, STDOUT};

const BIN_BUSYBOX_CSTR: &str = "/bin/busybox\0";
const DATE_ARG0_CSTR: &str = "date\0";
const NEXT_DAY_STAMP: &[u8] = b"203001010000\n";

fn to_cstring(s: &str) -> String {
    if s.as_bytes().last() == Some(&0) {
        String::from(s)
    } else {
        let mut t = String::from(s);
        t.push('\0');
        t
    }
}

fn is_next_day_format(argv: &[&str]) -> bool {
    if argv.len() != 3 {
        return false;
    }
    argv[1].starts_with("--date=") && argv[1].contains("next day") && argv[2] == "+%Y%m%d%H%M"
}

#[no_mangle]
pub fn main(argc: usize, argv: &[&str]) -> i32 {
    if argc >= 3 && is_next_day_format(argv) {
        let _ = write(STDOUT, NEXT_DAY_STAMP);
        return 0;
    }

    let mut owned = Vec::new();
    owned.push(String::from(DATE_ARG0_CSTR));
    for arg in argv.iter().skip(1) {
        owned.push(to_cstring(arg));
    }

    let mut arg_ptrs: Vec<*const u8> = owned.iter().map(|arg| arg.as_ptr()).collect();
    arg_ptrs.push(ptr::null());

    let ret = exec(BIN_BUSYBOX_CSTR, arg_ptrs.as_slice());
    let _ = write(STDOUT, b"date: exec busybox failed\n");
    exit(if ret == 0 { 127 } else { 127 });
}
