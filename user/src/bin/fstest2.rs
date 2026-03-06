#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use user_lib::{
    chdir, close, getcwd, getdents64, getpid, get_time, mkdir, open, read, write, OpenFlags,
};
use alloc::format;
use alloc::string::String;

#[no_mangle]
pub fn main() -> i32 {
    println!("Testing write large file to ext4 image...");
    let filename = "large_file.txt";
    for i in 0..50 {
        println!("Iteration {}: writing to {}", i, filename);
        let content = [b'x'; 5 * 1024];
        let fd = open(filename, OpenFlags::WRONLY | OpenFlags::CREATE);
        if fd < 0 {
            println!("Failed to open file: {}", filename);
            return -1;
        }
        let fd = fd as usize;
        let bytes_written = write(fd, &content);
        if bytes_written < 0 {
            println!("Failed to write to file: {}", filename);
            return -1;
        }
        close(fd);
    }

    println!("Testing making many directories...");
    for i in 0..50 {
        let dirname = format!("dir_{}", i);
        println!("Iteration {}: creating directory {}", i, dirname);
        if mkdir(&dirname, 0o755) < 0 {
            println!("Failed to create directory: {}", dirname);
            return -1;
        }
    }

    println!("Testing making deep directory...");
    let mut path = String::new();
    for i in 51..100 {
        path += &format!("dir_{}/", i);
        if mkdir(&path, 0o755) < 0 {
            println!("Failed to create deep directory: {}", path);
            return -1;
        }
    }
    println!("Creating deep directory: {}", path);
    0
}