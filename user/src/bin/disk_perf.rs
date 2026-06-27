#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use alloc::format;
use alloc::string::String;
use core::cmp::min;
use user_lib::{
    close, fsync, ftruncate, get_time, getpid, open, pread64, pwrite64, read, sync, unlink, write,
    OpenFlags,
};

const SEQ_BLOCK: usize = 16 * 1024;
const RAND_BLOCK: usize = 4 * 1024;

struct Case {
    name: &'static str,
    size: usize,
}

const CASES: &[Case] = &[
    Case {
        name: "small",
        size: 64 * 1024,
    },
    Case {
        name: "medium",
        size: 1024 * 1024,
    },
    Case {
        name: "large",
        size: 8 * 1024 * 1024,
    },
    Case {
        name: "xlarge",
        size: 32 * 1024 * 1024,
    },
    Case {
        name: "xxlarge",
        size: 128 * 1024 * 1024,
    },
];

#[derive(Clone, Copy)]
struct Report {
    bytes: usize,
    ops: usize,
    ms: isize,
    checksum: u64,
}

fn elapsed_ms(start: isize) -> isize {
    let end = get_time();
    if end > start {
        end - start
    } else {
        1
    }
}

fn fill_pattern(buf: &mut [u8], seed: u8) {
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte = seed.wrapping_add((i as u8).wrapping_mul(37));
    }
}

fn checksum(buf: &[u8]) -> u64 {
    let mut sum = 0u64;
    for &byte in buf {
        sum = sum.wrapping_add(byte as u64);
    }
    sum
}

fn next_rand(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    *state
}

fn print_rate(case: &Case, op: &str, report: Report) {
    let ms = if report.ms <= 0 { 1 } else { report.ms as u64 };
    let bytes = report.bytes as u64;
    let mbps_x100 = bytes.saturating_mul(100_000) / (1024 * 1024) / ms;
    let iops = (report.ops as u64).saturating_mul(1000) / ms;

    println!(
        "{:>6} {:>11}: {:>8} KiB, {:>5} ops, {:>5} ms, {:>5}.{:02} MiB/s, {:>6} ops/s, sum={}",
        case.name,
        op,
        report.bytes / 1024,
        report.ops,
        report.ms,
        mbps_x100 / 100,
        mbps_x100 % 100,
        iops,
        report.checksum
    );
}

fn make_path(base: &str, case: &Case) -> String {
    if base.as_bytes().last() == Some(&b'/') {
        format!("{}disk_perf_{}_{}.dat", base, getpid(), case.name)
    } else {
        format!("{}/disk_perf_{}_{}.dat", base, getpid(), case.name)
    }
}

fn write_all(fd: usize, buf: &[u8]) -> bool {
    let mut done = 0usize;
    while done < buf.len() {
        let n = write(fd, &buf[done..]);
        if n <= 0 {
            println!("disk_perf: write failed: {}", n);
            return false;
        }
        done += n as usize;
    }
    true
}

fn read_all(fd: usize, buf: &mut [u8]) -> bool {
    let mut done = 0usize;
    while done < buf.len() {
        let n = read(fd, &mut buf[done..]);
        if n <= 0 {
            println!("disk_perf: read failed: {}", n);
            return false;
        }
        done += n as usize;
    }
    true
}

fn pwrite_all(fd: usize, buf: &[u8], offset: usize) -> bool {
    let mut done = 0usize;
    while done < buf.len() {
        let n = pwrite64(fd, &buf[done..], offset + done);
        if n <= 0 {
            println!("disk_perf: pwrite64 failed: {}", n);
            return false;
        }
        done += n as usize;
    }
    true
}

fn pread_all(fd: usize, buf: &mut [u8], offset: usize) -> bool {
    let mut done = 0usize;
    while done < buf.len() {
        let n = pread64(fd, &mut buf[done..], offset + done);
        if n <= 0 {
            println!("disk_perf: pread64 failed: {}", n);
            return false;
        }
        done += n as usize;
    }
    true
}

fn bench_seq_write(path: &str, case: &Case, buf: &mut [u8]) -> Option<Report> {
    let fd = open(path, OpenFlags::CREATE | OpenFlags::TRUNC | OpenFlags::WRONLY);
    if fd < 0 {
        println!("disk_perf: open write {} failed: {}", path, fd);
        return None;
    }
    let fd = fd as usize;
    let start = get_time();
    let mut left = case.size;
    let mut bytes = 0usize;
    let mut ops = 0usize;
    let mut sum = 0u64;

    while left > 0 {
        let n = min(left, SEQ_BLOCK);
        fill_pattern(&mut buf[..n], ops as u8);
        if !write_all(fd, &buf[..n]) {
            let _ = close(fd);
            return None;
        }
        sum = sum.wrapping_add(checksum(&buf[..n]));
        bytes += n;
        ops += 1;
        left -= n;
    }
    let sync_ret = fsync(fd);
    let ms = elapsed_ms(start);
    let _ = close(fd);
    if sync_ret < 0 {
        println!("disk_perf: fsync after sequential write failed: {}", sync_ret);
        return None;
    }
    Some(Report {
        bytes,
        ops,
        ms,
        checksum: sum,
    })
}

