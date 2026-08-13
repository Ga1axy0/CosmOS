#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::hint::spin_loop;
use core::sync::atomic::{AtomicUsize, Ordering};
use user_lib::{
    clock_gettime_ns, exit, getcpu, munmap, sched_getaffinity, sched_setaffinity, sys_mmap_full,
    thread_create, yield_, CLOCK_MONOTONIC,
};

const PAGE_SIZE: usize = 4096;
const DEFAULT_ROUNDS: usize = 1_000;
const MAX_ROUNDS: usize = 100_000;
const QUARANTINE_PAGES: usize = 8;
const WAIT_TIMEOUT_NS: u64 = 5_000_000_000;

const PROT_READ: usize = 0x1;
const PROT_WRITE: usize = 0x2;
const MAP_PRIVATE: usize = 0x2;
const MAP_FIXED: usize = 0x10;
const MAP_ANONYMOUS: usize = 0x20;

const START_RUN: usize = 1;
const START_ABORT: usize = 2;

static TARGET_ADDR: AtomicUsize = AtomicUsize::new(0);
static WORKER_CPU: AtomicUsize = AtomicUsize::new(0);
static ROUNDS: AtomicUsize = AtomicUsize::new(0);
static START: AtomicUsize = AtomicUsize::new(0);
static WORKER_READY: AtomicUsize = AtomicUsize::new(0);
static WORKER_ERROR: AtomicUsize = AtomicUsize::new(0);
static WARMED: AtomicUsize = AtomicUsize::new(0);
static PUBLISHED_ROUND: AtomicUsize = AtomicUsize::new(0);
static ACKED_ROUND: AtomicUsize = AtomicUsize::new(0);
static EXPECTED: AtomicUsize = AtomicUsize::new(0);
static FAILURES: AtomicUsize = AtomicUsize::new(0);
static FIRST_FAILURE_ROUND: AtomicUsize = AtomicUsize::new(0);
static FIRST_EXPECTED: AtomicUsize = AtomicUsize::new(0);
static FIRST_OBSERVED: AtomicUsize = AtomicUsize::new(0);
static FINISHED: AtomicUsize = AtomicUsize::new(0);

fn parse_rounds(value: Option<&&str>) -> usize {
    let Some(value) = value else {
        return DEFAULT_ROUNDS;
    };
    let mut parsed = 0usize;
    for byte in value.bytes() {
        if !byte.is_ascii_digit() {
            return DEFAULT_ROUNDS;
        }
        parsed = parsed
            .saturating_mul(10)
            .saturating_add((byte - b'0') as usize);
    }
    parsed.clamp(1, MAX_ROUNDS)
}

fn cpu_bit(cpu: usize) -> usize {
    if cpu < usize::BITS as usize {
        1usize << cpu
    } else {
        0
    }
}

fn first_two_cpus(mask: usize) -> Option<(usize, usize)> {
    if mask.count_ones() < 2 {
        return None;
    }
    let first = mask.trailing_zeros() as usize;
    let remaining = mask & !cpu_bit(first);
    let second = remaining.trailing_zeros() as usize;
    Some((first, second))
}

fn now_ns() -> u64 {
    let value = clock_gettime_ns(CLOCK_MONOTONIC);
    if value < 0 {
        0
    } else {
        value as u64
    }
}

fn wait_at_least(value: &AtomicUsize, target: usize) -> bool {
    let start = now_ns();
    let mut polls = 0usize;
    while value.load(Ordering::Acquire) < target {
        spin_loop();
        polls = polls.wrapping_add(1);
        if polls & 0x3fff == 0 {
            yield_();
            let now = now_ns();
            if start != 0 && now != 0 && now.saturating_sub(start) >= WAIT_TIMEOUT_NS {
                return false;
            }
        }
    }
    true
}

fn map_anonymous(addr: usize, len: usize, fixed: bool) -> isize {
    let mut flags = MAP_PRIVATE | MAP_ANONYMOUS;
    if fixed {
        flags |= MAP_FIXED;
    }
    sys_mmap_full(addr, len, PROT_READ | PROT_WRITE, flags, 0, 0)
}

fn target_pattern(round: usize) -> usize {
    0xc05c_4f53_0000_0000usize
        ^ round.wrapping_mul(0x9e37_79b9_7f4a_7c15usize)
        ^ round.rotate_left(19)
}

