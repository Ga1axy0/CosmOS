#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{
    clock_gettime_ns, exit, fork, get_time, getcpu, getpid, sched_getaffinity,
    sched_setaffinity, wait, yield_, CLOCK_MONOTONIC, CLOCK_REALTIME,
};

const DEFAULT_WORKERS: usize = 4;
const DEFAULT_ITERS: usize = 20_000;
const DEFAULT_YIELD_EVERY: usize = 8;
const DEFAULT_MIGRATE_EVERY: usize = 257;
const LOG_LIMIT: usize = 12;

#[derive(Clone, Copy)]
struct ProbeSummary {
    anomalies: usize,
    mono_backsteps: usize,
    real_backsteps: usize,
    wall_backsteps: usize,
    cpu_changes: usize,
    forced_migrations: usize,
    failed_forced_migrations: usize,
    setaffinity_failures: usize,
    cpu_observe_failures: usize,
    unique_cpu_mask: usize,
    max_mono_back_ns: u64,
    max_real_back_ns: u64,
    max_wall_back_ms: isize,
    max_mono_jump_ns: u64,
    max_real_jump_ns: u64,
    max_wall_jump_ms: isize,
}

impl ProbeSummary {
    const fn new() -> Self {
        Self {
            anomalies: 0,
            mono_backsteps: 0,
            real_backsteps: 0,
            wall_backsteps: 0,
            cpu_changes: 0,
            forced_migrations: 0,
            failed_forced_migrations: 0,
            setaffinity_failures: 0,
            cpu_observe_failures: 0,
            unique_cpu_mask: 0,
            max_mono_back_ns: 0,
            max_real_back_ns: 0,
            max_wall_back_ms: 0,
            max_mono_jump_ns: 0,
            max_real_jump_ns: 0,
            max_wall_jump_ms: 0,
        }
    }
}

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
    if any { value } else { default }
}

fn mask_bit(cpu: usize) -> usize {
    if cpu >= usize::BITS as usize {
        0
    } else {
        1usize << cpu
    }
}

fn count_bits(mask: usize) -> usize {
    mask.count_ones() as usize
}

fn next_cpu_in_mask(mask: usize, current: usize) -> usize {
    let limit = usize::BITS as usize;
    if limit == 0 {
        return current;
    }
    for step in 1..=limit {
        let candidate = (current + step) % limit;
        if mask & mask_bit(candidate) != 0 {
            return candidate;
        }
    }
    current
}

fn sample_ns(clockid: i32, label: &str) -> u64 {
    let ret = clock_gettime_ns(clockid);
    if ret < 0 {
        println!(
            "time_smp_probe: pid={} clock_gettime({}) failed: {}",
            getpid(),
            label,
            ret
        );
        exit(2);
    }
    ret as u64
}

fn sample_cpu() -> isize {
    getcpu()
}

fn read_affinity_mask() -> usize {
    let ret = sched_getaffinity(0);
    if ret < 0 {
        println!(
            "time_smp_probe: pid={} sched_getaffinity failed: {}",
            getpid(),
            ret
        );
        exit(2);
    }
    ret as usize
}

fn try_force_migration(
    worker: usize,
    iter: usize,
    base_affinity: usize,
    prev_cpu: usize,
    summary: &mut ProbeSummary,
    logged: &mut usize,
) -> usize {
    let pid = getpid();
    let target_cpu = next_cpu_in_mask(base_affinity, prev_cpu);
    if target_cpu == prev_cpu {
        return prev_cpu;
    }

    let singleton = mask_bit(target_cpu);
    let set_ret = sched_setaffinity(0, singleton);
    if set_ret < 0 {
        summary.setaffinity_failures += 1;
        if *logged < LOG_LIMIT {
            println!(
                "time_smp_probe: worker={} pid={} iter={} setaffinity(singleton cpu={}) failed {}",
                worker,
                pid,
                iter,
                target_cpu,
                set_ret
            );
            *logged += 1;
        }
        return prev_cpu;
    }

    summary.forced_migrations += 1;
    let _ = yield_();

    let observed = sample_cpu();
    let observed_cpu = if observed < 0 {
        summary.cpu_observe_failures += 1;
        prev_cpu
    } else {
        observed as usize
    };

    if observed_cpu == target_cpu {
        if observed_cpu != prev_cpu {
            summary.cpu_changes += 1;
        }
        summary.unique_cpu_mask |= mask_bit(observed_cpu);
    } else {
        summary.failed_forced_migrations += 1;
        if *logged < LOG_LIMIT {
            println!(
                "time_smp_probe: worker={} pid={} iter={} forced migration target_cpu={} observed_cpu={}",
                worker,
                pid,
                iter,
                target_cpu,
                observed_cpu
            );
            *logged += 1;
        }
    }

    let restore_ret = sched_setaffinity(0, base_affinity);
    if restore_ret < 0 {
        summary.setaffinity_failures += 1;
        if *logged < LOG_LIMIT {
            println!(
                "time_smp_probe: worker={} pid={} iter={} restore affinity {:#x} failed {}",
                worker,
                pid,
                iter,
                base_affinity,
                restore_ret
            );
            *logged += 1;
        }
    }

    observed_cpu
}

