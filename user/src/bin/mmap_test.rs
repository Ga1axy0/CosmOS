#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
extern crate user_lib;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use user_lib::{
    close, exit, fork, get_time, getpid, mmap_full, munmap, open, read, unlink, waitpid, write,
    MMapFlags, MMapProt, OpenFlags, SIGBUS,
};

const PAGE_SIZE: usize = 4096;

/// 生成一个尽量避免冲突的测试文件名。
fn unique_name(prefix: &str) -> String {
    format!("{}_{}_{}", prefix, getpid(), get_time())
}

/// 将 ASCII 字节串按字符串形式打印，便于观察测试中的读写内容。
fn print_ascii(label: &str, bytes: &[u8]) {
    let text = core::str::from_utf8(bytes).unwrap_or("<non-utf8>");
    println!("    {}: {}", label, text);
}

/// 重新打开文件并读取前 `buf.len()` 字节。
fn reopen_and_read(path: &str, buf: &mut [u8]) -> isize {
    let fd = open(path, OpenFlags::RDONLY);
    assert!(fd >= 0, "reopen {} failed: {}", path, fd);
    let fd = fd as usize;
    let n = read(fd, buf);
    assert_eq!(close(fd), 0);
    n
}

/// 返回当前时间戳（毫秒）。
fn now_ms() -> usize {
    let t = get_time();
    assert!(t >= 0, "get_time failed: {}", t);
    t as usize
}

/// 从命令行参数里读取一个 `usize`，不存在或解析失败时返回默认值。
fn parse_arg(argv: &[&str], idx: usize, default: usize) -> usize {
    argv.get(idx)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(default)
}

/// 清理一批已成功建立的映射，避免 benchmark 中途失败后泄漏。
fn unmap_all(addrs: &[usize], len: usize) {
    for &addr in addrs {
        assert_eq!(munmap(addr, len), 0, "munmap failed at {:#x}", addr);
    }
}

/// 统计一轮“很多小映射”的耗时，并返回是否成功。
fn bench_many_anon_maps(count: usize, rounds: usize) -> bool {
    println!(
        "[bench] many anonymous mmaps: count={} rounds={} page_size={}",
        count, rounds, PAGE_SIZE
    );

    let mut total_map_ms = 0usize;
    let mut total_touch_ms = 0usize;
    let mut total_unmap_ms = 0usize;
    let mut total_checksum = 0usize;

    for round in 0..rounds {
        let mut addrs = Vec::with_capacity(count);
        let map_begin = now_ms();
        let mut failed = false;

        for idx in 0..count {
            let addr = mmap_full(
                0,
                PAGE_SIZE,
                MMapProt::PROT_READ | MMapProt::PROT_WRITE,
                MMapFlags::MAP_PRIVATE | MMapFlags::MAP_ANONYMOUS,
                0,
                0,
            );
            if addr <= 0 {
                println!(
                    "[bench] many anonymous mmaps round {} failed at {} / {} with ret={}",
                    round + 1,
                    idx,
                    count,
                    addr
                );
                failed = true;
                break;
            }
            addrs.push(addr as usize);
        }

        let map_ms = now_ms() - map_begin;
        total_map_ms += map_ms;

        if failed {
            unmap_all(&addrs, PAGE_SIZE);
            return false;
        }

        let touch_begin = now_ms();
        let mut checksum = 0usize;
        for (idx, &addr) in addrs.iter().enumerate() {
            unsafe {
                let ptr = addr as *mut u8;
                let value = ((idx as u8).wrapping_mul(31)).wrapping_add(7);
                core::ptr::write_volatile(ptr, value);
                checksum = checksum.wrapping_add(core::ptr::read_volatile(ptr) as usize);
            }
        }
        let touch_ms = now_ms() - touch_begin;
        total_touch_ms += touch_ms;
        total_checksum = total_checksum.wrapping_add(checksum);

        let unmap_begin = now_ms();
        unmap_all(&addrs, PAGE_SIZE);
        let unmap_ms = now_ms() - unmap_begin;
        total_unmap_ms += unmap_ms;

        println!(
            "[bench] round {}/{}: map={}ms touch={}ms unmap={}ms checksum={}",
            round + 1,
            rounds,
            map_ms,
            touch_ms,
            unmap_ms,
            checksum
        );
    }

    println!(
        "[bench] many anonymous mmaps summary: count={} rounds={} avg_map={}ms avg_touch={}ms avg_unmap={}ms checksum={}",
        count,
        rounds,
        total_map_ms / rounds.max(1),
        total_touch_ms / rounds.max(1),
        total_unmap_ms / rounds.max(1),
        total_checksum
    );
    true
}

