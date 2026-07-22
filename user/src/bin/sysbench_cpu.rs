#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::sync::atomic::{AtomicUsize, Ordering};
use user_lib::{
    clock_gettime_ns, exit, getcpu, sched_getaffinity, thread_create, yield_, CLOCK_MONOTONIC,
};

const MAX_THREADS: usize = 8;
const DEFAULT_SECONDS: usize = 5;
const DEFAULT_MAX_PRIME: usize = 20_000;
const MAX_PRIME: usize = 100_000;
// Only the controller worker reads the clock.  The other workers poll STOP
// occasionally, so the hot path does not perform one clock syscall per worker.
const CONTROL_CHECK_EVERY: usize = 64;
const STARTUP_TIMEOUT_NS: u64 = 5_000_000_000;
const FINISH_TIMEOUT_NS: u64 = 2_000_000_000;

// One event is the same basic unit as sysbench's CPU test: calculate all
// primes up to a configurable limit.  The benchmark is intentionally kept in
// userspace so the result mostly reflects scheduling and CPU execution.
static STARTED: AtomicUsize = AtomicUsize::new(0);
static GO: AtomicUsize = AtomicUsize::new(0);
static STOP: AtomicUsize = AtomicUsize::new(0);
static START_NS: AtomicUsize = AtomicUsize::new(0);
static DURATION_NS: AtomicUsize = AtomicUsize::new(0);
static PRIME_LIMIT: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_PRIME);
static STARTED_FLAGS: [AtomicUsize; MAX_THREADS] = [const { AtomicUsize::new(0) }; MAX_THREADS];
static EVENTS: [AtomicUsize; MAX_THREADS] = [const { AtomicUsize::new(0) }; MAX_THREADS];
static PRIME_SUMS: [AtomicUsize; MAX_THREADS] = [const { AtomicUsize::new(0) }; MAX_THREADS];
static CPU_MASKS: [AtomicUsize; MAX_THREADS] = [const { AtomicUsize::new(0) }; MAX_THREADS];
static FINISHED: [AtomicUsize; MAX_THREADS] = [const { AtomicUsize::new(0) }; MAX_THREADS];

fn parse_usize(text: &str, default: usize) -> usize {
    let mut value = 0usize;
    let mut any = false;
    for &byte in text.as_bytes() {
        if !byte.is_ascii_digit() {
            return default;
        }
        any = true;
        value = value
            .saturating_mul(10)
            .saturating_add((byte - b'0') as usize);
    }
    if any {
        value
    } else {
        default
    }
}

fn mask_bit(cpu: usize) -> usize {
    if cpu < usize::BITS as usize {
        1usize << cpu
    } else {
        0
    }
}

fn count_bits(mask: usize) -> usize {
    mask.count_ones() as usize
}

fn now_ns() -> u64 {
    let value = clock_gettime_ns(CLOCK_MONOTONIC);
    if value < 0 {
        0
    } else {
        value as u64
    }
}

#[inline(never)]
fn is_prime(value: usize) -> bool {
    if value < 2 {
        return false;
    }
    if value == 2 {
        return true;
    }
    if value & 1 == 0 {
        return false;
    }

    let mut divisor = 3usize;
    while divisor <= value / divisor {
        if value % divisor == 0 {
            return false;
        }
        divisor += 2;
    }
    true
}

#[inline(never)]
fn cpu_event(limit: usize) -> usize {
    let mut primes = 0usize;
    for value in 2..=limit {
        if is_prime(value) {
            primes += 1;
        }
    }
    primes
}

fn sample_cpu(cpu_mask: &mut usize) {
    let cpu = getcpu();
    if cpu >= 0 {
        *cpu_mask |= mask_bit(cpu as usize);
    }
}

fn run_worker(index: usize) {
    while GO.load(Ordering::Acquire) == 0 {
        yield_();
    }

    let start = START_NS.load(Ordering::Acquire) as u64;
    let deadline = start.saturating_add(DURATION_NS.load(Ordering::Acquire) as u64);
    let limit = PRIME_LIMIT.load(Ordering::Relaxed);
    let mut events = 0usize;
    let mut prime_sum = 0usize;
    let mut cpu_mask = 0usize;
    sample_cpu(&mut cpu_mask);

    loop {
        // Worker 0 is the only timekeeper.  All other workers use one cheap
        // user-space atomic poll per chunk and therefore do not contend on the
        // process time-namespace lock in clock_gettime().
        if index != 0
            && events & (CONTROL_CHECK_EVERY - 1) == 0
            && STOP.load(Ordering::Relaxed) != 0
        {
            break;
        }
        prime_sum = prime_sum.wrapping_add(cpu_event(limit));
        events = events.wrapping_add(1);

        if events & (CONTROL_CHECK_EVERY - 1) == 0 {
            if index == 0 {
                if now_ns() >= deadline {
                    STOP.store(1, Ordering::Relaxed);
                    break;
                }
            } else if STOP.load(Ordering::Relaxed) != 0 {
                break;
            }
        }
    }

    sample_cpu(&mut cpu_mask);
    EVENTS[index].store(events, Ordering::Release);
    PRIME_SUMS[index].store(prime_sum, Ordering::Release);
    CPU_MASKS[index].store(cpu_mask, Ordering::Release);
    FINISHED[index].store(1, Ordering::Release);
}

extern "C" fn worker_entry(arg: usize) -> isize {
    let index = arg;
    STARTED_FLAGS[index].store(1, Ordering::Release);
    STARTED.fetch_add(1, Ordering::AcqRel);
    run_worker(index);
    exit(0);
}

