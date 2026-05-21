#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{
    exit, fork, get_time, getpid, getpriority, sched_getparam, sched_getscheduler,
    sched_setscheduler, setpriority, wait, yield_, SchedParam, PRIO_PROCESS, SCHED_OTHER, SCHED_RR,
};

fn busy_for(ms: isize) -> usize {
    let deadline = get_time() + ms;
    let mut acc = 0usize;
    while get_time() < deadline {
        acc = acc.wrapping_add(1);
        if acc & 0xfff == 0 {
            yield_();
        }
    }
    acc
}

fn child(label: usize, nice: i32) -> ! {
    let ret = setpriority(PRIO_PROCESS, 0, nice);
    let observed = getpriority(PRIO_PROCESS, 0);
    println!(
        "sched_cfs_test child{} pid={} setpriority={} nice={}",
        label,
        getpid(),
        ret,
        observed
    );
    let work = busy_for(120);
    println!("sched_cfs_test child{} work={}", label, work);
    exit(0);
}

#[no_mangle]
fn main() -> i32 {
    println!("sched_cfs_test: start");

    let policy = sched_getscheduler(0);
    let mut param = SchedParam { sched_priority: -1 };
    let getparam = sched_getparam(0, &mut param);
    println!(
        "sched_cfs_test default policy={} getparam={} prio={}",
        policy, getparam, param.sched_priority
    );
    assert_eq!(policy, SCHED_OTHER as isize);
    assert_eq!(getparam, 0);
    assert_eq!(param.sched_priority, 0);

    assert_eq!(setpriority(PRIO_PROCESS, 0, 7), 0);
    assert_eq!(getpriority(PRIO_PROCESS, 0), 7);
    assert_eq!(
        sched_setscheduler(0, SCHED_OTHER, &SchedParam { sched_priority: 0 }),
        0
    );

    let rr_param = SchedParam { sched_priority: 2 };
    assert_eq!(sched_setscheduler(0, SCHED_RR, &rr_param), 0);
    assert_eq!(sched_getscheduler(0), SCHED_RR as isize);
    assert_eq!(
        sched_setscheduler(0, SCHED_OTHER, &SchedParam { sched_priority: 0 }),
        0
    );

    let nice_values = [-5, 0, 10];
    for (idx, nice) in nice_values.iter().enumerate() {
        let pid = fork();
        if pid == 0 {
            child(idx, *nice);
        }
        assert!(pid > 0);
    }

    let mut exit_code = 0;
    for _ in 0..nice_values.len() {
        let waited = wait(&mut exit_code);
        assert!(waited > 0);
        assert_eq!(exit_code, 0);
    }

    println!("sched_cfs_test: pass");
    0
}
