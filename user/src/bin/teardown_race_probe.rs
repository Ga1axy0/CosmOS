#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};
use user_lib::{
    clock_gettime_ns, exec, exit, fork, kill, pipe, sched_getaffinity, sched_setaffinity, syscall,
    sys_ppoll_time32, thread_create, waitpid, yield_, PollFd, CLOCK_MONOTONIC,
};

const SYS_EXIT_GROUP: usize = 94;
const SYS_WAIT4: usize = 260;
const WNOHANG: usize = 1;
const SIGKILL: i32 = 9;
const ROUNDS: usize = 160;
const CHILD_TIMEOUT_NS: u64 = 2_000_000_000;
const SELF_PATH: &str = "/.cosmos-old-root/root/guest-payload\0";
const SELF_ARG0: &str = "teardown_race_probe\0";
const TARGET_ARG: &str = "target\0";

static READY: AtomicUsize = AtomicUsize::new(0);
static GO: AtomicUsize = AtomicUsize::new(0);
static WORKER_CPU: AtomicUsize = AtomicUsize::new(0);
static BLOCKER_CPU: AtomicUsize = AtomicUsize::new(0);
static BLOCKER_READ_FD: AtomicUsize = AtomicUsize::new(usize::MAX);
static WORKER_DELAYS: AtomicUsize = AtomicUsize::new(0);

fn now_ns() -> u64 {
    let value = clock_gettime_ns(CLOCK_MONOTONIC);
    if value < 0 {
        0
    } else {
        value as u64
    }
}

fn cpu_bit(cpu: usize) -> usize {
    if cpu < usize::BITS as usize {
        1usize << cpu
    } else {
        0
    }
}

fn first_two_cpus(mask: usize) -> Option<(usize, usize)> {
    let first = mask.trailing_zeros() as usize;
    if first >= usize::BITS as usize {
        return None;
    }
    let rest = mask & !cpu_bit(first);
    let second = rest.trailing_zeros() as usize;
    (second < usize::BITS as usize).then_some((first, second))
}

extern "C" fn exit_group_worker(_arg: usize) -> isize {
    let cpu = WORKER_CPU.load(Ordering::Acquire);
    let _ = sched_setaffinity(0, cpu_bit(cpu));
    READY.fetch_add(1, Ordering::Release);
    while GO.load(Ordering::Acquire) == 0 {
        let _ = yield_();
    }
    for _ in 0..WORKER_DELAYS.load(Ordering::Relaxed) {
        let _ = yield_();
    }
    let _ = syscall(SYS_EXIT_GROUP, [23, 0, 0]);
    exit(90)
}

extern "C" fn blocking_worker(_arg: usize) -> isize {
    let cpu = BLOCKER_CPU.load(Ordering::Acquire);
    let _ = sched_setaffinity(0, cpu_bit(cpu));
    READY.fetch_add(1, Ordering::Release);
    while GO.load(Ordering::Acquire) == 0 {
        let _ = yield_();
    }
    let fd = BLOCKER_READ_FD.load(Ordering::Acquire);
    let mut fds = [PollFd {
        fd: fd as i32,
        events: 0x001,
        revents: 0,
    }];
    // No writer ever publishes data. This exercises keyed poll registration,
    // its wait queue cleanup while process teardown races us. An infinite wait
    // also makes an unexpected return unambiguously visible as a probe failure.
    let _ = sys_ppoll_time32(&mut fds, None);
    exit(94)
}

fn wait_child_with_timeout(pid: usize, status: &mut i32) -> isize {
    let deadline = now_ns().saturating_add(CHILD_TIMEOUT_NS);
    loop {
        let result = syscall(
            SYS_WAIT4,
            [pid, status as *mut i32 as usize, WNOHANG],
        );
        if result != 0 {
            return result;
        }
        if now_ns() >= deadline {
            let _ = kill(pid, SIGKILL);
            return waitpid(pid, status);
        }
        let _ = yield_();
    }
}

fn run_child(round: usize, leader_cpu: usize, worker_cpu: usize, blocker_cpu: usize) -> ! {
    READY.store(0, Ordering::Relaxed);
    GO.store(0, Ordering::Relaxed);
    WORKER_CPU.store(worker_cpu, Ordering::Release);
    BLOCKER_CPU.store(blocker_cpu, Ordering::Release);
    WORKER_DELAYS.store(round % 4, Ordering::Relaxed);
    let _ = sched_setaffinity(0, cpu_bit(leader_cpu));
    let mut pipe_fds = [-1i32; 2];
    if pipe(&mut pipe_fds) < 0 {
        exit(95);
    }
    BLOCKER_READ_FD.store(pipe_fds[0] as usize, Ordering::Release);
    if thread_create(exit_group_worker as usize, 0) <= 0 {
        exit(91);
    }
    if thread_create(blocking_worker as usize, 0) <= 0 {
        exit(93);
    }
    while READY.load(Ordering::Acquire) < 2 {
        let _ = yield_();
    }
    GO.store(1, Ordering::Release);
    for _ in 0..((round / 4) % 4) {
        let _ = yield_();
    }

    let args = [
        SELF_ARG0.as_ptr(),
        TARGET_ARG.as_ptr(),
        ptr::null::<u8>(),
    ];
    let result = exec(SELF_PATH, &args);
    println!(
        "teardown_race_probe: exec returned round={} result={}",
        round, result
    );
    exit(92)
}

#[no_mangle]
fn main(argc: usize, argv: &[&str]) -> i32 {
    if argc > 1 && argv[1] == "target" {
        return 42;
    }

    let affinity = sched_getaffinity(0);
    let Some((leader_cpu, worker_cpu)) =
        (affinity > 0).then(|| first_two_cpus(affinity as usize)).flatten()
    else {
        println!(
            "teardown_race_probe: SKIP need two CPUs affinity={:#x}",
            affinity
        );
        return 0;
    };
    let remaining = affinity as usize & !cpu_bit(leader_cpu) & !cpu_bit(worker_cpu);
    let blocker_cpu = if remaining == 0 {
        worker_cpu
    } else {
        remaining.trailing_zeros() as usize
    };

    let mut failures = 0usize;
    let mut exec_wins = 0usize;
    let mut exit_wins = 0usize;
    for round in 0..ROUNDS {
        let child = fork();
        if child < 0 {
            println!("teardown_race_probe: fork failed round={} ret={}", round, child);
            failures += 1;
            break;
        }
        if child == 0 {
            run_child(round, leader_cpu, worker_cpu, blocker_cpu);
        }

        let mut status = -1;
        let waited = wait_child_with_timeout(child as usize, &mut status);
        if waited != child {
            println!(
                "teardown_race_probe: wait failed round={} pid={} waited={} status={}",
                round, child, waited, status
            );
            failures += 1;
            continue;
        }
        match status {
            status if status == (42 << 8) => exec_wins += 1,
            status if status == (23 << 8) => exit_wins += 1,
            9 => {
                println!("teardown_race_probe: TIMEOUT round={}", round);
                failures += 1;
            }
            _ => {
                println!(
                    "teardown_race_probe: unexpected status round={} status={}",
                    round, status
                );
                failures += 1;
            }
        }
    }

    println!(
        "teardown_race_probe: rounds={} exec_wins={} exit_wins={} failures={}",
        ROUNDS, exec_wins, exit_wins, failures
    );
    if failures == 0 {
        println!("teardown_race_probe: PASS");
        0
    } else {
        println!("teardown_race_probe: FAIL");
        1
    }
}
