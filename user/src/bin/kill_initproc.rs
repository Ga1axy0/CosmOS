#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{exit, getpid, kill, SIGKILL, SIGTERM};

const EPERM: isize = -1;

#[no_mangle]
pub fn main(_argc: usize, _argv: &[&str]) -> i32 {
    let self_pid = getpid();
    if self_pid == 1 {
        println!("[kill_initproc] test must not run as initproc");
        exit(1);
    }

    let term_ret = kill(1, SIGTERM);
    let kill_ret = kill(1, SIGKILL);
    let zero_ret = kill(1, 0);

    if term_ret != EPERM {
        println!(
            "[kill_initproc] SIGTERM to pid 1 returned {}, expected {}",
            term_ret, EPERM
        );
        exit(1);
    }
    if kill_ret != EPERM {
        println!(
            "[kill_initproc] SIGKILL to pid 1 returned {}, expected {}",
            kill_ret, EPERM
        );
        exit(1);
    }
    if zero_ret != 0 {
        println!(
            "[kill_initproc] signal 0 to pid 1 returned {}, expected 0",
            zero_ret
        );
        exit(1);
    }

    println!("[kill_initproc] pid 1 is protected from termination");
    exit(0);
}