/// 统计单个大映射的耗时，方便和“很多小映射”对比。
fn bench_one_big_anon_map(count: usize, rounds: usize) -> bool {
    let len = match count.checked_mul(PAGE_SIZE) {
        Some(v) => v,
        None => {
            println!("[bench] big anonymous mmap skipped: count overflow");
            return false;
        }
    };

    println!(
        "[bench] one big anonymous mmap: pages={} len={} rounds={}",
        count, len, rounds
    );

    let mut total_ms = 0usize;
    let mut total_checksum = 0usize;

    for round in 0..rounds {
        let begin = now_ms();
        let addr = mmap_full(
            0,
            len,
            MMapProt::PROT_READ | MMapProt::PROT_WRITE,
            MMapFlags::MAP_PRIVATE | MMapFlags::MAP_ANONYMOUS,
            0,
            0,
        );
        assert!(addr > 0, "big anonymous mmap failed: {}", addr);

        let mut checksum = 0usize;
        for idx in 0..count {
            unsafe {
                let ptr = (addr as usize + idx * PAGE_SIZE) as *mut u8;
                let value = ((idx as u8).wrapping_mul(17)).wrapping_add(3);
                core::ptr::write_volatile(ptr, value);
                checksum = checksum.wrapping_add(core::ptr::read_volatile(ptr) as usize);
            }
        }

        assert_eq!(munmap(addr as usize, len), 0, "big mmap munmap failed");
        let elapsed = now_ms() - begin;
        total_ms += elapsed;
        total_checksum = total_checksum.wrapping_add(checksum);

        println!(
            "[bench] big round {}/{}: total={}ms checksum={}",
            round + 1,
            rounds,
            elapsed,
            checksum
        );
    }

    println!(
        "[bench] big anonymous mmap summary: pages={} rounds={} avg_total={}ms checksum={}",
        count,
        rounds,
        total_ms / rounds.max(1),
        total_checksum
    );
    true
}

/// 运行一组按规模递增的 mmap 性能测试。
fn run_mmap_perf_suite(max_pages: usize, rounds: usize) {
    let max_pages = max_pages.max(1);
    let rounds = rounds.max(1);
    let candidates = [
        core::cmp::max(1, max_pages / 4),
        core::cmp::max(1, max_pages / 2),
        max_pages,
    ];

    println!(
        "[bench] start mmap perf suite: max_pages={} rounds={} candidates={:?}",
        max_pages, rounds, candidates
    );

    let mut last_pages = 0usize;
    for &pages in &candidates {
        if pages == last_pages {
            continue;
        }
        last_pages = pages;
        if !bench_many_anon_maps(pages, rounds) {
            println!("[bench] stop after many-mmap benchmark failure");
            return;
        }
        if !bench_one_big_anon_map(pages, rounds) {
            println!("[bench] stop after big-mmap benchmark failure");
            return;
        }
    }
}

