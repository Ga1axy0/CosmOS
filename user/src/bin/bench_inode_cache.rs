#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use user_lib::{close, fstat, link, mkdir, open, read, unlink, write, OpenFlags};

const FILE_SIZE_MB: usize = 32;
const FILE_SIZE: usize = FILE_SIZE_MB * 1024 * 1024;
const BUF_SIZE: usize = 256 * 1024; // 256 KiB read buffer
const NUM_LINKS: usize = 10;
const BENCH_DIR: &str = "/bench_inode";

/// Wall-clock time in milliseconds since boot.
fn now_ms() -> isize {
    // get_time() returns ms; clock_gettime_ns for ns precision
    user_lib::get_time()
}

/// Read an entire file from the given path, discarding the data, and return
/// the elapsed wall-clock time in milliseconds.
fn timed_read_all(path: &str) -> isize {
    let fd = open(path, OpenFlags::RDONLY);
    if fd < 0 {
        println!("ERROR: open({}) -> {}", path, fd);
        return -1;
    }
    let fd = fd as usize;

    let mut buf = [0u8; BUF_SIZE];
    let start = now_ms();

    loop {
        let n = read(fd, &mut buf);
        if n < 0 {
            println!("ERROR: read({}) -> {}", path, n);
            close(fd);
            return -1;
        }
        if n == 0 {
            break; // EOF
        }
    }

    let elapsed = now_ms() - start;
    close(fd);
    elapsed
}

/// Create a file filled with a repeating 64-byte pattern so that identical
/// hard-link reads produce deterministic data.
fn create_base_file(path: &str) -> bool {
    let fd = open(path, OpenFlags::CREATE | OpenFlags::TRUNC | OpenFlags::WRONLY);
    if fd < 0 {
        println!("ERROR: create({}) -> {}", path, fd);
        return false;
    }
    let fd = fd as usize;

    let pattern: [u8; 64] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B,
        0x0C, 0x0D, 0x0E, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
        0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23,
        0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x2B, 0x2C, 0x2D, 0x2E, 0x2F,
        0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x3B,
        0x3C, 0x3D, 0x3E, 0x3F,
    ];

    let mut written: usize = 0;
    while written < FILE_SIZE {
        let chunk = BUF_SIZE.min(FILE_SIZE - written);
        let rem = chunk % pattern.len();
        // Write full pattern repeats, then partial.
        let full_blocks = chunk / pattern.len();
        for _ in 0..full_blocks {
            let n = write(fd, &pattern);
            if n as usize != pattern.len() {
                println!(
                    "ERROR: write base file at {} bytes -> {}",
                    written, n
                );
                close(fd);
                return false;
            }
            written += pattern.len();
        }
        if rem > 0 {
            let n = write(fd, &pattern[..rem]);
            if n as usize != rem {
                println!(
                    "ERROR: write base file rem at {} bytes -> {}",
                    written, n
                );
                close(fd);
                return false;
            }
            written += rem;
        }
    }

    close(fd);
    println!(
        "  Created base file ({} MiB) at {}",
        FILE_SIZE_MB, path
    );
    true
}

/// Verify a file has the expected size (basic sanity check via fstat).
fn verify_size(path: &str, expected: usize) -> bool {
    let fd = open(path, OpenFlags::RDONLY);
    if fd < 0 {
        return false;
    }
    let mut st = user_lib::Stat::default();
    let ret = fstat(fd as usize, &mut st);
    close(fd as usize);
    ret == 0 && (st.size as usize) == expected
}

