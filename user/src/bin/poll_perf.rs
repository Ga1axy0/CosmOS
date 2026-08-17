#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use user_lib::{
    clock_gettime_ns, close, pipe, read, sys_ppoll_time32, thread_create, write, yield_,
    OldTimespec32, PollFd, CLOCK_MONOTONIC,
};

const MAX_FDS: usize = 128;
const MAX_WORKERS: usize = 64;
const DEFAULT_FDS: usize = 16;
const DEFAULT_ROUNDS: usize = 500;
const DEFAULT_TIMEOUT_NS: i32 = 1_000_000;
const POLLIN: i16 = 0x001;
const START_TIMEOUT_NS: u64 = 10_000_000_000;

static WAKE_FDS: [AtomicIsize; MAX_FDS] = [const { AtomicIsize::new(-1) }; MAX_FDS];
static WAKE_FD_COUNT: AtomicUsize = AtomicUsize::new(0);
static WAKE_ROUNDS: AtomicUsize = AtomicUsize::new(0);
static WAKE_START: AtomicUsize = AtomicUsize::new(0);
static WAKE_CONSUMED: AtomicUsize = AtomicUsize::new(0);
static WAKE_ERROR: AtomicIsize = AtomicIsize::new(0);

static FANOUT_READ_FDS: [AtomicIsize; MAX_WORKERS] = [const { AtomicIsize::new(-1) }; MAX_WORKERS];
static FANOUT_WRITE_FDS: [AtomicIsize; MAX_WORKERS] = [const { AtomicIsize::new(-1) }; MAX_WORKERS];
static FANOUT_ROUNDS: AtomicUsize = AtomicUsize::new(0);
static FANOUT_READY: [AtomicUsize; MAX_WORKERS] = [const { AtomicUsize::new(0) }; MAX_WORKERS];
static FANOUT_DONE: [AtomicUsize; MAX_WORKERS] = [const { AtomicUsize::new(0) }; MAX_WORKERS];
static FANOUT_GO: AtomicUsize = AtomicUsize::new(0);
static FANOUT_ERROR: AtomicIsize = AtomicIsize::new(0);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Scan,
    Timeout,
    Wake,
    Fanout,
    Parallel,
}

fn parse_usize(value: &str, default: usize) -> usize {
    let mut parsed = 0usize;
    if value.is_empty() {
        return default;
    }
    for byte in value.bytes() {
        if !byte.is_ascii_digit() {
            return default;
        }
        parsed = parsed
            .saturating_mul(10)
            .saturating_add((byte - b'0') as usize);
    }
    parsed
}

fn parse_mode(value: &str) -> Option<Mode> {
    match value {
        "scan" => Some(Mode::Scan),
        "timeout" => Some(Mode::Timeout),
        "wake" => Some(Mode::Wake),
        "fanout" => Some(Mode::Fanout),
        "parallel" => Some(Mode::Parallel),
        _ => None,
    }
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Scan => "scan",
        Mode::Timeout => "timeout",
        Mode::Wake => "wake",
        Mode::Fanout => "fanout",
        Mode::Parallel => "parallel",
    }
}

fn monotonic_ns() -> u64 {
    let value = clock_gettime_ns(CLOCK_MONOTONIC);
    if value < 0 {
        0
    } else {
        value as u64
    }
}

fn setup_pipes(count: usize, reads: &mut [i32; MAX_FDS], writes: &mut [i32; MAX_FDS]) -> bool {
    for index in 0..count {
        let mut pair = [-1i32; 2];
        if pipe(&mut pair) < 0 {
            close_fds(reads, writes, index);
            return false;
        }
        reads[index] = pair[0];
        writes[index] = pair[1];
    }
    true
}

fn close_fds(reads: &[i32; MAX_FDS], writes: &[i32; MAX_FDS], count: usize) {
    for index in 0..count {
        if reads[index] >= 0 {
            let _ = close(reads[index] as usize);
        }
        if writes[index] >= 0 {
            let _ = close(writes[index] as usize);
        }
    }
}

fn wait_until<F>(deadline: u64, mut condition: F) -> bool
where
    F: FnMut() -> bool,
{
    while !condition() {
        if monotonic_ns() >= deadline {
            return false;
        }
        let _ = yield_();
    }
    true
}