fn run_probe(worker: usize, iters: usize, yield_every: usize, migrate_every: usize) -> ProbeSummary {
    let pid = getpid();
    let mut summary = ProbeSummary::new();
    let mut logged = 0usize;
    let base_affinity = read_affinity_mask();
    let mut prev_cpu = sample_cpu();
    if prev_cpu < 0 {
        println!("time_smp_probe: worker={} pid={} getcpu failed: {}", worker, pid, prev_cpu);
        exit(2);
    }
    let mut prev_cpu = prev_cpu as usize;
    summary.unique_cpu_mask |= mask_bit(prev_cpu);

    let mut prev_mono = sample_ns(CLOCK_MONOTONIC, "MONOTONIC");
    let mut prev_real = sample_ns(CLOCK_REALTIME, "REALTIME");
    let mut prev_wall = get_time();

    println!(
        "time_smp_probe: worker={} pid={} start cpu={} affinity={:#x} cpu_count={} mono_ns={} real_ns={} wall_ms={} iters={} yield_every={} migrate_every={}",
        worker,
        pid,
        prev_cpu,
        base_affinity,
        count_bits(base_affinity),
        prev_mono,
        prev_real,
        prev_wall,
        iters,
        yield_every,
        migrate_every
    );

    for iter in 0..iters {
        let cpu = sample_cpu();
        let cpu = if cpu < 0 {
            summary.cpu_observe_failures += 1;
            prev_cpu
        } else {
            cpu as usize
        };

        if cpu != prev_cpu {
            summary.cpu_changes += 1;
            summary.unique_cpu_mask |= mask_bit(cpu);
            if logged < LOG_LIMIT {
                println!(
                    "time_smp_probe: worker={} pid={} iter={} cpu change {} -> {}",
                    worker,
                    pid,
                    iter,
                    prev_cpu,
                    cpu
                );
                logged += 1;
            }
        }

        let mono = sample_ns(CLOCK_MONOTONIC, "MONOTONIC");
        let real = sample_ns(CLOCK_REALTIME, "REALTIME");
        let wall = get_time();

        if mono < prev_mono {
            let back = prev_mono - mono;
            summary.anomalies += 1;
            summary.mono_backsteps += 1;
            summary.max_mono_back_ns = summary.max_mono_back_ns.max(back);
            if logged < LOG_LIMIT {
                println!(
                    "time_smp_probe: worker={} pid={} iter={} cpu={} MONOTONIC backstep {} ns (prev={} now={})",
                    worker,
                    pid,
                    iter,
                    cpu,
                    back,
                    prev_mono,
                    mono
                );
                logged += 1;
            }
        } else {
            summary.max_mono_jump_ns = summary.max_mono_jump_ns.max(mono - prev_mono);
        }

        if real < prev_real {
            let back = prev_real - real;
            summary.anomalies += 1;
            summary.real_backsteps += 1;
            summary.max_real_back_ns = summary.max_real_back_ns.max(back);
            if logged < LOG_LIMIT {
                println!(
                    "time_smp_probe: worker={} pid={} iter={} cpu={} REALTIME backstep {} ns (prev={} now={})",
                    worker,
                    pid,
                    iter,
                    cpu,
                    back,
                    prev_real,
                    real
                );
                logged += 1;
            }
        } else {
            summary.max_real_jump_ns = summary.max_real_jump_ns.max(real - prev_real);
        }

        if wall < prev_wall {
            let back = prev_wall - wall;
            summary.anomalies += 1;
            summary.wall_backsteps += 1;
            summary.max_wall_back_ms = summary.max_wall_back_ms.max(back);
            if logged < LOG_LIMIT {
                println!(
                    "time_smp_probe: worker={} pid={} iter={} cpu={} get_time backstep {} ms (prev={} now={})",
                    worker,
                    pid,
                    iter,
                    cpu,
                    back,
                    prev_wall,
                    wall
                );
                logged += 1;
            }
        } else {
            summary.max_wall_jump_ms = summary.max_wall_jump_ms.max(wall - prev_wall);
        }

        prev_mono = mono;
        prev_real = real;
        prev_wall = wall;
        prev_cpu = cpu;

        if migrate_every != 0 && count_bits(base_affinity) > 1 && iter % migrate_every == 0 {
            prev_cpu = try_force_migration(
                worker,
                iter,
                base_affinity,
                prev_cpu,
                &mut summary,
                &mut logged,
            );
        } else if yield_every != 0 && iter % yield_every == 0 {
            let _ = yield_();
        }
    }

    println!(
        "time_smp_probe: worker={} pid={} done anomalies={} mono_backsteps={} real_backsteps={} wall_backsteps={} cpu_changes={} forced_migrations={} failed_forced_migrations={} setaffinity_failures={} cpu_observe_failures={} unique_cpu_mask={:#x} max_mono_back_ns={} max_real_back_ns={} max_wall_back_ms={} max_mono_jump_ns={} max_real_jump_ns={} max_wall_jump_ms={}",
        worker,
        pid,
        summary.anomalies,
        summary.mono_backsteps,
        summary.real_backsteps,
        summary.wall_backsteps,
        summary.cpu_changes,
        summary.forced_migrations,
        summary.failed_forced_migrations,
        summary.setaffinity_failures,
        summary.cpu_observe_failures,
        summary.unique_cpu_mask,
        summary.max_mono_back_ns,
        summary.max_real_back_ns,
        summary.max_wall_back_ms,
        summary.max_mono_jump_ns,
        summary.max_real_jump_ns,
        summary.max_wall_jump_ms
    );

    summary
}

