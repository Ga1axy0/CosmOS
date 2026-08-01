#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{
    close, fcntl,
    net::{IoVec, MsgHdr, AF_INET, AF_UNIX, SOCK_DGRAM, SOCK_STREAM},
    recvfrom, recvmsg, sendto, socket, socketpair, F_GETFL, F_SETFL,
};

const O_NONBLOCK: i32 = 0x800;
const MSG_DONTWAIT: usize = 0x40;
const EAGAIN: isize = 11;

fn expect_eagain(fd: usize, flags: usize, case: &str) -> bool {
    let mut byte = [0u8; 1];
    let result = recvfrom(fd, &mut byte, flags, None);
    if result != -EAGAIN {
        println!(
            "socket_nonblock_test: {} expected -EAGAIN, got {}",
            case, result
        );
        return false;
    }
    true
}

fn expect_recvmsg_eagain(fd: usize, flags: usize, case: &str) -> bool {
    let mut byte = [0u8; 1];
    let iov = IoVec::from_mut_slice(&mut byte);
    let mut msg = MsgHdr {
        msg_iov: &iov as *const IoVec as usize,
        msg_iovlen: 1,
        ..MsgHdr::default()
    };
    let result = recvmsg(fd, &mut msg, flags);
    if result != -EAGAIN {
        println!(
            "socket_nonblock_test: {} expected -EAGAIN, got {}",
            case, result
        );
        return false;
    }
    true
}

fn test_udp() -> bool {
    let fd = socket(AF_INET, SOCK_DGRAM, 0);
    if fd < 0 {
        println!("socket_nonblock_test: UDP socket failed: {}", fd);
        return false;
    }
    let fd = fd as usize;

    let status = fcntl(fd, F_GETFL, 0);
    let passed = status >= 0
        && fcntl(fd, F_SETFL, status as i32 | O_NONBLOCK) >= 0
        && expect_eagain(fd, 0, "UDP O_NONBLOCK")
        && fcntl(fd, F_SETFL, status as i32 & !O_NONBLOCK) >= 0
        && expect_eagain(fd, MSG_DONTWAIT, "UDP MSG_DONTWAIT");
    let _ = close(fd);
    passed
}

fn test_unix_stream() -> bool {
    let mut sv = [-1i32; 2];
    let result = socketpair(AF_UNIX, SOCK_STREAM, 0, &mut sv);
    if result < 0 {
        println!("socket_nonblock_test: socketpair failed: {}", result);
        return false;
    }
    let fd = sv[0] as usize;
    let peer = sv[1] as usize;
    let status = fcntl(fd, F_GETFL, 0);
    let mut passed = status >= 0
        && fcntl(fd, F_SETFL, status as i32 | O_NONBLOCK) >= 0
        && expect_eagain(fd, 0, "AF_UNIX recv O_NONBLOCK")
        && expect_recvmsg_eagain(fd, 0, "AF_UNIX recvmsg O_NONBLOCK");

    let fill = [0xa5u8; 256];
    let mut filled = false;
    if passed {
        for _ in 0..64 {
            let written = sendto(fd, &fill, 0, None);
            if written == -EAGAIN {
                filled = true;
                break;
            }
            if written <= 0 {
                println!(
                    "socket_nonblock_test: AF_UNIX nonblocking send failed: {}",
                    written
                );
                break;
            }
        }
        if !filled {
            println!("socket_nonblock_test: AF_UNIX send did not reach -EAGAIN");
            passed = false;
        }
    }

    if passed {
        passed = fcntl(fd, F_SETFL, status as i32 & !O_NONBLOCK) >= 0
            && expect_eagain(fd, MSG_DONTWAIT, "AF_UNIX recv MSG_DONTWAIT")
            && expect_recvmsg_eagain(fd, MSG_DONTWAIT, "AF_UNIX recvmsg MSG_DONTWAIT")
            && sendto(fd, &fill, MSG_DONTWAIT, None) == -EAGAIN;
        if !passed {
            println!("socket_nonblock_test: AF_UNIX MSG_DONTWAIT send failed");
        }
    }

    let _ = close(fd);
    let _ = close(peer);
    passed
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    if !test_udp() || !test_unix_stream() {
        return -1;
    }
    println!("socket_nonblock_test: PASS");
    0
}