fn print_result(
    mode: Mode,
    fds: usize,
    workers: usize,
    rounds: usize,
    elapsed_ns: u64,
    checksum: usize,
) {
    let per_op_ns = elapsed_ns / rounds.max(1) as u64;
    println!(
        "POLL_PERF_RESULT mode={} fds={} workers={} rounds={} elapsed_ns={} per_op_ns={} checksum={} status=PASS",
        mode_name(mode), fds, workers, rounds, elapsed_ns, per_op_ns, checksum
    );
}

fn run_scan_or_timeout(mode: Mode, fds: usize, rounds: usize) -> i32 {
    let mut reads = [-1i32; MAX_FDS];
    let mut writes = [-1i32; MAX_FDS];
    if !setup_pipes(fds, &mut reads, &mut writes) {
        println!("POLL_PERF_FAIL mode={} reason=pipe", mode_name(mode));
        return 1;
    }

    let timeout = match mode {
        Mode::Scan => OldTimespec32 {
            tv_sec: 0,
            tv_nsec: 0,
        },
        Mode::Timeout => OldTimespec32 {
            tv_sec: 0,
            tv_nsec: DEFAULT_TIMEOUT_NS,
        },
        _ => unreachable!(),
    };
    let mut pollfds = [PollFd {
        fd: -1,
        events: POLLIN,
        revents: 0,
    }; MAX_FDS];
    for index in 0..fds {
        pollfds[index].fd = reads[index];
    }

    let start = monotonic_ns();
    let mut checksum = 0usize;
    for round in 0..rounds {
        for pfd in &mut pollfds[..fds] {
            pfd.revents = 0;
        }
        let ret = sys_ppoll_time32(&mut pollfds[..fds], Some(&timeout));
        let expected = 0;
        if ret != expected {
            println!(
                "POLL_PERF_FAIL mode={} round={} poll_ret={} expected={} revents={:#x}",
                mode_name(mode),
                round,
                ret,
                expected,
                pollfds[0].revents
            );
            close_fds(&reads, &writes, fds);
            return 2;
        }
        checksum = checksum.wrapping_add(pollfds[round % fds].revents as usize);
    }
    let elapsed_ns = monotonic_ns().saturating_sub(start);
    close_fds(&reads, &writes, fds);
    print_result(mode, fds, 0, rounds, elapsed_ns, checksum);
    0
}

extern "C" fn wake_writer(_arg: usize) -> isize {
    let count = WAKE_FD_COUNT.load(Ordering::Acquire);
    let rounds = WAKE_ROUNDS.load(Ordering::Acquire);
    for round in 1..=rounds {
        while WAKE_START.load(Ordering::Acquire) < round {
            let _ = yield_();
        }
        let index = (round - 1) % count;
        let fd = WAKE_FDS[index].load(Ordering::Acquire);
        let payload = [round as u8];
        if fd < 0 || write(fd as usize, &payload) != 1 {
            WAKE_ERROR.store(-1, Ordering::Release);
            return 1;
        }
        while WAKE_CONSUMED.load(Ordering::Acquire) < round {
            if WAKE_ERROR.load(Ordering::Acquire) != 0 {
                return 1;
            }
            let _ = yield_();
        }
    }
    0
}