#[no_mangle]
fn main(argc: usize, argv: &[&str]) -> i32 {
    let mut workers = if argc > 1 {
        parse_usize(argv[1], DEFAULT_WORKERS)
    } else {
        DEFAULT_WORKERS
    };
    workers = workers.max(1).min(8);

    let iters = if argc > 2 {
        parse_usize(argv[2], DEFAULT_ITERS)
    } else {
        DEFAULT_ITERS
    };
    let yield_every = if argc > 3 {
        parse_usize(argv[3], DEFAULT_YIELD_EVERY)
    } else {
        DEFAULT_YIELD_EVERY
    };
    let migrate_every = if argc > 4 {
        parse_usize(argv[4], DEFAULT_MIGRATE_EVERY)
    } else {
        DEFAULT_MIGRATE_EVERY
    };

    println!(
        "time_smp_probe: start pid={} workers={} iters={} yield_every={} migrate_every={}",
        getpid(),
        workers,
        iters,
        yield_every,
        migrate_every
    );

    for worker in 1..workers {
        let pid = fork();
        assert!(pid >= 0, "time_smp_probe: fork failed for worker{}: {}", worker, pid);
        if pid == 0 {
            let summary = run_probe(worker, iters, yield_every, migrate_every);
            exit(if summary.anomalies == 0 { 0 } else { 1 });
        }
    }

    let parent_summary = run_probe(0, iters, yield_every, migrate_every);
    let mut failed = if parent_summary.anomalies == 0 { 0 } else { 1 };
    let mut exit_code = 0;

    for _ in 1..workers {
        let waited = wait(&mut exit_code);
        assert!(waited > 0, "time_smp_probe: wait failed: {}", waited);
        if exit_code != 0 {
            failed += 1;
        }
    }

    println!(
        "time_smp_probe: complete workers={} failed={} parent_anomalies={} parent_cpu_changes={} parent_unique_cpu_mask={:#x}",
        workers,
        failed,
        parent_summary.anomalies,
        parent_summary.cpu_changes,
        parent_summary.unique_cpu_mask
    );

    if failed == 0 { 0 } else { 1 }
}
