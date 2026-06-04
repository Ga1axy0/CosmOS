#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::ptr;

use user_lib::exec;

const BIN_SH: &str = "/bin/sh\0";

#[no_mangle]
fn main(_argc: usize, argv: &[&str]) -> i32 {
    let mut forwarded: Vec<String> = Vec::new();
    forwarded.push(String::from(BIN_SH));
    for arg in argv.iter().skip(1) {
        let mut s = String::from(*arg);
        s.push('\0');
        forwarded.push(s);
    }

    let mut argv_ptrs: Vec<*const u8> = forwarded.iter().map(|arg| arg.as_ptr()).collect();
    argv_ptrs.push(ptr::null());

    let ret = exec(BIN_SH, argv_ptrs.as_slice());
    println!("bash: exec /bin/sh failed: {}", ret);
    127
}
