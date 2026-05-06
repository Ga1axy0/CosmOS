#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

extern crate alloc;

use user_lib::{socketpair, read, write, close, net::{AF_UNIX, SOCK_STREAM}};

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("socketpair_test: starting");

    let mut sv: [i32; 2] = [-1, -1];
    if socketpair(AF_UNIX, SOCK_STREAM, 0, &mut sv) < 0 {
        println!("socketpair_test: socketpair() failed");
        return -1;
    }
    let fd0 = sv[0] as usize;
    let fd1 = sv[1] as usize;
    println!("socketpair_test: created fds {} {}", fd0, fd1);

    // Write from fd0 -> fd1
    let msg = b"hello from fd0\n";
    let wn = write(fd0, msg);
    if wn < 0 {
        println!("socketpair_test: write(fd0) failed: {}", wn);
    }

    let mut buf = [0u8; 128];
    let rn = read(fd1, &mut buf);
    if rn <= 0 {
        println!("socketpair_test: read(fd1) failed: {}", rn);
        let _ = close(fd0);
        let _ = close(fd1);
        return -1;
    }
    let rn = rn as usize;
    if let Ok(s) = core::str::from_utf8(&buf[..rn]) {
        println!("socketpair_test: fd1 received: {}", s);
    } else {
        println!("socketpair_test: fd1 received (non-utf8)");
    }

    // Reply from fd1 -> fd0
    let reply = b"reply from fd1\n";
    let wn2 = write(fd1, reply);
    if wn2 < 0 {
        println!("socketpair_test: write(fd1) failed: {}", wn2);
    }

    let rn2 = read(fd0, &mut buf);
    if rn2 <= 0 {
        println!("socketpair_test: read(fd0) failed: {}", rn2);
        let _ = close(fd0);
        let _ = close(fd1);
        return -1;
    }
    let rn2 = rn2 as usize;
    if let Ok(s) = core::str::from_utf8(&buf[..rn2]) {
        println!("socketpair_test: fd0 received: {}", s);
    } else {
        println!("socketpair_test: fd0 received (non-utf8)");
    }

    let _ = close(fd0);
    let _ = close(fd1);
    println!("socketpair_test: success");
    0
}