/// 测试 `MAP_SHARED` 的基本共享语义。
fn case_mmap_shared_basic() {
    println!("[suite] case_mmap_shared_basic");
    let name = unique_name("suite_sh");
    let fd = open(name.as_str(), OpenFlags::CREATE | OpenFlags::RDWR);
    assert!(fd >= 0, "open failed: {}", fd);
    let fd = fd as usize;

    let initial = b"hello_shared_page_cache";
    print_ascii("initial file bytes", initial);
    assert_eq!(write(fd, initial), initial.len() as isize);

    let addr = mmap_full(
        0,
        PAGE_SIZE,
        MMapProt::PROT_READ | MMapProt::PROT_WRITE,
        MMapFlags::MAP_SHARED,
        fd,
        0,
    );
    assert!(addr > 0, "mmap shared failed: {}", addr);
    assert_eq!(close(fd), 0, "close after mmap should succeed");

    let page = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, PAGE_SIZE) };
    print_ascii("mapped bytes before write", &page[..initial.len()]);
    assert_eq!(&page[..initial.len()], initial, "shared mapping initial read mismatch");

    page[0] = b'H';
    page[6] = b'S';
    page[7] = b'H';
    print_ascii("mapped bytes after write", &page[..initial.len()]);

    let mut buf = [0u8; 32];
    let n = reopen_and_read(name.as_str(), &mut buf);
    assert!(n >= initial.len() as isize, "reopen read too short: {}", n);
    print_ascii("reopen read bytes", &buf[..initial.len()]);
    assert_eq!(&buf[..5], b"Hello");
    assert_eq!(&buf[6..8], b"SH");

    assert_eq!(munmap(addr as usize, PAGE_SIZE), 0, "munmap failed");
    assert_eq!(unlink(name.as_str()), 0, "unlink failed");
}

/// 测试 `MAP_PRIVATE` 的首次读后写物化语义。
fn case_mmap_private_basic() {
    println!("[suite] case_mmap_private_basic");

    let name_read_then_write = unique_name("suite_pr1");
    let fd = open(name_read_then_write.as_str(), OpenFlags::CREATE | OpenFlags::RDWR);
    assert!(fd >= 0, "open failed: {}", fd);
    let fd = fd as usize;
    let initial = b"abcdef_private_mapping";
    print_ascii("initial private file bytes", initial);
    assert_eq!(write(fd, initial), initial.len() as isize);
    let addr = mmap_full(
        0,
        PAGE_SIZE,
        MMapProt::PROT_READ | MMapProt::PROT_WRITE,
        MMapFlags::MAP_PRIVATE,
        fd,
        0,
    );
    assert!(addr > 0, "mmap private failed: {}", addr);
    assert_eq!(close(fd), 0);

    let page = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, PAGE_SIZE) };
    print_ascii("mapped private bytes before write", &page[..initial.len()]);
    assert_eq!(&page[..initial.len()], initial, "private mapping initial read mismatch");
    page[0] = b'Z';
    page[1] = b'Y';
    print_ascii("mapped private bytes after write", &page[..initial.len()]);

    let mut buf = [0u8; 32];
    let n = reopen_and_read(name_read_then_write.as_str(), &mut buf);
    assert!(n >= initial.len() as isize);
    print_ascii("file bytes after private write", &buf[..initial.len()]);
    assert_eq!(&buf[..initial.len()], initial, "MAP_PRIVATE write should not modify file");
    assert_eq!(munmap(addr as usize, PAGE_SIZE), 0);
    assert_eq!(unlink(name_read_then_write.as_str()), 0);
}

