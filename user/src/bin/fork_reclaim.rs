#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use core::cmp::min;
use user_lib::{
    close, exit, fork, get_time, getpid, lseek, open, read, sbrk, unlink, waitpid, write,
    OpenFlags, SEEK_SET,
};

const PAGE_SIZE: usize = 4096;
const MIB: usize = 1024 * 1024;

const DEFAULT_CACHE_MIB: usize = 128;
const DEFAULT_ANON_MIB: usize = 64;
const DEFAULT_ROUNDS: usize = 128;
const DEFAULT_CHUNK_KIB: usize = 64;

fn parse_arg(argv: &[&str], idx: usize, default: usize) -> usize {
    argv.get(idx)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(default)
}

fn unique_name(prefix: &str) -> String {
    format!("{}_{}_{}.bin", prefix, getpid(), get_time())
}

fn write_all(fd: usize, buf: &[u8]) -> bool {
    let mut written = 0usize;
    while written < buf.len() {
        let n = write(fd, &buf[written..]);
        if n <= 0 {
            println!(
                "[fork_reclaim] write failed: fd={} written={} len={} ret={}",
                fd,
                written,
                buf.len(),
                n
            );
            return false;
        }
        written += n as usize;
    }
    true
}

fn read_exact(fd: usize, buf: &mut [u8]) -> bool {
    let mut done = 0usize;
    while done < buf.len() {
        let n = read(fd, &mut buf[done..]);
        if n <= 0 {
            println!(
                "[fork_reclaim] read failed: fd={} done={} len={} ret={}",
                fd,
                done,
                buf.len(),
                n
            );
            return false;
        }
        done += n as usize;
    }
    true
}

fn verify_page(buf: &[u8], expected: u8, label: &str, round: usize) -> bool {
    for (idx, &byte) in buf.iter().enumerate() {
        if byte != expected {
            println!(
                "[fork_reclaim] verify failed: round={} label={} offset={} got={} expected={}",
                round,
                label,
                idx,
                byte,
                expected
            );
            return false;
        }
    }
    true
}

fn wait_child_ok(pid: isize, label: &str, round: usize) -> bool {
    let mut status = -1;
    let waited = waitpid(pid as usize, &mut status);
    if waited != pid {
        println!(
            "[fork_reclaim] waitpid mismatch: round={} label={} expected_pid={} waited={}",
            round,
            label,
            pid,
            waited
        );
        return false;
    }
    if status != 0 {
        println!(
            "[fork_reclaim] child failed: round={} label={} pid={} status={}",
            round,
            label,
            pid,
            status
        );
        return false;
    }
    true
}

fn create_control_file(path: &str) -> bool {
    let fd = open(path, OpenFlags::CREATE | OpenFlags::TRUNC | OpenFlags::RDWR);
    if fd < 0 {
        println!("[fork_reclaim] open control file failed: path={} ret={}", path, fd);
        return false;
    }
    let fd = fd as usize;
    let page_a = [b'a'; PAGE_SIZE];
    let page_b = [b'b'; PAGE_SIZE];
    let page_c = [b'c'; PAGE_SIZE];
    let ok = write_all(fd, &page_a) && write_all(fd, &page_b) && write_all(fd, &page_c);
    let close_ret = close(fd);
    if close_ret != 0 {
        println!(
            "[fork_reclaim] close control file failed: path={} ret={}",
            path, close_ret
        );
        return false;
    }
    ok
}

fn fill_dirty_page_cache(path: &str, total_bytes: usize, chunk_bytes: usize) -> bool {
    let fd = open(path, OpenFlags::CREATE | OpenFlags::TRUNC | OpenFlags::RDWR);
    if fd < 0 {
        println!(
            "[fork_reclaim] open pressure file failed: path={} ret={}",
            path, fd
        );
        return false;
    }
    let fd = fd as usize;
    let mut chunk = vec![0u8; chunk_bytes];
    for (idx, byte) in chunk.iter_mut().enumerate() {
        *byte = b'P'.wrapping_add((idx / PAGE_SIZE) as u8);
    }

    let mut written = 0usize;
    let mut next_report = MIB;
    while written < total_bytes {
        let now = min(chunk.len(), total_bytes - written);
        if !write_all(fd, &chunk[..now]) {
            let _ = close(fd);
            return false;
        }
        written += now;
        if written >= next_report || written == total_bytes {
            println!(
                "[fork_reclaim] dirty cache progress: {} / {} KiB",
                written / 1024,
                total_bytes / 1024
            );
            next_report += MIB;
        }
    }

    let close_ret = close(fd);
    if close_ret != 0 {
        println!(
            "[fork_reclaim] close pressure file failed: path={} ret={}",
            path, close_ret
        );
        return false;
    }
    true
}

