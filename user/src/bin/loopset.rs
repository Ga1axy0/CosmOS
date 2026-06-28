#![no_std]
#![no_main]

use user_lib::{OpenFlags, close, ioctl, open, println};

const LOOP_SET_FD: usize = 0x4c00;
const LOOP_CLR_FD: usize = 0x4c01;
const DEFAULT_LOOP_DEVICE: &str = "/dev/loop0";

fn usage() -> i32 {
    println!("usage: loopset <image> [loopdev]");
    println!("       loopset --clear [loopdev]");
    1
}

#[no_mangle]
pub fn main(argc: usize, argv: &[&str]) -> i32 {
    if argc < 2 {
        return usage();
    }

    if argv[1] == "--clear" {
        let loopdev = if argc >= 3 {
            argv[2]
        } else {
            DEFAULT_LOOP_DEVICE
        };
        let loop_fd = open(loopdev, OpenFlags::RDONLY);
        if loop_fd < 0 {
            println!("loopset: open {} failed: {}", loopdev, loop_fd);
            return 1;
        }
        let ret = ioctl(loop_fd as usize, LOOP_CLR_FD, 0);
        let _ = close(loop_fd as usize);
        if ret < 0 {
            println!("loopset: clear {} failed: {}", loopdev, ret);
            return 1;
        }
        println!("loopset: cleared {}", loopdev);
        return 0;
    }

    let image = argv[1];
    let loopdev = if argc >= 3 {
        argv[2]
    } else {
        DEFAULT_LOOP_DEVICE
    };

    let image_fd = open(image, OpenFlags::RDONLY);
    if image_fd < 0 {
        println!("loopset: open {} failed: {}", image, image_fd);
        return 1;
    }

    let loop_fd = open(loopdev, OpenFlags::RDONLY);
    if loop_fd < 0 {
        println!("loopset: open {} failed: {}", loopdev, loop_fd);
        let _ = close(image_fd as usize);
        return 1;
    }

    let ret = ioctl(loop_fd as usize, LOOP_SET_FD, image_fd as usize);
    let _ = close(loop_fd as usize);
    let _ = close(image_fd as usize);
    if ret < 0 {
        println!(
            "loopset: attach {} -> {} failed: {}",
            image, loopdev, ret
        );
        return 1;
    }

    println!("loopset: attached {} -> {}", image, loopdev);
    0
}