/// 测试 `MAP_PRIVATE` 第一次访问就是写时的物化语义。
fn case_mmap_private_first_write() {
    println!("[suite] case_mmap_private_first_write");
    let name = unique_name("suite_pr2");
    let fd = open(name.as_str(), OpenFlags::CREATE | OpenFlags::RDWR);
    assert!(fd >= 0, "open failed: {}", fd);
    let fd = fd as usize;
    let initial = b"qrstuv_first_write";
    print_ascii("initial file bytes", initial);
    assert_eq!(write(fd, initial), initial.len() as isize);

    let addr = mmap_full(
        0,
        PAGE_SIZE,
        MMapProt::PROT_READ | MMapProt::PROT_WRITE,
        MMapFlags::MAP_PRIVATE,
        fd,
        0,
    );
    assert!(addr > 0, "mmap private failed: {}", addr);
    assert_eq!(close(fd), 0);

    let page = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, PAGE_SIZE) };
    page[0] = b'Q';
    page[1] = b'Q';
    print_ascii("mapped bytes after first write", &page[..initial.len()]);

    let mut buf = [0u8; 32];
    let n = reopen_and_read(name.as_str(), &mut buf);
    assert!(n >= initial.len() as isize);
    print_ascii("file bytes after first-write materialize", &buf[..initial.len()]);
    assert_eq!(&buf[..initial.len()], initial, "first write materialization should not modify file");
    assert_eq!(munmap(addr as usize, PAGE_SIZE), 0);
    assert_eq!(unlink(name.as_str()), 0);
}

/// 测试匿名页在 `fork` 后的写时复制。
fn case_fork_cow_anon() {
    println!("[suite] case_fork_cow_anon");
    let addr = mmap_full(
        0,
        PAGE_SIZE,
        MMapProt::PROT_READ | MMapProt::PROT_WRITE,
        MMapFlags::MAP_PRIVATE | MMapFlags::MAP_ANONYMOUS,
        0,
        0,
    );
    assert!(addr > 0, "anonymous mmap failed: {}", addr);
    let page = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, PAGE_SIZE) };
    page[0] = 0x11;
    page[1] = 0x22;
    println!("    parent anon bytes before fork: {:02x} {:02x}", page[0], page[1]);

    let pid = fork();
    assert!(pid >= 0, "fork failed: {}", pid);
    if pid == 0 {
        page[0] = 0x33;
        page[1] = 0x44;
        println!("    child anon bytes after write: {:02x} {:02x}", page[0], page[1]);
        assert_eq!(munmap(addr as usize, PAGE_SIZE), 0);
        exit(0);
    }

    let mut exit_code = -1;
    let waited = waitpid(pid as usize, &mut exit_code);
    assert_eq!(waited, pid, "waitpid failed: waited={}, pid={}", waited, pid);
    println!("    parent anon bytes after child exit: {:02x} {:02x}", page[0], page[1]);
    assert_eq!(page[0], 0x11, "parent page[0] should remain unchanged after child write");
    assert_eq!(page[1], 0x22, "parent page[1] should remain unchanged after child write");
    assert_eq!(munmap(addr as usize, PAGE_SIZE), 0);
}

/// 测试 `MAP_PRIVATE` 映射在 `fork` 后的隔离语义。
fn case_fork_mmap_private() {
    println!("[suite] case_fork_mmap_private");
    let name = unique_name("suite_fpri");
    let fd = open(name.as_str(), OpenFlags::CREATE | OpenFlags::RDWR);
    assert!(fd >= 0, "open failed: {}", fd);
    let fd = fd as usize;
    let initial = b"abcd_private_fork";
    print_ascii("initial file bytes", initial);
    assert_eq!(write(fd, initial), initial.len() as isize);

    let addr = mmap_full(
        0,
        PAGE_SIZE,
        MMapProt::PROT_READ | MMapProt::PROT_WRITE,
        MMapFlags::MAP_PRIVATE,
        fd,
        0,
    );
    assert!(addr > 0, "mmap private failed: {}", addr);
    assert_eq!(close(fd), 0);

    let page = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, PAGE_SIZE) };
    print_ascii("parent mapped bytes before fork", &page[..initial.len()]);

    let pid = fork();
    assert!(pid >= 0, "fork failed: {}", pid);
    if pid == 0 {
        page[0] = b'Z';
        page[1] = b'Y';
        print_ascii("child mapped bytes after write", &page[..initial.len()]);
        assert_eq!(munmap(addr as usize, PAGE_SIZE), 0);
        exit(0);
    }

    let mut exit_code = -1;
    let waited = waitpid(pid as usize, &mut exit_code);
    assert_eq!(waited, pid, "waitpid failed: waited={}, pid={}", waited, pid);
    print_ascii("parent mapped bytes after child exit", &page[..initial.len()]);
    assert_eq!(
        &page[..initial.len()],
        initial,
        "parent private mapping should remain unchanged after child write"
    );

    let mut buf = [0u8; 32];
    let n = reopen_and_read(name.as_str(), &mut buf);
    assert!(n >= initial.len() as isize, "file read too short: {}", n);
    print_ascii("file bytes after child private write", &buf[..initial.len()]);
    assert_eq!(&buf[..initial.len()], initial, "child private write should not modify file");
    assert_eq!(munmap(addr as usize, PAGE_SIZE), 0);
    assert_eq!(unlink(name.as_str()), 0);
}