#[no_mangle]
pub fn main(_argc: usize, _argv: &[&str]) -> i32 {
    println!("=== Inode Cache Benchmark ===");
    println!(
        "File size: {} MiB, number of hard links: {}",
        FILE_SIZE_MB, NUM_LINKS
    );
    println!("");

    // ---- Setup ----
    println!("[Setup] Creating benchmark directory {}", BENCH_DIR);
    if mkdir(BENCH_DIR, 0o755) < 0 {
        // Directory may already exist from a previous run — that's fine.
        println!("  (directory may already exist, continuing)");
    }

    let base_path = "/bench_inode/base_file";
    // Clean up leftover files from a previous run.
    let _ = unlink(base_path);
    for i in 0..NUM_LINKS {
        let link_path = alloc::format!("/bench_inode/link_{}", i);
        let _ = unlink(&link_path);
    }

    if !create_base_file(base_path) {
        println!("FATAL: failed to create base file");
        return -1;
    }

    // Verify the base file has the correct size.
    if !verify_size(base_path, FILE_SIZE) {
        println!("FATAL: base file size mismatch");
        return -1;
    }
    println!("  Base file size verified: {} bytes", FILE_SIZE);

    // Create hard links.
    println!("[Setup] Creating {} hard links...", NUM_LINKS);
    for i in 0..NUM_LINKS {
        let link_path = alloc::format!("/bench_inode/link_{}", i);
        let ret = link(base_path, &link_path);
        if ret < 0 {
            println!("ERROR: link({}, {}) -> {}", base_path, link_path, ret);
            return -1;
        }
    }
    println!("  All {} hard links created.", NUM_LINKS);
    println!("");

    // ---- Warm-up: read base file once to populate page cache for its inode ----
    println!("[Warm-up] First read of {} (populating caches)...", base_path);
    let warmup_ms = timed_read_all(base_path);
    if warmup_ms < 0 {
        return -1;
    }
    println!("  Warm-up read: {} ms", warmup_ms);
    println!("");

    // ---- Benchmark: read each hard link ----
    println!("[Benchmark] Reading {} hard links sequentially...", NUM_LINKS);
    println!("{:<20} {:>12} {:>12}", "Path", "Time(ms)", "Speed(MiB/s)");
    println!("{:-<20} {:-<12} {:-<12}", "", "", "");

    let mut total_ms: i64 = 0;
    for i in 0..NUM_LINKS {
        let link_path = alloc::format!("/bench_inode/link_{}", i);
        let elapsed = timed_read_all(&link_path);
        if elapsed < 0 {
            return -1;
        }
        let speed = if elapsed > 0 {
            (FILE_SIZE_MB as i64) * 1000 / (elapsed as i64)
        } else {
            0
        };
        println!(
            "{:<20} {:>8} ms  {:>8} MiB/s",
            alloc::format!("link_{}", i),
            elapsed,
            speed
        );
        total_ms += elapsed as i64;
    }

    let avg_ms = total_ms / (NUM_LINKS as i64);
    let avg_speed = if avg_ms > 0 {
        (FILE_SIZE_MB as i64) * 1000 / avg_ms
    } else {
        0
    };
    println!("{:-<20} {:-<12} {:-<12}", "", "", "");
    println!(
        "{:<20} {:>8} ms  {:>8} MiB/s",
        "AVERAGE", avg_ms, avg_speed
    );
    println!("");

    // ---- Key metric: first vs subsequent ----
    // With inode cache: link_0 shares the page cache populated by the warm-up
    // read of base_file (same Arc<Inode>). It should be fast (~RAM speed).
    // Without inode cache: link_0 gets a NEW Arc<Inode> with empty page cache,
    // and must re-read the full 32 MiB from disk.

    println!("=== Analysis ===");
    println!(
        "If inode cache is ENABLED: link_0 should be fast (< ~100 ms for {} MiB from RAM)",
        FILE_SIZE_MB
    );
    println!(
        "If inode cache is DISABLED: link_0 must re-read {} MiB from disk (~{} ms depending on disk speed)",
        FILE_SIZE_MB, FILE_SIZE_MB * 10
    );
    println!("");

    // ---- Cleanup ----
    println!("[Cleanup] Removing test files...");
    for i in 0..NUM_LINKS {
        let link_path = alloc::format!("/bench_inode/link_{}", i);
        let _ = unlink(&link_path);
    }
    let _ = unlink(base_path);
    println!("  Cleanup done.");

    println!("=== Benchmark complete ===");
    0
}
