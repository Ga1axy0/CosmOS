#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{
    close, exit, fork, fsync, get_time, getpid, open, read, unlink, wait, write, OpenFlags,
};

const DEFAULT_IO_PROCS: usize = 4;
const DEFAULT_MIB_PER_PROC: usize = 16;
const DEFAULT_BLOCK_SIZE: usize = 4096;
const DEFAULT_CPU_MS: isize = 8000;
const BASELINE_CPU_MS: isize = 1000;
const MAX_BLOCK_SIZE: usize = 16 * 1024;
const CPU_TIME_CHECK_INTERVAL: u64 = 4096;

fn parse_usize(s: &str, default: usize) -> usize {
    let mut value = 0usize;
    let mut any = false;
    for byte in s.as_bytes() {
        if !byte.is_ascii_digit() {
            return default;
        }
        any = true;
        value = value.saturating_mul(10).saturating_add((byte - b'0') as usize);
    }
    if any { value } else { default }
}

fn parse_isize(s: &str, default: isize) -> isize {
    parse_usize(s, default as usize) as isize
}

fn write_all(fd: usize, buf: &[u8]) -> bool {
    let mut done = 0usize;
    while done < buf.len() {
        let n = write(fd, &buf[done..]);
        if n <= 0 {
            println!("[io_blocking_bench] write failed: ret={}", n);
            return false;
        }
        done += n as usize;
    }
    true
}

fn read_all(fd: usize, buf: &mut [u8]) -> usize {
    let mut done = 0usize;
    while done < buf.len() {
        let n = read(fd, &mut buf[done..]);
        if n <= 0 {
            break;
        }
        done += n as usize;
    }
    done
}

fn fill_block(buf: &mut [u8], child: usize, seq: usize) {
    let seed = (child as u8)
        .wrapping_mul(17)
        .wrapping_add((seq as u8).wrapping_mul(31));
    for (i, byte) in buf.iter_mut().enumerate() {
        *byte = seed.wrapping_add((i as u8).wrapping_mul(13));
    }
}

fn write_number(path: &str, value: u64) {
    let fd = open(path, OpenFlags::CREATE | OpenFlags::TRUNC | OpenFlags::WRONLY);
    if fd < 0 {
        println!("[io_blocking_bench] open result {} failed: {}", path, fd);
        return;
    }
    let mut buf = [0u8; 32];
    let mut n = value;
    let mut pos = buf.len();
    if n == 0 {
        pos -= 1;
        buf[pos] = b'0';
    } else {
        while n != 0 && pos > 0 {
            pos -= 1;
            buf[pos] = b'0' + (n % 10) as u8;
            n /= 10;
        }
    }
    let _ = write_all(fd as usize, &buf[pos..]);
    let _ = close(fd as usize);
}

fn read_number(path: &str) -> u64 {
    let fd = open(path, OpenFlags::RDONLY);
    if fd < 0 {
        return 0;
    }
    let mut buf = [0u8; 32];
    let n = read_all(fd as usize, &mut buf);
    let _ = close(fd as usize);
    let mut value = 0u64;
    for &byte in &buf[..n] {
        if !byte.is_ascii_digit() {
            break;
        }
        value = value.saturating_mul(10).saturating_add((byte - b'0') as u64);
    }
    value
}

fn io_path(child: usize) -> &'static str {
    match child {
        0 => "io_blocking_bench_0.dat",
        1 => "io_blocking_bench_1.dat",
        2 => "io_blocking_bench_2.dat",
        3 => "io_blocking_bench_3.dat",
        4 => "io_blocking_bench_4.dat",
        5 => "io_blocking_bench_5.dat",
        6 => "io_blocking_bench_6.dat",
        _ => "io_blocking_bench_7.dat",
    }
}

fn io_result_path(child: usize) -> &'static str {
    match child {
        0 => "io_blocking_bench_0.res",
        1 => "io_blocking_bench_1.res",
        2 => "io_blocking_bench_2.res",
        3 => "io_blocking_bench_3.res",
        4 => "io_blocking_bench_4.res",
        5 => "io_blocking_bench_5.res",
        6 => "io_blocking_bench_6.res",
        _ => "io_blocking_bench_7.res",
    }
}

fn io_child(child: usize, total_bytes: usize, block_size: usize) -> ! {
    let path = io_path(child);
    let _ = unlink(path);
    let fd = open(path, OpenFlags::CREATE | OpenFlags::TRUNC | OpenFlags::WRONLY);
    if fd < 0 {
        println!("[io_blocking_bench] child{} open failed: {}", child, fd);
        exit(1);
    }
    let fd = fd as usize;
    let start = get_time();
    let mut buf = [0u8; MAX_BLOCK_SIZE];
    let mut written = 0usize;
    let mut seq = 0usize;
    while written < total_bytes {
        let n = core::cmp::min(block_size, total_bytes - written);
        fill_block(&mut buf[..n], child, seq);
        if !write_all(fd, &buf[..n]) {
            let _ = close(fd);
            exit(2);
        }
        written += n;
        seq += 1;
    }
    let sync_ret = fsync(fd);
    let elapsed = core::cmp::max(1, get_time() - start) as u64;
    let _ = close(fd);
    if sync_ret < 0 {
        println!("[io_blocking_bench] child{} fsync failed: {}", child, sync_ret);
        exit(3);
    }
    write_number(io_result_path(child), elapsed);
    exit(0);
}

