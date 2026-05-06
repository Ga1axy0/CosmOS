#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{exec, exit, fork, wait, yield_};

#[no_mangle]
fn main() -> i32 {
    let pid = fork();
    if pid == 0 {
        exec("sh\0", &[core::ptr::null::<u8>()]);
    } else {
        loop {
            let mut exit_code: i32 = 0;
            let wait_result = wait(&mut exit_code);
            if wait_result == pid {
                println!("[initproc] No more children, shutting down...");
                exit(0);
            }
            println!(
                "[initproc] Released a zombie process, pid={}, exit_code={}",
                wait_result, exit_code,
            );
        }
    }
    0
}