fn reserve_anon_memory(total_bytes: usize) -> usize {
    if total_bytes == 0 {
        return 0;
    }
    if total_bytes > i32::MAX as usize {
        println!(
            "[fork_reclaim] anon target too large for sbrk: {} > {}",
            total_bytes,
            i32::MAX
        );
        return 0;
    }
    let base = sbrk(total_bytes as i32);
    if base < 0 {
        println!(
            "[fork_reclaim] sbrk failed: bytes={} ret={}",
            total_bytes, base
        );
        return 0;
    }

    let ptr = base as *mut u8;
    let mut touched = 0usize;
    let mut next_report = MIB;
    while touched < total_bytes {
        unsafe {
            let page = ptr.add(touched);
            page.write_volatile(((touched / PAGE_SIZE) as u8).wrapping_mul(17).wrapping_add(3));
            let _ = page.read_volatile();
        }
        touched += PAGE_SIZE;
        if touched >= next_report || touched >= total_bytes {
            println!(
                "[fork_reclaim] anon touch progress: {} / {} KiB",
                touched / 1024,
                total_bytes / 1024
            );
            next_report += MIB;
        }
    }
    total_bytes
}

fn run_shared_fd_round(fd: usize, round: usize) -> bool {
    let seek_ret = lseek(fd, 0, SEEK_SET);
    if seek_ret != 0 {
        println!(
            "[fork_reclaim] round {}: reset lseek failed: ret={}",
            round, seek_ret
        );
        return false;
    }

    let pid1 = fork();
    if pid1 < 0 {
        println!("[fork_reclaim] round {}: first fork failed: ret={}", round, pid1);
        return false;
    }
    if pid1 == 0 {
        let ret = lseek(fd, PAGE_SIZE as isize, SEEK_SET);
        exit(if ret == PAGE_SIZE as isize { 0 } else { 11 });
    }
    if !wait_child_ok(pid1, "child_lseek", round) {
        return false;
    }

    let mut page_b = [0u8; PAGE_SIZE];
    if !read_exact(fd, &mut page_b) || !verify_page(&page_b, b'b', "parent_read_b", round) {
        return false;
    }

    let pid2 = fork();
    if pid2 < 0 {
        println!("[fork_reclaim] round {}: second fork failed: ret={}", round, pid2);
        return false;
    }
    if pid2 == 0 {
        let mut page_c = [0u8; PAGE_SIZE];
        if !read_exact(fd, &mut page_c) || !verify_page(&page_c, b'c', "child_read_c", round) {
            exit(22);
        }
        exit(0);
    }
    wait_child_ok(pid2, "child_read", round)
}

#[no_mangle]
pub fn main(_argc: usize, argv: &[&str]) -> i32 {
    let cache_mib = parse_arg(argv, 1, DEFAULT_CACHE_MIB);
    let anon_mib = parse_arg(argv, 2, DEFAULT_ANON_MIB);
    let rounds = parse_arg(argv, 3, DEFAULT_ROUNDS);
    let chunk_kib = parse_arg(argv, 4, DEFAULT_CHUNK_KIB).max(4);
    let chunk_bytes = (chunk_kib * 1024 / PAGE_SIZE).max(1) * PAGE_SIZE;
    let cache_bytes = cache_mib * MIB;
    let anon_bytes = anon_mib * MIB;

    let pressure_path = unique_name("fork_reclaim_pressure");
    let control_path = unique_name("fork_reclaim_control");

    println!(
        "[fork_reclaim] start: cache_mib={} anon_mib={} rounds={} chunk_kib={}",
        cache_mib, anon_mib, rounds, chunk_kib
    );
    println!(
        "[fork_reclaim] files: pressure={} control={}",
        pressure_path,
        control_path
    );
    println!(
        "[fork_reclaim] strategy: dirty page cache -> touch anonymous memory -> repeat fork10-style shared-fd checks"
    );

    if !create_control_file(control_path.as_str()) {
        return 1;
    }
    if !fill_dirty_page_cache(pressure_path.as_str(), cache_bytes, chunk_bytes) {
        let _ = unlink(control_path.as_str());
        return 2;
    }

    let touched = reserve_anon_memory(anon_bytes);
    println!(
        "[fork_reclaim] anon memory touched: {} / {} KiB",
        touched / 1024,
        anon_bytes / 1024
    );

    let fd = open(control_path.as_str(), OpenFlags::RDONLY);
    if fd < 0 {
        println!(
            "[fork_reclaim] reopen control file failed: path={} ret={}",
            control_path, fd
        );
        let _ = unlink(control_path.as_str());
        let _ = unlink(pressure_path.as_str());
        return 3;
    }
    let fd = fd as usize;

    for round in 0..rounds {
        if round == 0 || (round + 1) % 8 == 0 {
            println!("[fork_reclaim] round {} / {}", round + 1, rounds);
        }
        if !run_shared_fd_round(fd, round + 1) {
            println!("[fork_reclaim] stop after failure at round {}", round + 1);
            let _ = close(fd);
            let _ = unlink(control_path.as_str());
            let _ = unlink(pressure_path.as_str());
            return 4;
        }
    }

    let close_ret = close(fd);
    if close_ret != 0 {
        println!("[fork_reclaim] close control fd failed: ret={}", close_ret);
        return 5;
    }

    let _ = unlink(control_path.as_str());
    let _ = unlink(pressure_path.as_str());

    println!(
        "[fork_reclaim] completed: cache_mib={} anon_mib={} rounds={}",
        cache_mib, anon_mib, rounds
    );
    0
}