fn bench_seq_read(path: &str, case: &Case, buf: &mut [u8]) -> Option<Report> {
    let fd = open(path, OpenFlags::RDONLY);
    if fd < 0 {
        println!("disk_perf: open read {} failed: {}", path, fd);
        return None;
    }
    let fd = fd as usize;
    let start = get_time();
    let mut left = case.size;
    let mut bytes = 0usize;
    let mut ops = 0usize;
    let mut sum = 0u64;

    while left > 0 {
        let n = min(left, SEQ_BLOCK);
        if !read_all(fd, &mut buf[..n]) {
            let _ = close(fd);
            return None;
        }
        sum = sum.wrapping_add(checksum(&buf[..n]));
        bytes += n;
        ops += 1;
        left -= n;
    }
    let ms = elapsed_ms(start);
    let _ = close(fd);
    Some(Report {
        bytes,
        ops,
        ms,
        checksum: sum,
    })
}

fn bench_rand_write(path: &str, case: &Case, buf: &mut [u8]) -> Option<Report> {
    let fd = open(path, OpenFlags::RDWR);
    if fd < 0 {
        println!("disk_perf: open random write {} failed: {}", path, fd);
        return None;
    }
    let fd = fd as usize;
    if ftruncate(fd, case.size as isize) < 0 {
        println!("disk_perf: ftruncate failed");
        let _ = close(fd);
        return None;
    }

    let blocks = case.size / RAND_BLOCK;
    let mut state = 0x9e37_79b9_7f4a_7c15u64 ^ case.size as u64;
    let start = get_time();
    let mut sum = 0u64;

    for op in 0..blocks {
        let block = (next_rand(&mut state) as usize) % blocks;
        let offset = block * RAND_BLOCK;
        fill_pattern(&mut buf[..RAND_BLOCK], (op ^ block) as u8);
        if !pwrite_all(fd, &buf[..RAND_BLOCK], offset) {
            let _ = close(fd);
            return None;
        }
        sum = sum.wrapping_add(checksum(&buf[..RAND_BLOCK]));
    }
    let sync_ret = fsync(fd);
    let ms = elapsed_ms(start);
    let _ = close(fd);
    if sync_ret < 0 {
        println!("disk_perf: fsync after random write failed: {}", sync_ret);
        return None;
    }
    Some(Report {
        bytes: blocks * RAND_BLOCK,
        ops: blocks,
        ms,
        checksum: sum,
    })
}

fn bench_rand_read(path: &str, case: &Case, buf: &mut [u8]) -> Option<Report> {
    let fd = open(path, OpenFlags::RDONLY);
    if fd < 0 {
        println!("disk_perf: open random read {} failed: {}", path, fd);
        return None;
    }
    let fd = fd as usize;
    let blocks = case.size / RAND_BLOCK;
    let mut state = 0x243f_6a88_85a3_08d3u64 ^ case.size as u64;
    let start = get_time();
    let mut sum = 0u64;

    for _ in 0..blocks {
        let block = (next_rand(&mut state) as usize) % blocks;
        let offset = block * RAND_BLOCK;
        if !pread_all(fd, &mut buf[..RAND_BLOCK], offset) {
            let _ = close(fd);
            return None;
        }
        sum = sum.wrapping_add(checksum(&buf[..RAND_BLOCK]));
    }
    let ms = elapsed_ms(start);
    let _ = close(fd);
    Some(Report {
        bytes: blocks * RAND_BLOCK,
        ops: blocks,
        ms,
        checksum: sum,
    })
}

#[no_mangle]
pub fn main(argc: usize, argv: &[&str]) -> i32 {
    let base = if argc > 1 { argv[1] } else { "." };
    let mut buf = [0u8; SEQ_BLOCK];

    println!(
        "disk_perf: base={}, seq_block={} KiB, rand_block={} KiB",
        base,
        SEQ_BLOCK / 1024,
        RAND_BLOCK / 1024
    );
    println!("disk_perf: write numbers include fsync; read numbers may benefit from cache");

    for case in CASES {
        let path = make_path(base, case);
        let _ = unlink(&path);

        if let Some(report) = bench_seq_write(&path, case, &mut buf) {
            print_rate(case, "seq write", report);
        } else {
            let _ = unlink(&path);
            return -1;
        }

        let _ = sync();

        if let Some(report) = bench_seq_read(&path, case, &mut buf) {
            print_rate(case, "seq read", report);
        } else {
            let _ = unlink(&path);
            return -1;
        }

        if let Some(report) = bench_rand_write(&path, case, &mut buf) {
            print_rate(case, "rand write", report);
        } else {
            let _ = unlink(&path);
            return -1;
        }

        let _ = sync();

        if let Some(report) = bench_rand_read(&path, case, &mut buf) {
            print_rate(case, "rand read", report);
        } else {
            let _ = unlink(&path);
            return -1;
        }

        let _ = unlink(&path);
    }

    println!("disk_perf: done");
    0
}
