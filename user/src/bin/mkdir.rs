#![no_std]
#![no_main]

use user_lib::{STDOUT, exit, mkdir, println, write};


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
        println!("mkdir: failed to create '{}':", dir);
        match ret {
            -1 => println!("Operation not permitted"),
            -2 => println!("No such file or directory"),
            -5 => println!("IO error"),
            -17 => println!("File exists"),
            _ => println!("Unknown error {}", ret),
        }
        exit(-1);
    }
    0
}