fn run_wake(fds: usize, rounds: usize) -> i32 {
    let mut reads = [-1i32; MAX_FDS];
    let mut writes = [-1i32; MAX_FDS];
    if !setup_pipes(fds, &mut reads, &mut writes) {
        println!("POLL_PERF_FAIL mode=wake reason=pipe");
        return 1;
    }
    for index in 0..fds {
        WAKE_FDS[index].store(writes[index] as isize, Ordering::Release);
    }
    WAKE_FD_COUNT.store(fds, Ordering::Release);
    WAKE_ROUNDS.store(rounds, Ordering::Release);
    WAKE_START.store(0, Ordering::Release);
    WAKE_CONSUMED.store(0, Ordering::Release);
    WAKE_ERROR.store(0, Ordering::Release);

    if thread_create(wake_writer as usize, 0) <= 0 {
        println!("POLL_PERF_FAIL mode=wake reason=thread_create");
        close_fds(&reads, &writes, fds);
        return 2;
    }

    let mut pollfds = [PollFd {
        fd: -1,
        events: POLLIN,
        revents: 0,
    }; MAX_FDS];
    for index in 0..fds {
        pollfds[index].fd = reads[index];
    }
    let timeout = OldTimespec32 {
        tv_sec: 1,
        tv_nsec: 0,
    };
    let start = monotonic_ns();
    let mut checksum = 0usize;
    for round in 1..=rounds {
        WAKE_START.store(round, Ordering::Release);
        for pfd in &mut pollfds[..fds] {
            pfd.revents = 0;
        }
        let ret = sys_ppoll_time32(&mut pollfds[..fds], Some(&timeout));
        let index = (round - 1) % fds;
        if ret != 1 || (pollfds[index].revents & POLLIN) == 0 {
            println!(
                "POLL_PERF_FAIL mode=wake round={} poll_ret={} target={} revents={:#x} writer_error={}",
                round,
                ret,
                index,
                pollfds[index].revents,
                WAKE_ERROR.load(Ordering::Acquire)
            );
            WAKE_ERROR.store(-2, Ordering::Release);
            close_fds(&reads, &writes, fds);
            return 3;
        }
        let mut payload = [0u8; 1];
        if read(reads[index] as usize, &mut payload) != 1 {
            println!("POLL_PERF_FAIL mode=wake round={} reason=read", round);
            WAKE_ERROR.store(-3, Ordering::Release);
            close_fds(&reads, &writes, fds);
            return 4;
        }
        checksum = checksum.wrapping_add(payload[0] as usize);
        WAKE_CONSUMED.store(round, Ordering::Release);
    }
    let elapsed_ns = monotonic_ns().saturating_sub(start);
    close_fds(&reads, &writes, fds);
    print_result(Mode::Wake, fds, 1, rounds, elapsed_ns, checksum);
    0
}

extern "C" fn fanout_worker(index: usize) -> isize {
    FANOUT_READY[index].store(1, Ordering::Release);
    while FANOUT_GO.load(Ordering::Acquire) == 0 {
        let _ = yield_();
    }

    let rounds = FANOUT_ROUNDS.load(Ordering::Acquire);
    let fd = FANOUT_READ_FDS[index].load(Ordering::Acquire);
    let timeout = OldTimespec32 {
        tv_sec: 5,
        tv_nsec: 0,
    };
    let mut pollfd = [PollFd {
        fd: fd as i32,
        events: POLLIN,
        revents: 0,
    }];
    let mut payload = [0u8; 1];
    for round in 1..=rounds {
        pollfd[0].revents = 0;
        let ret = sys_ppoll_time32(&mut pollfd, Some(&timeout));
        if ret != 1 || (pollfd[0].revents & POLLIN) == 0 || read(fd as usize, &mut payload) != 1 {
            FANOUT_ERROR.store(-1, Ordering::Release);
            return 1;
        }
        FANOUT_DONE[index].store(round, Ordering::Release);
    }
    0
}