fn burn_cpu_for(run_ms: isize) -> u64 {
    let deadline = get_time() + core::cmp::max(1, run_ms);
    let mut iters = 0u64;
    let mut state = 0x1234_5678_9abc_def0u64 ^ getpid() as u64;
    loop {
        for _ in 0..CPU_TIME_CHECK_INTERVAL {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            iters = iters.wrapping_add(1);
        }
        if get_time() >= deadline {
            break;
        }
    }
    core::hint::black_box(state);
    iters
}

fn cpu_child(run_ms: isize) -> ! {
    let iters = burn_cpu_for(run_ms);
    write_number("io_blocking_bench_cpu.res", iters);
    exit(0);
}

fn cleanup(io_procs: usize) {
    let _ = unlink("io_blocking_bench_cpu.res");
    let limit = core::cmp::min(io_procs, 8);
    for child in 0..limit {
        let _ = unlink(io_path(child));
        let _ = unlink(io_result_path(child));
    }
}

#[no_mangle]
fn main(argc: usize, argv: &[&str]) -> i32 {
    let mut io_procs = if argc > 1 {
        parse_usize(argv[1], DEFAULT_IO_PROCS)
    } else {
        DEFAULT_IO_PROCS
    };
    io_procs = core::cmp::max(1, core::cmp::min(io_procs, 8));
    let mib_per_proc = if argc > 2 {
        parse_usize(argv[2], DEFAULT_MIB_PER_PROC)
    } else {
        DEFAULT_MIB_PER_PROC
    };
    let mut block_size = if argc > 3 {
        parse_usize(argv[3], DEFAULT_BLOCK_SIZE)
    } else {
        DEFAULT_BLOCK_SIZE
    };
    block_size = core::cmp::max(512, core::cmp::min(block_size, MAX_BLOCK_SIZE));
    let cpu_ms = if argc > 4 {
        parse_isize(argv[4], DEFAULT_CPU_MS)
    } else {
        DEFAULT_CPU_MS
    };

    cleanup(io_procs);

    let total_bytes = mib_per_proc * 1024 * 1024;
    println!(
        "[io_blocking_bench] start: io_procs={} mib_per_proc={} block={} cpu_ms={}",
        io_procs, mib_per_proc, block_size, cpu_ms
    );
    println!(
        "[io_blocking_bench] cpu baseline: {} ms without io pressure",
        BASELINE_CPU_MS
    );
    let baseline_iters = burn_cpu_for(BASELINE_CPU_MS);
    let baseline_per_ms = baseline_iters / BASELINE_CPU_MS as u64;
    println!(
        "[io_blocking_bench] cpu_baseline_iters={} cpu_baseline_iters_per_ms={}",
        baseline_iters, baseline_per_ms
    );
    println!(
        "[io_blocking_bench] compare cpu_retention_pct and io_makespan_ms; higher retention and lower makespan are better"
    );

    let start = get_time();
    let cpu_pid = fork();
    if cpu_pid == 0 {
        cpu_child(cpu_ms);
    }
    if cpu_pid < 0 {
        println!("[io_blocking_bench] fork cpu child failed: {}", cpu_pid);
        return 1;
    }

    for child in 0..io_procs {
        let pid = fork();
        if pid == 0 {
            io_child(child, total_bytes, block_size);
        }
        if pid < 0 {
            println!("[io_blocking_bench] fork io child{} failed: {}", child, pid);
            return 1;
        }
    }

    let mut status = 0;
    for _ in 0..(io_procs + 1) {
        let waited = wait(&mut status);
        if waited < 0 {
            println!("[io_blocking_bench] wait failed: {}", waited);
            return 1;
        }
        if status != 0 {
            println!("[io_blocking_bench] child pid={} status={}", waited, status);
        }
    }
    let elapsed = core::cmp::max(1, get_time() - start) as u64;

    let mut io_child_ms_sum = 0u64;
    let mut io_child_ms_max = 0u64;
    for child in 0..io_procs {
        let ms = read_number(io_result_path(child));
        io_child_ms_sum = io_child_ms_sum.saturating_add(ms);
        io_child_ms_max = core::cmp::max(io_child_ms_max, ms);
    }
    let cpu_iters = read_number("io_blocking_bench_cpu.res");
    let total_mib = io_procs as u64 * mib_per_proc as u64;
    let wall_throughput_x100 = total_mib.saturating_mul(100_000) / elapsed;
    let io_throughput_x100 = total_mib.saturating_mul(100_000) / core::cmp::max(1, io_child_ms_max);
    let cpu_per_ms = cpu_iters / core::cmp::max(1, cpu_ms as u64);
    let retention_x100 = cpu_per_ms.saturating_mul(10_000) / core::cmp::max(1, baseline_per_ms);

    println!(
        "[io_blocking_bench] result: wall_ms={} total_io={} MiB wall_throughput={}.{:02} MiB/s",
        elapsed,
        total_mib,
        wall_throughput_x100 / 100,
        wall_throughput_x100 % 100
    );
    println!(
        "[io_blocking_bench] io_child_ms: avg={} max={} io_throughput={}.{:02} MiB/s",
        io_child_ms_sum / io_procs as u64,
        io_child_ms_max,
        io_throughput_x100 / 100,
        io_throughput_x100 % 100
    );
    println!(
        "[io_blocking_bench] cpu_iters={} cpu_iters_per_ms={} cpu_retention_pct={}.{:02}",
        cpu_iters,
        cpu_per_ms,
        retention_x100 / 100,
        retention_x100 % 100
    );
    println!(
        "[io_blocking_bench] note: wall_throughput is capped by fixed cpu_ms; use io_throughput for I/O speed"
    );

    cleanup(io_procs);
    0
}
