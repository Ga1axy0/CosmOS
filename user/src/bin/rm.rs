#![no_std]
#![no_main]

use user_lib::{AT_FDCWD, AT_REMOVEDIR, OpenFlags, STDOUT, close, exit, getdents64, open, print, println, sys_linkat, sys_mkdirat, sys_openat, sys_unlinkat, write};

const BUF_SIZE: usize = 6 * 1024;
const DT_DIR: u8 = 4;

#[no_mangle]
pub fn main(_argc: usize, _argv: &[&str]) -> i32 {
    if _argc < 2 {
        let msg = "Usage: rm <file>";
        println!("{}", msg);
        exit(-1);
    }
    let path = _argv[1];
    let ret = sys_unlinkat(AT_FDCWD as usize, path, 0);
    if ret < 0 {
        print!("rm: failed to remove '{}': ", path);
        match ret {
            -2 => println!("No such file or directory"),
            -5 => println!("IO error"),
            -21 => println!("Is a directory"),
            -22 => println!("Invalid flags"),
            _ => println!("Unknown error {}", ret),
        }
    }
    0
}