fn run_fanout(workers: usize, rounds: usize) -> i32 {
    let mut reads = [-1i32; MAX_FDS];
    let mut writes = [-1i32; MAX_FDS];
    if !setup_pipes(workers, &mut reads, &mut writes) {
        println!("POLL_PERF_FAIL mode=fanout reason=pipe");
        return 1;
    }
    FANOUT_ROUNDS.store(rounds, Ordering::Release);
    FANOUT_GO.store(0, Ordering::Release);
    FANOUT_ERROR.store(0, Ordering::Release);
    for index in 0..workers {
        FANOUT_READY[index].store(0, Ordering::Release);
        FANOUT_DONE[index].store(0, Ordering::Release);
        FANOUT_READ_FDS[index].store(reads[index] as isize, Ordering::Release);
        FANOUT_WRITE_FDS[index].store(writes[index] as isize, Ordering::Release);
        if thread_create(fanout_worker as usize, index) <= 0 {
            println!(
                "POLL_PERF_FAIL mode=fanout reason=thread_create index={}",
                index
            );
            close_fds(&reads, &writes, workers);
            return 2;
        }
    }

    let ready_deadline = monotonic_ns().saturating_add(START_TIMEOUT_NS);
    if !wait_until(ready_deadline, || {
        FANOUT_READY[..workers]
            .iter()
            .all(|ready| ready.load(Ordering::Acquire) != 0)
    }) {
        println!("POLL_PERF_FAIL mode=fanout reason=worker_startup");
        FANOUT_ERROR.store(-2, Ordering::Release);
        close_fds(&reads, &writes, workers);
        return 3;
    }

    FANOUT_GO.store(1, Ordering::Release);
    // Give every worker a scheduling opportunity to pass the startup barrier
    // and enter its first ppoll before the producer publishes the first
    // batch.  The kernel still has a post-registration readiness recheck, but
    // this keeps the benchmark focused on steady-state fanout rather than
    // process-start scheduling noise.
    for _ in 0..workers.saturating_mul(4).max(8) {
        let _ = yield_();
    }
    let start = monotonic_ns();
    let mut checksum = 0usize;
    for round in 1..=rounds {
        let payload = [round as u8];
        for index in 0..workers {
            let fd = FANOUT_WRITE_FDS[index].load(Ordering::Acquire);
            if fd < 0 || write(fd as usize, &payload) != 1 {
                FANOUT_ERROR.store(-3, Ordering::Release);
                println!("POLL_PERF_FAIL mode=fanout round={} reason=write", round);
                close_fds(&reads, &writes, workers);
                return 4;
            }
        }
        let deadline = monotonic_ns().saturating_add(START_TIMEOUT_NS);
        if !wait_until(deadline, || {
            FANOUT_ERROR.load(Ordering::Acquire) == 0
                && FANOUT_DONE[..workers]
                    .iter()
                    .all(|done| done.load(Ordering::Acquire) >= round)
        }) {
            println!(
                "POLL_PERF_FAIL mode=fanout round={} reason=worker_timeout error={}",
                round,
                FANOUT_ERROR.load(Ordering::Acquire)
            );
            FANOUT_ERROR.store(-4, Ordering::Release);
            close_fds(&reads, &writes, workers);
            return 5;
        }
        checksum = checksum.wrapping_add(round.saturating_mul(workers));
    }
    let elapsed_ns = monotonic_ns().saturating_sub(start);
    close_fds(&reads, &writes, workers);
    print_result(Mode::Fanout, workers, workers, rounds, elapsed_ns, checksum);
    0
}

/// Run independent timed polls concurrently.  Unlike `fanout`, this mode
/// does not depend on a burst of cross-thread pipe notifications; it isolates
/// registration, timer, cleanup, and the shared poll locks under SMP load.
extern "C" fn parallel_worker(index: usize) -> isize {
    FANOUT_READY[index].store(1, Ordering::Release);
    while FANOUT_GO.load(Ordering::Acquire) == 0 {
        let _ = yield_();
    }

    let rounds = FANOUT_ROUNDS.load(Ordering::Acquire);
    let fd = FANOUT_READ_FDS[index].load(Ordering::Acquire);
    let timeout = OldTimespec32 {
        tv_sec: 0,
        tv_nsec: DEFAULT_TIMEOUT_NS,
    };
    let mut pollfd = [PollFd {
        fd: fd as i32,
        events: POLLIN,
        revents: 0,
    }];
    for round in 1..=rounds {
        pollfd[0].revents = 0;
        let ret = sys_ppoll_time32(&mut pollfd, Some(&timeout));
        if ret != 0 {
            FANOUT_ERROR.store(-5, Ordering::Release);
            return 1;
        }
        FANOUT_DONE[index].store(round, Ordering::Release);
    }
    0
}

