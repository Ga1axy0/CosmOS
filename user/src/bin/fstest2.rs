#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use user_lib::{
	chdir, close, getcwd, getdents64, getpid, get_time, mkdir, open, read, write, OpenFlags,
};

#[no_mangle]
pub fn main() -> i32 {
    println!("Testing write large file to ext4 image...");
    let filename = "large_file.txt";
    for i in 0..50 {
        println!("Iteration {}: writing to {}", i, filename);
        let content = [b'x'; 5 * 1024];
        let fd = open(filename, OpenFlags::CREATE | OpenFlags::WRONLY);
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
        close(fd);  // ← 别忘了关文件
    }
    0
}