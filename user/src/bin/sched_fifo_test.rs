#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{
    exit, fork, sched_getparam, sched_getscheduler, sched_setscheduler, wait, waitpid, yield_,
    SchedParam, SCHED_FIFO, SCHED_OTHER, SCHED_RR,
};

fn busy(rounds: usize) {
    let mut acc = 0usize;
    for _ in 0..rounds {
        acc = acc.wrapping_mul(1664525).wrapping_add(1013904223);
    }
    if acc == usize::MAX {
        println!("sched_fifo_test impossible={}", acc);
    }
}

fn set_fifo(prio: i32) {
    assert_eq!(
        sched_setscheduler(
            0,
            SCHED_FIFO,
            &SchedParam {
                sched_priority: prio
            }
        ),
        0
    );
}

fn set_other() {
    assert_eq!(
        sched_setscheduler(0, SCHED_OTHER, &SchedParam { sched_priority: 0 }),
        0
    );
}

fn inherited_fifo_child(rounds: usize) -> ! {
    busy(rounds);
    exit(0);
}

fn reprio_fifo_child(prio: i32, rounds: usize) -> ! {
    set_fifo(prio);
    busy(rounds);
    exit(0);
}

fn check_abi() {
    let mut param = SchedParam { sched_priority: -1 };

    set_fifo(7);
    assert_eq!(sched_getscheduler(0), SCHED_FIFO as isize);
    assert_eq!(sched_getparam(0, &mut param), 0);
    assert_eq!(param.sched_priority, 7);

    assert!(sched_setscheduler(0, SCHED_FIFO, &SchedParam { sched_priority: 0 }) < 0);
    assert!(
        sched_setscheduler(
            0,
            SCHED_FIFO,
            &SchedParam {
                sched_priority: 100
            }
        ) < 0
    );
    assert!(sched_setscheduler(0, SCHED_OTHER, &SchedParam { sched_priority: 1 }) < 0);
    assert!(sched_setscheduler(0, SCHED_RR, &SchedParam { sched_priority: 0 }) < 0);

    set_other();
}

fn check_fifo_yield_order() {
    set_fifo(50);

    let pid1 = fork();
    if pid1 == 0 {
        inherited_fifo_child(60_000);
    }
    assert!(pid1 > 0);

    let pid2 = fork();
    if pid2 == 0 {
        inherited_fifo_child(60_000);
    }
    assert!(pid2 > 0);

    yield_();

    let mut status = 0;
    assert_eq!(wait(&mut status), pid1);
    assert_eq!(status, 0);
    assert_eq!(wait(&mut status), pid2);
    assert_eq!(status, 0);

    set_other();
}

fn check_fifo_priority_preemption() {
    set_fifo(50);

    let low_pid = fork();
    if low_pid == 0 {
        reprio_fifo_child(5, 800_000);
    }
    assert!(low_pid > 0);

    let high_pid = fork();
    if high_pid == 0 {
        reprio_fifo_child(60, 40_000);
    }
    assert!(high_pid > 0);

    yield_();

    let mut status = 0;
    assert_eq!(waitpid(high_pid as usize, &mut status), high_pid);
    assert_eq!(status, 0);

    set_other();
    assert_eq!(waitpid(low_pid as usize, &mut status), low_pid);
    assert_eq!(status, 0);
}

#[no_mangle]
fn main() -> i32 {
    println!("sched_fifo_test: start");
    check_abi();
    check_fifo_yield_order();
    check_fifo_priority_preemption();
    println!("sched_fifo_test: pass");
    0
}
