#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{exec, exit, fork, wait};

const ECHILD: isize = -10;

#[no_mangle]
fn main() -> i32 {
    let pid = fork();
    if pid == 0 {
        exec("sh\0", &[core::ptr::null::<u8>()]);
    } else if pid < 0 {
        println!("[initproc] fork failed: {}", pid);
        exit(1);
    } else {
        loop {
            let mut exit_code: i32 = 0;
            let wait_result = wait(&mut exit_code);
            if wait_result == ECHILD {
                println!("[initproc] No more children, shutting down...");
                exit(0);
            }
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