fn run_parallel(workers: usize, rounds: usize) -> i32 {
    let mut reads = [-1i32; MAX_FDS];
    let mut writes = [-1i32; MAX_FDS];
    if !setup_pipes(workers, &mut reads, &mut writes) {
        println!("POLL_PERF_FAIL mode=parallel reason=pipe");
        return 1;
    }
    FANOUT_ROUNDS.store(rounds, Ordering::Release);
    FANOUT_GO.store(0, Ordering::Release);
    FANOUT_ERROR.store(0, Ordering::Release);
    for index in 0..workers {
        FANOUT_READY[index].store(0, Ordering::Release);
        FANOUT_DONE[index].store(0, Ordering::Release);
        FANOUT_READ_FDS[index].store(reads[index] as isize, Ordering::Release);
        FANOUT_WRITE_FDS[index].store(writes[index] as isize, Ordering::Release);
        if thread_create(parallel_worker as usize, index) <= 0 {
            println!(
                "POLL_PERF_FAIL mode=parallel reason=thread_create index={}",
                index
            );
            close_fds(&reads, &writes, workers);
            return 2;
        }
    }

    let ready_deadline = monotonic_ns().saturating_add(START_TIMEOUT_NS);
    if !wait_until(ready_deadline, || {
        FANOUT_READY[..workers]
            .iter()
            .all(|ready| ready.load(Ordering::Acquire) != 0)
    }) {
        println!("POLL_PERF_FAIL mode=parallel reason=worker_startup");
        FANOUT_ERROR.store(-6, Ordering::Release);
        close_fds(&reads, &writes, workers);
        return 3;
    }

    FANOUT_GO.store(1, Ordering::Release);
    let start = monotonic_ns();
    let mut checksum = 0usize;
    for round in 1..=rounds {
        let deadline = monotonic_ns().saturating_add(START_TIMEOUT_NS);
        if !wait_until(deadline, || {
            FANOUT_ERROR.load(Ordering::Acquire) == 0
                && FANOUT_DONE[..workers]
                    .iter()
                    .all(|done| done.load(Ordering::Acquire) >= round)
        }) {
            println!(
                "POLL_PERF_FAIL mode=parallel round={} reason=worker_timeout error={}",
                round,
                FANOUT_ERROR.load(Ordering::Acquire)
            );
            FANOUT_ERROR.store(-7, Ordering::Release);
            close_fds(&reads, &writes, workers);
            return 4;
        }
        checksum = checksum.wrapping_add(round.saturating_mul(workers));
    }
    let elapsed_ns = monotonic_ns().saturating_sub(start);
    close_fds(&reads, &writes, workers);
    print_result(
        Mode::Parallel,
        workers,
        workers,
        rounds,
        elapsed_ns,
        checksum,
    );
    0
}

fn usage() {
    println!("usage: poll_perf <scan|timeout|wake|fanout|parallel> [fds_or_workers] [rounds]");
    println!(
        "  scan: zero-timeout scans; timeout: {}ms idle waits; wake: one producer over N fds; fanout: N concurrent pipe pollers; parallel: N concurrent timed pollers",
        DEFAULT_TIMEOUT_NS / 1_000_000
    );
}

#[no_mangle]
fn main(argc: usize, argv: &[&str]) -> i32 {
    if argc < 2 {
        usage();
        return 1;
    }
    let Some(mode) = parse_mode(argv[1]) else {
        usage();
        return 1;
    };

    let requested = if argc > 2 {
        parse_usize(argv[2], DEFAULT_FDS)
    } else {
        DEFAULT_FDS
    };
    let rounds = if argc > 3 {
        parse_usize(argv[3], DEFAULT_ROUNDS).max(1)
    } else {
        DEFAULT_ROUNDS
    };
    let limit = if matches!(mode, Mode::Fanout | Mode::Parallel) {
        MAX_WORKERS
    } else {
        MAX_FDS
    };
    let count = requested.clamp(1, limit);

    println!(
        "POLL_PERF_START mode={} fds={} workers={} rounds={}",
        mode_name(mode),
        count,
        if matches!(mode, Mode::Fanout | Mode::Parallel) {
            count
        } else {
            0
        },
        rounds
    );
    match mode {
        Mode::Scan | Mode::Timeout => run_scan_or_timeout(mode, count, rounds),
        Mode::Wake => run_wake(count, rounds),
        Mode::Fanout => run_fanout(count, rounds),
        Mode::Parallel => run_parallel(count, rounds),
    }
}