fn poison_pattern(round: usize, page: usize) -> usize {
    0xdead_7ab1_0000_0000usize
        ^ round.wrapping_mul(0xd1b5_4a32_d192_ed03usize)
        ^ page.wrapping_mul(0x100_0000_01b3usize)
}

fn pin_current(cpu: usize) -> bool {
    if sched_setaffinity(0, cpu_bit(cpu)) < 0 {
        return false;
    }
    // Setting affinity requests rescheduling; yield so the following getcpu
    // observes the destination hart before the hot, syscall-free phase.
    yield_();
    getcpu() == cpu as isize
}

fn record_failure(round: usize, expected: usize, observed: usize) {
    FAILURES.fetch_add(1, Ordering::Relaxed);
    if FIRST_FAILURE_ROUND
        .compare_exchange(0, round, Ordering::AcqRel, Ordering::Relaxed)
        .is_ok()
    {
        FIRST_EXPECTED.store(expected, Ordering::Relaxed);
        FIRST_OBSERVED.store(observed, Ordering::Relaxed);
    }
}

extern "C" fn reader_entry(_: usize) -> isize {
    let cpu = WORKER_CPU.load(Ordering::Acquire);
    if !pin_current(cpu) {
        WORKER_ERROR.store(1, Ordering::Release);
    }
    WORKER_READY.store(1, Ordering::Release);

    while START.load(Ordering::Acquire) == 0 {
        spin_loop();
    }
    if START.load(Ordering::Acquire) == START_ABORT {
        FINISHED.store(1, Ordering::Release);
        exit(1);
    }

    let target = TARGET_ADDR.load(Ordering::Acquire) as *const usize;
    let initial_expected = EXPECTED.load(Ordering::Acquire);
    let initial_observed = unsafe { core::ptr::read_volatile(target) };
    if initial_observed != initial_expected {
        record_failure(usize::MAX, initial_expected, initial_observed);
    }
    // The read above installs the target translation on the remote hart.
    // From this point until all checks finish the worker performs no syscalls,
    // so a successful check cannot be explained by a context-switch fence.
    WARMED.store(1, Ordering::Release);

    let rounds = ROUNDS.load(Ordering::Acquire);
    for round in 1..=rounds {
        while PUBLISHED_ROUND.load(Ordering::Acquire) < round {
            spin_loop();
        }
        let expected = EXPECTED.load(Ordering::Relaxed);
        let observed = unsafe { core::ptr::read_volatile(target) };
        if observed != expected {
            record_failure(round, expected, observed);
        }
        // This also confirms that the current target translation is resident
        // before the controller starts the next replacement.
        ACKED_ROUND.store(round, Ordering::Release);
    }

    FINISHED.store(1, Ordering::Release);
    exit(0);
}

fn abort_worker() {
    START.store(START_ABORT, Ordering::Release);
    let _ = wait_at_least(&FINISHED, 1);
}

