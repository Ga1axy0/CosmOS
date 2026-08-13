#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::sync::atomic::{AtomicUsize, Ordering};
use user_lib::{
    clock_gettime, clock_gettime_ns, exit, fork, getcpu, sched_getaffinity, sched_setaffinity,
    setitimer, sigaction, sleep_blocking, syscall, thread_create, waitpid, waittid, yield_,
    Itimerval, SignalAction, TimeVal, Timespec, ITIMER_PROF, ITIMER_VIRTUAL, SIGPROF, SIGVTALRM,
};

// Linux generic syscall numbers, used on RISC-V by CosmOS.
const SYS_TIMES: usize = 153;
const SYS_GETRUSAGE: usize = 165;
const RUSAGE_SELF: i32 = 0;
const RUSAGE_CHILDREN: i32 = -1;
const CLOCK_PROCESS_CPUTIME_ID: i32 = 2;
const USER_HZ: u64 = 100;

const MAX_WORKERS: usize = 4;
const RUN_NS: u64 = 600_000_000;
const START_TIMEOUT_NS: u64 = 5_000_000_000;
const TIMER_TIMEOUT_NS: u64 = 2_000_000_000;
const CHILD_RUN_NS: u64 = 250_000_000;

static TARGET_CPUS: [AtomicUsize; MAX_WORKERS] = [const { AtomicUsize::new(0) }; MAX_WORKERS];
static READY: AtomicUsize = AtomicUsize::new(0);
static GO: AtomicUsize = AtomicUsize::new(0);
static STOP: AtomicUsize = AtomicUsize::new(0);
static PIN_FAILURES: AtomicUsize = AtomicUsize::new(0);
static CHECKSUM: AtomicUsize = AtomicUsize::new(0);
static VTALRM_COUNT: AtomicUsize = AtomicUsize::new(0);
static PROF_COUNT: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RUsage {
    ru_utime: TimeVal,
    ru_stime: TimeVal,
    ru_maxrss: isize,
    ru_ixrss: isize,
    ru_idrss: isize,
    ru_isrss: isize,
    ru_minflt: isize,
    ru_majflt: isize,
    ru_nswap: isize,
    ru_inblock: isize,
    ru_oublock: isize,
    ru_msgsnd: isize,
    ru_msgrcv: isize,
    ru_nsignals: isize,
    ru_nvcsw: isize,
    ru_nivcsw: isize,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Tms {
    tms_utime: usize,
    tms_stime: usize,
    tms_cutime: usize,
    tms_cstime: usize,
}

#[derive(Clone, Copy, Default)]
struct AccountingSample {
    clock_ns: u64,
    rusage_ns: u64,
    times_ns: u64,
}

fn monotonic_ns() -> u64 {
    let value = clock_gettime_ns(user_lib::CLOCK_MONOTONIC);
    if value < 0 {
        0
    } else {
        value as u64
    }
}

fn timeval_ns(value: TimeVal) -> u64 {
    (value.sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add((value.usec as u64).saturating_mul(1_000))
}

fn raw_getrusage(who: i32, usage: &mut RUsage) -> isize {
    syscall(
        SYS_GETRUSAGE,
        [who as usize, usage as *mut RUsage as usize, 0],
    )
}

fn raw_times(value: &mut Tms) -> isize {
    syscall(SYS_TIMES, [value as *mut Tms as usize, 0, 0])
}

fn accounting_sample() -> Option<AccountingSample> {
    let mut clock = Timespec::new();
    if clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &mut clock) < 0 {
        return None;
    }
    let mut usage = RUsage::default();
    if raw_getrusage(RUSAGE_SELF, &mut usage) < 0 {
        return None;
    }
    let mut times = Tms::default();
    if raw_times(&mut times) < 0 {
        return None;
    }
    Some(AccountingSample {
        clock_ns: clock.as_ns(),
        rusage_ns: timeval_ns(usage.ru_utime).saturating_add(timeval_ns(usage.ru_stime)),
        times_ns: (times.tms_utime as u64)
            .saturating_add(times.tms_stime as u64)
            .saturating_mul(1_000_000_000 / USER_HZ),
    })
}

fn children_sample() -> Option<(u64, u64)> {
    let mut usage = RUsage::default();
    if raw_getrusage(RUSAGE_CHILDREN, &mut usage) < 0 {
        return None;
    }
    let mut times = Tms::default();
    if raw_times(&mut times) < 0 {
        return None;
    }
    let rusage_ns = timeval_ns(usage.ru_utime).saturating_add(timeval_ns(usage.ru_stime));
    let times_ns = (times.tms_cutime as u64)
        .saturating_add(times.tms_cstime as u64)
        .saturating_mul(1_000_000_000 / USER_HZ);
    Some((rusage_ns, times_ns))
}

#[inline(never)]
fn burn_chunk(mut value: usize) -> usize {
    // The dependency chain makes the loop real CPU work and avoids memory or
    // syscall time dominating the accounting interval.
    for round in 0usize..80_000 {
        value = value
            .wrapping_add(round ^ 0x9e37_79b9)
            .rotate_left(7)
            .wrapping_mul(0xd6e8_feb8_6659_fd93usize)
            ^ value.rotate_right(11);
    }
    value
}

fn cpu_bit(cpu: usize) -> usize {
    if cpu < usize::BITS as usize {
        1usize << cpu
    } else {
        0
    }
}

fn cpu_list(mask: usize, output: &mut [usize; MAX_WORKERS]) -> usize {
    let mut count = 0;
    for cpu in 0..usize::BITS as usize {
        if mask & cpu_bit(cpu) != 0 {
            output[count] = cpu;
            count += 1;
            if count == output.len() {
                break;
            }
        }
    }
    count
}

extern "C" fn worker_entry(index: usize) -> isize {
    let cpu = TARGET_CPUS[index].load(Ordering::Acquire);
    if sched_setaffinity(0, cpu_bit(cpu)) < 0 {
        PIN_FAILURES.fetch_add(1, Ordering::Relaxed);
    } else {
        let _ = yield_();
        let observed = getcpu();
        if observed < 0 || observed as usize != cpu {
            PIN_FAILURES.fetch_add(1, Ordering::Relaxed);
        }
    }

    READY.fetch_add(1, Ordering::Release);
    while GO.load(Ordering::Acquire) == 0 {
        let _ = yield_();
    }

    let mut checksum = index.wrapping_add(1);
    while STOP.load(Ordering::Acquire) == 0 {
        checksum = burn_chunk(checksum);
    }
    CHECKSUM.fetch_xor(checksum, Ordering::Relaxed);
    exit(0)
}

fn nondecreasing(previous: AccountingSample, next: AccountingSample) -> bool {
    next.clock_ns >= previous.clock_ns
        && next.rusage_ns >= previous.rusage_ns
        && next.times_ns >= previous.times_ns
}

fn run_multithread_probe() -> usize {
    let affinity = sched_getaffinity(0);
    if affinity <= 0 {
        println!(
            "cpu_accounting_probe: sched_getaffinity failed: {}",
            affinity
        );
        return 1;
    }

    let mut cpus = [0usize; MAX_WORKERS];
    let requested = cpu_list(affinity as usize, &mut cpus);
    if requested == 0 {
        println!("cpu_accounting_probe: affinity contains no usable CPU");
        return 1;
    }

    let mut tids = [0usize; MAX_WORKERS];
    let mut workers = 0;
    for index in 0..requested {
        TARGET_CPUS[index].store(cpus[index], Ordering::Release);
        let tid = thread_create(worker_entry as usize, index);
        if tid <= 0 {
            println!(
                "cpu_accounting_probe: thread_create index={} failed: {}",
                index, tid
            );
            break;
        }
        tids[index] = tid as usize;
        workers += 1;
    }
    if workers == 0 {
        return 1;
    }

    let startup_deadline = monotonic_ns().saturating_add(START_TIMEOUT_NS);
    while READY.load(Ordering::Acquire) < workers && monotonic_ns() < startup_deadline {
        let _ = yield_();
    }
    if READY.load(Ordering::Acquire) != workers {
        println!(
            "cpu_accounting_probe: worker startup timeout ready={} workers={}",
            READY.load(Ordering::Acquire),
            workers
        );
        GO.store(1, Ordering::Release);
        STOP.store(1, Ordering::Release);
        for index in 0..workers {
            let _ = waittid(tids[index]);
        }
        return 1;
    }

    let Some(before) = accounting_sample() else {
        println!("cpu_accounting_probe: initial process accounting read failed");
        GO.store(1, Ordering::Release);
        STOP.store(1, Ordering::Release);
        for index in 0..workers {
            let _ = waittid(tids[index]);
        }
        return 1;
    };

    let start_wall = monotonic_ns();
    let deadline = start_wall.saturating_add(RUN_NS);
    GO.store(1, Ordering::Release);
    let mut previous = before;
    let mut backsteps = 0usize;
    let mut samples = 0usize;
    while monotonic_ns() < deadline {
        sleep_blocking(20);
        if let Some(next) = accounting_sample() {
            if !nondecreasing(previous, next) {
                backsteps += 1;
            }
            previous = next;
            samples += 1;
        } else {
            println!("cpu_accounting_probe: process accounting sample failed during run");
            backsteps += 1;
        }
    }
    let stop_wall = monotonic_ns();
    STOP.store(1, Ordering::Release);

    let mut join_failures = 0usize;
    for index in 0..workers {
        let result = waittid(tids[index]);
        if result != 0 {
            println!(
                "cpu_accounting_probe: waittid tid={} returned {}",
                tids[index], result
            );
            join_failures += 1;
        }
    }
    let _ = yield_();
    let Some(after) = accounting_sample() else {
        println!("cpu_accounting_probe: final process accounting read failed");
        return 1;
    };

    let wall_ns = stop_wall.saturating_sub(start_wall);
    let clock_delta = after.clock_ns.saturating_sub(before.clock_ns);
    let rusage_delta = after.rusage_ns.saturating_sub(before.rusage_ns);
    let times_delta = after.times_ns.saturating_sub(before.times_ns);
    let expected = wall_ns.saturating_mul(workers as u64);
    let ratio_milli = clock_delta.saturating_mul(1_000) / expected.max(1);

    println!(
        "cpu_accounting_probe: threads workers={} cpus={:#x} samples={} wall_ns={} expected_ns={} clock_ns={} rusage_ns={} times_ns={} ratio_milli={} checksum={:#x}",
        workers,
        affinity as usize,
        samples,
        wall_ns,
        expected,
        clock_delta,
        rusage_delta,
        times_delta,
        ratio_milli,
        CHECKSUM.load(Ordering::Relaxed)
    );

    let mut failures = backsteps + join_failures + PIN_FAILURES.load(Ordering::Relaxed);
    // With N pinned runnable workers, process CPU time should be close to the
    // sum of N wall-time intervals. The deliberately broad bounds tolerate
    // emulation jitter while still catching single-process-state accounting,
    // which tends toward only one wall-time interval.
    let lower = expected.saturating_mul(55) / 100;
    let upper = expected
        .saturating_mul(220)
        .saturating_div(100)
        .saturating_add(200_000_000);
    if clock_delta < lower || clock_delta > upper {
        println!(
            "cpu_accounting_probe: aggregate process CPU outside bounds [{}, {}]",
            lower, upper
        );
        failures += 1;
    }

    let minimum = clock_delta.min(rusage_delta).min(times_delta);
    let maximum = clock_delta.max(rusage_delta).max(times_delta);
    let cross_tolerance = expected / 2 + 100_000_000;
    if maximum.saturating_sub(minimum) > cross_tolerance {
        println!(
            "cpu_accounting_probe: clock/getrusage/times disagree by {} ns (tolerance {})",
            maximum.saturating_sub(minimum),
            cross_tolerance
        );
        failures += 1;
    }
    if workers < 2 {
        println!(
            "cpu_accounting_probe: NOTE only one CPU available; cross-hart aggregation was not exercised"
        );
    }
    failures
}

fn burn_for_wall(duration_ns: u64, seed: usize) -> usize {
    let deadline = monotonic_ns().saturating_add(duration_ns);
    let mut value = seed;
    while monotonic_ns() < deadline {
        value = burn_chunk(value);
    }
    value
}

fn run_child_probe(first_cpu: usize) -> usize {
    let Some(before) = children_sample() else {
        println!("cpu_accounting_probe: initial children accounting read failed");
        return 1;
    };
    let child = fork();
    if child < 0 {
        println!("cpu_accounting_probe: fork failed: {}", child);
        return 1;
    }
    if child == 0 {
        if sched_setaffinity(0, cpu_bit(first_cpu)) < 0 {
            exit(3);
        }
        let value = burn_for_wall(CHILD_RUN_NS, 0x1234_5678);
        CHECKSUM.store(value, Ordering::Relaxed);
        exit(0);
    }

    let mut status = -1;
    let waited = waitpid(child as usize, &mut status);
    let Some(after) = children_sample() else {
        println!("cpu_accounting_probe: final children accounting read failed");
        return 1;
    };
    let rusage_delta = after.0.saturating_sub(before.0);
    let times_delta = after.1.saturating_sub(before.1);
    println!(
        "cpu_accounting_probe: child pid={} waited={} status={:#x} rusage_children_ns={} times_children_ns={}",
        child, waited, status, rusage_delta, times_delta
    );

    let mut failures = 0;
    if waited != child || status != 0 {
        failures += 1;
    }
    // The child burns for 250 ms; 50 ms is intentionally conservative for a
    // slow or oversubscribed emulator. Both interfaces must observe a final,
    // nontrivial value after wait/reap.
    if rusage_delta < 50_000_000 || times_delta < 50_000_000 {
        println!("cpu_accounting_probe: child CPU time was not fully accumulated");
        failures += 1;
    }
    let child_difference = rusage_delta.abs_diff(times_delta);
    if child_difference > 100_000_000 {
        println!(
            "cpu_accounting_probe: child getrusage/times differ by {} ns",
            child_difference
        );
        failures += 1;
    }
    failures
}

extern "C" fn on_vtalrm(_signal: i32) {
    VTALRM_COUNT.fetch_add(1, Ordering::Relaxed);
}

extern "C" fn on_prof(_signal: i32) {
    PROF_COUNT.fetch_add(1, Ordering::Relaxed);
}

fn run_one_cpu_timer(which: i32, label: &str, counter: &AtomicUsize, value_usec: usize) -> usize {
    counter.store(0, Ordering::Relaxed);
    let timer = Itimerval {
        it_interval: TimeVal { sec: 0, usec: 0 },
        it_value: TimeVal {
            sec: 0,
            usec: value_usec,
        },
    };
    if setitimer(which, Some(&timer), None) < 0 {
        println!("cpu_accounting_probe: setitimer({}) failed", label);
        return 1;
    }

    let cpu_before = accounting_sample().map_or(0, |sample| sample.clock_ns);
    let start = monotonic_ns();
    let deadline = start.saturating_add(TIMER_TIMEOUT_NS);
    let mut value = which as usize + 1;
    while counter.load(Ordering::Acquire) == 0 && monotonic_ns() < deadline {
        value = burn_chunk(value);
    }
    CHECKSUM.fetch_xor(value, Ordering::Relaxed);
    let wall_ns = monotonic_ns().saturating_sub(start);
    let cpu_after = accounting_sample().map_or(cpu_before, |sample| sample.clock_ns);
    let count = counter.load(Ordering::Acquire);

    let disarm = Itimerval::default();
    let _ = setitimer(which, Some(&disarm), None);
    println!(
        "cpu_accounting_probe: timer={} count={} wall_ns={} process_cpu_ns={}",
        label,
        count,
        wall_ns,
        cpu_after.saturating_sub(cpu_before)
    );
    usize::from(count == 0)
}

fn run_timer_probe() -> usize {
    let virtual_action = SignalAction {
        handler: on_vtalrm as usize,
        sa_flags: 0,
        sa_mask: 0,
    };
    let prof_action = SignalAction {
        handler: on_prof as usize,
        sa_flags: 0,
        sa_mask: 0,
    };
    if sigaction(SIGVTALRM, Some(&virtual_action), None) < 0
        || sigaction(SIGPROF, Some(&prof_action), None) < 0
    {
        println!("cpu_accounting_probe: installing CPU timer signal handlers failed");
        return 1;
    }

    let virtual_failures =
        run_one_cpu_timer(ITIMER_VIRTUAL, "ITIMER_VIRTUAL", &VTALRM_COUNT, 60_000);
    let prof_failures = run_one_cpu_timer(ITIMER_PROF, "ITIMER_PROF", &PROF_COUNT, 80_000);
    virtual_failures + prof_failures
}

#[no_mangle]
fn main() -> i32 {
    let affinity = sched_getaffinity(0);
    if affinity <= 0 {
        println!(
            "cpu_accounting_probe: cannot read initial affinity: {}",
            affinity
        );
        return 2;
    }
    let first_cpu = (affinity as usize).trailing_zeros() as usize;

    let mut failures = run_multithread_probe();
    failures += run_child_probe(first_cpu);
    failures += run_timer_probe();

    if failures == 0 {
        println!("cpu_accounting_probe: PASS");
        0
    } else {
        println!("cpu_accounting_probe: FAIL failures={}", failures);
        1
    }
}