/// 测试 `MAP_SHARED` 映射在 `fork` 后的共享语义。
fn case_fork_mmap_shared() {
    println!("[suite] case_fork_mmap_shared");
    let name = unique_name("suite_fshr");
    let fd = open(name.as_str(), OpenFlags::CREATE | OpenFlags::RDWR);
    assert!(fd >= 0, "open failed: {}", fd);
    let fd = fd as usize;
    let initial = b"abcd_shared_fork";
    print_ascii("initial file bytes", initial);
    assert_eq!(write(fd, initial), initial.len() as isize);

    let addr = mmap_full(
        0,
        PAGE_SIZE,
        MMapProt::PROT_READ | MMapProt::PROT_WRITE,
        MMapFlags::MAP_SHARED,
        fd,
        0,
    );
    assert!(addr > 0, "mmap shared failed: {}", addr);
    assert_eq!(close(fd), 0);

    let page = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, PAGE_SIZE) };
    page[0] = b'P';
    print_ascii("parent mapped bytes before fork", &page[..initial.len()]);

    let pid = fork();
    assert!(pid >= 0, "fork failed: {}", pid);
    if pid == 0 {
        print_ascii("child observed bytes before write", &page[..initial.len()]);
        assert_eq!(page[0], b'P', "child should observe parent shared write");
        page[1] = b'C';
        print_ascii("child mapped bytes after write", &page[..initial.len()]);
        assert_eq!(munmap(addr as usize, PAGE_SIZE), 0);
        exit(0);
    }

    let mut exit_code = -1;
    let waited = waitpid(pid as usize, &mut exit_code);
    assert_eq!(waited, pid, "waitpid failed: waited={}, pid={}", waited, pid);
    print_ascii("parent mapped bytes after child exit", &page[..initial.len()]);
    assert_eq!(page[1], b'C', "parent should observe child shared write");

    let mut buf = [0u8; 32];
    let n = reopen_and_read(name.as_str(), &mut buf);
    assert!(n >= initial.len() as isize, "file read too short: {}", n);
    print_ascii("file bytes after shared writes", &buf[..initial.len()]);
    assert_eq!(&buf[..2], b"PC", "shared writes should be visible through file read");
    assert_eq!(munmap(addr as usize, PAGE_SIZE), 0);
    assert_eq!(unlink(name.as_str()), 0);
}