#[no_mangle]
fn main(_argc: usize, argv: &[&str]) -> i32 {
    let rounds = parse_rounds(argv.get(1));
    let affinity = sched_getaffinity(0);
    if affinity < 0 {
        println!(
            "tlb_shootdown_probe: FAIL sched_getaffinity returned {}",
            affinity
        );
        return 1;
    }
    let affinity = affinity as usize;
    let Some((controller_cpu, reader_cpu)) = first_two_cpus(affinity) else {
        println!(
            "tlb_shootdown_probe: SKIP need at least two harts affinity={:#x}",
            affinity
        );
        return 0;
    };

    let target = map_anonymous(0, PAGE_SIZE, false);
    if target <= 0 {
        println!("tlb_shootdown_probe: FAIL initial mmap returned {}", target);
        return 2;
    }
    let target = target as usize;
    let initial = target_pattern(0);
    unsafe { core::ptr::write_volatile(target as *mut usize, initial) };

    TARGET_ADDR.store(target, Ordering::Release);
    WORKER_CPU.store(reader_cpu, Ordering::Release);
    ROUNDS.store(rounds, Ordering::Release);
    EXPECTED.store(initial, Ordering::Release);

    let tid = thread_create(reader_entry as usize, 0);
    if tid <= 0 {
        let _ = munmap(target, PAGE_SIZE);
        println!("tlb_shootdown_probe: FAIL thread_create returned {}", tid);
        return 3;
    }
    if !wait_at_least(&WORKER_READY, 1) {
        abort_worker();
        let _ = munmap(target, PAGE_SIZE);
        println!("tlb_shootdown_probe: FAIL reader startup timeout");
        return 4;
    }
    if WORKER_ERROR.load(Ordering::Acquire) != 0 {
        abort_worker();
        let _ = munmap(target, PAGE_SIZE);
        println!(
            "tlb_shootdown_probe: FAIL could not pin reader to cpu {}",
            reader_cpu
        );
        return 5;
    }
    if !pin_current(controller_cpu) {
        abort_worker();
        let _ = munmap(target, PAGE_SIZE);
        println!(
            "tlb_shootdown_probe: FAIL could not pin controller to cpu {}",
            controller_cpu
        );
        return 6;
    }

    println!(
        "tlb_shootdown_probe: start rounds={} target={:#x} controller_cpu={} reader_cpu={} affinity={:#x}",
        rounds, target, controller_cpu, reader_cpu, affinity
    );
    START.store(START_RUN, Ordering::Release);
    if !wait_at_least(&WARMED, 1) {
        let _ = munmap(target, PAGE_SIZE);
        println!("tlb_shootdown_probe: FAIL initial remote TLB warmup timeout");
        return 7;
    }

    for round in 1..=rounds {
        // Reserve a distinct VMA before making target a hole.  Once target is
        // unmapped, touching these pages consumes its just-released frame (and
        // several neighbours), making a stale target translation observable
        // rather than accidentally reusing the same physical page.
        let quarantine_len = QUARANTINE_PAGES * PAGE_SIZE;
        let quarantine = map_anonymous(0, quarantine_len, false);
        if quarantine <= 0 {
            println!(
                "tlb_shootdown_probe: FAIL round={} quarantine mmap returned {}",
                round, quarantine
            );
            return 8;
        }
        let quarantine = quarantine as usize;

        if munmap(target, PAGE_SIZE) != 0 {
            let _ = munmap(quarantine, quarantine_len);
            println!("tlb_shootdown_probe: FAIL round={} target munmap", round);
            return 9;
        }
        for page in 0..QUARANTINE_PAGES {
            unsafe {
                core::ptr::write_volatile(
                    (quarantine + page * PAGE_SIZE) as *mut usize,
                    poison_pattern(round, page),
                );
            }
        }

        let remapped = map_anonymous(target, PAGE_SIZE, true);
        if remapped != target as isize {
            let _ = munmap(quarantine, quarantine_len);
            println!(
                "tlb_shootdown_probe: FAIL round={} MAP_FIXED returned {:#x}, expected {:#x}",
                round, remapped, target
            );
            return 10;
        }
        let expected = target_pattern(round);
        unsafe { core::ptr::write_volatile(target as *mut usize, expected) };
        EXPECTED.store(expected, Ordering::Relaxed);
        PUBLISHED_ROUND.store(round, Ordering::Release);

        if !wait_at_least(&ACKED_ROUND, round) {
            let _ = munmap(quarantine, quarantine_len);
            println!(
                "tlb_shootdown_probe: FAIL round={} reader acknowledgement timeout",
                round
            );
            return 11;
        }
        if munmap(quarantine, quarantine_len) != 0 {
            println!(
                "tlb_shootdown_probe: FAIL round={} quarantine munmap",
                round
            );
            return 12;
        }
    }

    if !wait_at_least(&FINISHED, 1) {
        let _ = munmap(target, PAGE_SIZE);
        println!("tlb_shootdown_probe: FAIL reader completion timeout");
        return 13;
    }
    let failures = FAILURES.load(Ordering::Acquire);
    let first_round = FIRST_FAILURE_ROUND.load(Ordering::Acquire);
    let first_expected = FIRST_EXPECTED.load(Ordering::Acquire);
    let first_observed = FIRST_OBSERVED.load(Ordering::Acquire);
    let unmap_ret = munmap(target, PAGE_SIZE);

    if failures != 0 {
        println!(
            "tlb_shootdown_probe: FAIL stale_reads={} first_round={} expected={:#x} observed={:#x}",
            failures, first_round, first_expected, first_observed
        );
        return 14;
    }
    if unmap_ret != 0 {
        println!(
            "tlb_shootdown_probe: FAIL final munmap returned {}",
            unmap_ret
        );
        return 15;
    }

    println!(
        "tlb_shootdown_probe: PASS rounds={} stale_reads=0 controller_cpu={} reader_cpu={}",
        rounds, controller_cpu, reader_cpu
    );
    0
}