fn print_result(index: usize, elapsed_ns: u64) -> (usize, usize, usize) {
    let events = EVENTS[index].load(Ordering::Acquire);
    let primes = PRIME_SUMS[index].load(Ordering::Acquire);
    let cpus = CPU_MASKS[index].load(Ordering::Acquire);
    let events_per_sec = (events as u64).saturating_mul(1_000_000_000) / elapsed_ns.max(1);
    println!(
        "sysbench_cpu: worker={} events={} events/s={} prime_sum={} cpu_mask={:#x}",
        index, events, events_per_sec, primes, cpus
    );
    (events, primes, cpus)
}

#[no_mangle]
fn main(argc: usize, argv: &[&str]) -> i32 {
    // threads=0 means all CPUs in the current affinity mask; this makes the
    // default invocation useful for an SMP image while still allowing a 1-vs-N
    // comparison from the shell.
    let affinity = sched_getaffinity(0);
    let affinity_mask = if affinity < 0 {
        1usize
    } else {
        affinity as usize
    };
    let available = count_bits(affinity_mask).max(1);
    let requested_threads = if argc > 1 { parse_usize(argv[1], 0) } else { 0 };
    let threads = if requested_threads == 0 {
        available.min(MAX_THREADS)
    } else {
        requested_threads.clamp(1, MAX_THREADS)
    };
    let seconds = if argc > 2 {
        parse_usize(argv[2], DEFAULT_SECONDS).clamp(1, 3600)
    } else {
        DEFAULT_SECONDS
    };
    let prime_limit = if argc > 3 {
        parse_usize(argv[3], DEFAULT_MAX_PRIME).clamp(100, MAX_PRIME)
    } else {
        DEFAULT_MAX_PRIME
    };

    PRIME_LIMIT.store(prime_limit, Ordering::Release);
    DURATION_NS.store(seconds.saturating_mul(1_000_000_000), Ordering::Release);
    println!(
        "sysbench_cpu: threads={} requested={} seconds={} max_prime={} affinity={:#x} cpus={}",
        threads, requested_threads, seconds, prime_limit, affinity_mask, available
    );
    println!(
        "sysbench_cpu: compare `sysbench_cpu 1` with `sysbench_cpu {}` for SMP scaling",
        threads
    );

    let mut created = 0usize;
    let mut creation_failures = 0usize;
    for index in 1..threads {
        let tid = thread_create(worker_entry as usize, index);
        if tid <= 0 {
            println!(
                "sysbench_cpu: thread_create worker={} failed: {}",
                index, tid
            );
            creation_failures += 1;
            break;
        }
        created += 1;
    }
    let active = created + 1;

    let startup_deadline = now_ns().saturating_add(STARTUP_TIMEOUT_NS);
    while STARTED.load(Ordering::Acquire) < created && now_ns() < startup_deadline {
        yield_();
    }
    let started = STARTED.load(Ordering::Acquire);
    if started < created {
        let missing = created - started;
        println!(
            "sysbench_cpu: startup_timeout started={} created={} missing={}",
            started, created, missing
        );
    }
    let mut startup_failure_mask = 0usize;
    for index in 1..=created {
        if STARTED_FLAGS[index].load(Ordering::Acquire) == 0 {
            startup_failure_mask |= mask_bit(index);
        }
    }

    let start = now_ns();
    START_NS.store(start as usize, Ordering::Release);
    STOP.store(0, Ordering::Relaxed);
    GO.store(1, Ordering::Release);
    run_worker(0);
    let end = now_ns();
    let elapsed_ns = end.saturating_sub(start).max(1);

    // Do not use waittid here: this kernel currently returns a global
    // thread_id from thread_create(), while waittid() indexes the process-local
    // task table, and the user wrapper also retries the wrong EAGAIN value.
    // The completion flags are sufficient for publishing benchmark results and
    // avoid adding join-related kernel locking to the measured interval.
    let finish_deadline = now_ns().saturating_add(FINISH_TIMEOUT_NS);
    loop {
        let mut finished = 0usize;
        for index in 0..active {
            if FINISHED[index].load(Ordering::Acquire) != 0 {
                finished += 1;
            }
        }
        if finished == active || now_ns() >= finish_deadline {
            break;
        }
        yield_();
    }
    let mut finished = 0usize;
    for index in 0..active {
        if FINISHED[index].load(Ordering::Acquire) != 0 {
            finished += 1;
        }
    }
    if finished < active {
        let missing = active - finished;
        println!(
            "sysbench_cpu: finish_timeout finished={} workers={} missing={}",
            finished, active, missing
        );
    }

    let mut finish_failure_mask = 0usize;
    for index in 0..active {
        if FINISHED[index].load(Ordering::Acquire) == 0 {
            finish_failure_mask |= mask_bit(index);
        }
    }
    let worker_failures =
        creation_failures + count_bits(startup_failure_mask | finish_failure_mask);
    let started_final = STARTED.load(Ordering::Acquire);

    let mut total_events = 0usize;
    let mut total_primes = 0usize;
    let mut total_cpu_mask = 0usize;
    for index in 0..active {
        let (events, primes, cpus) = print_result(index, elapsed_ns);
        total_events = total_events.wrapping_add(events);
        total_primes = total_primes.wrapping_add(primes);
        total_cpu_mask |= cpus;
    }
    let total_per_sec = (total_events as u64).saturating_mul(1_000_000_000) / elapsed_ns;
    println!(
        "sysbench_cpu: total_events={} total_events/s={} total_prime_sum={} elapsed_ms={} observed_cpu_mask={:#x} workers={} started={} finished={} failures={}",
        total_events,
        total_per_sec,
        total_primes,
        elapsed_ns / 1_000_000,
        total_cpu_mask,
        active,
        started_final,
        finished,
        worker_failures
    );

    if worker_failures == 0 {
        0
    } else {
        1
    }
}