/// 测试尾页 EOF 之后的字节会被补零。
fn case_mmap_tail_zero() {
    println!("[suite] case_mmap_tail_zero");
    let name = unique_name("suite_tail");
    let fd = open(name.as_str(), OpenFlags::CREATE | OpenFlags::RDWR);
    assert!(fd >= 0, "open failed: {}", fd);
    let fd = fd as usize;

    let mut data = [0u8; PAGE_SIZE + 5];
    for (idx, byte) in data.iter_mut().enumerate() {
        *byte = b'a' + (idx % 26) as u8;
    }
    print_ascii("tail file prefix", &data[..32]);
    assert_eq!(write(fd, &data), data.len() as isize);

    let addr = mmap_full(
        0,
        PAGE_SIZE * 2,
        MMapProt::PROT_READ,
        MMapFlags::MAP_PRIVATE,
        fd,
        0,
    );
    assert!(addr > 0, "mmap private failed: {}", addr);
    assert_eq!(close(fd), 0);

    let page = unsafe { core::slice::from_raw_parts(addr as *const u8, PAGE_SIZE * 2) };
    print_ascii("mapped prefix", &page[..32]);
    assert_eq!(&page[..data.len()], &data, "mapped bytes before EOF mismatch");
    for (idx, &byte) in page.iter().enumerate().skip(data.len()) {
        assert_eq!(byte, 0, "bytes after EOF within tail page should be zero at {}", idx);
    }
    println!("    zero-filled bytes checked: [{}..{})", data.len(), PAGE_SIZE * 2);

    assert_eq!(munmap(addr as usize, PAGE_SIZE * 2), 0);
    assert_eq!(unlink(name.as_str()), 0);
}

/// 测试整页落在 EOF 之后时会触发 `SIGBUS`。
fn case_mmap_sigbus_eof() {
    println!("[suite] case_mmap_sigbus_eof");
    let name = unique_name("suite_bus");
    let fd = open(name.as_str(), OpenFlags::CREATE | OpenFlags::RDWR);
    assert!(fd >= 0, "open failed: {}", fd);
    let fd = fd as usize;
    let initial = b"sigbus_tail";
    print_ascii("initial file bytes", initial);
    assert_eq!(write(fd, initial), initial.len() as isize);

    let addr = mmap_full(
        0,
        PAGE_SIZE * 2,
        MMapProt::PROT_READ,
        MMapFlags::MAP_PRIVATE,
        fd,
        0,
    );
    assert!(addr > 0, "mmap private failed: {}", addr);
    assert_eq!(close(fd), 0);

    let pid = fork();
    assert!(pid >= 0, "fork failed: {}", pid);
    if pid == 0 {
        println!("    child will touch second page at {:#x}", addr as usize + PAGE_SIZE);
        // 用 volatile 读强制触发第二页访存，避免被编译器优化掉。
        let second_page_ptr = unsafe { (addr as *const u8).add(PAGE_SIZE) };
        let _ = unsafe { core::ptr::read_volatile(second_page_ptr) };
        // TODO：若未来 SIGBUS 处理被改成用户态可恢复，这里需要改成显式断言不可达。
        panic!("second page beyond EOF should raise SIGBUS");
    }

    let mut exit_code = -1;
    let waited = waitpid(pid as usize, &mut exit_code);
    assert_eq!(waited, pid, "waitpid failed: waited={}, pid={}", waited, pid);
    println!("    child waitpid status: {}", exit_code);
    assert_eq!(exit_code, SIGBUS, "whole page beyond EOF should terminate child with SIGBUS");
    assert_eq!(munmap(addr as usize, PAGE_SIZE * 2), 0);
    assert_eq!(unlink(name.as_str()), 0);
}

#[no_mangle]
pub fn main(argc: usize, argv: &[&str]) -> i32 {
    let max_pages = parse_arg(argv, 1, 512);
    let rounds = parse_arg(argv, 2, 3);

    println!(
        "[mmap_test] begin argc={} max_pages={} rounds={}",
        argc, max_pages, rounds
    );
    case_mmap_shared_basic();
    case_mmap_private_basic();
    case_mmap_private_first_write();
    case_fork_cow_anon();
    case_fork_mmap_private();
    case_fork_mmap_shared();
    case_mmap_tail_zero();
    case_mmap_sigbus_eof();
    run_mmap_perf_suite(max_pages, rounds);
    println!("[mmap_test] pass");
    0
}
