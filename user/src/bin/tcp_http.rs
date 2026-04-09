#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{
    close, connect, read, socket, write,
    net::{SockAddrIn, AF_INET, SOCK_STREAM},
};

#[unsafe(no_mangle)]
fn main() -> i32 {
    // Connect to host-side service through QEMU slirp.
    // In QEMU user networking, the host is reachable at 10.0.2.2 from the guest.
    let server = SockAddrIn::from_ipv4_port([10, 0, 2, 2], 8000);

    let fd = socket(AF_INET, SOCK_STREAM, 0);
    if fd < 0 {
        println!("tcp_http: socket() failed");
        return -1;
    }
    let fd = fd as usize;

    if connect(fd, &server) < 0 {
        println!("tcp_http: connect() failed");
        let _ = close(fd);
        return -1;
    }

    let req = b"GET / HTTP/1.0\r\nHost: 10.0.2.2\r\n\r\n";
    let wn = write(fd, req);
    if wn < 0 {
        println!("tcp_http: write() failed");
        let _ = close(fd);
        return -1;
    }

    let mut buf = [0u8; 1024];
    let rn = read(fd, &mut buf);
    if rn < 0 {
        println!("tcp_http: read() failed");
        let _ = close(fd);
        return -1;
    }

    let rn = rn as usize;
    println!("tcp_http: read {} bytes", rn);
    if let Ok(s) = core::str::from_utf8(&buf[..rn]) {
        println!("{}", s);
    } else {
        // Print raw bytes if not UTF-8.
        for &b in buf[..rn].iter() {
            print!("{:02x} ", b);
        }
        println!("");
    }

    let _ = close(fd);
    0
}
