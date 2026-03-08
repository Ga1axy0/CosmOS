#![no_std]
#![no_main]

use user_lib::{exit, mkdir, STDOUT, write};


extern crate user_lib;


#[no_mangle]
pub fn main(argc: usize, argv: &[&str]) -> i32 {
    if argc < 2 {
        let msg = b"Usage: mkdir <directory>\n";
        write(STDOUT, msg);
        exit(-1);
    }
    let dir = argv[1];
    let ret = mkdir(dir, 0o755);
    if ret < 0 {
        let msg = b"mkdir: failed to create directory\n";
        write(STDOUT, msg);
        exit(-1);
    }
    0
}