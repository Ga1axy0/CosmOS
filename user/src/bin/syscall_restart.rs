#![no_std]
#![no_main]

use user_lib::{
    ITIMER_REAL, Itimerval, SIGALRM, SignalAction, TimeVal, fork, pipe, read, setitimer, sigaction, sleep, write
};

#[macro_use]
extern crate user_lib;

extern crate alloc;

extern "C" fn on_alarm(_sig: i32) {
    println!("got SIGALRM");
}

#[no_mangle]
fn main() {
    let mut fds = [0i32; 2];
    if pipe(&mut fds) < 0 {
        println!("pipe failed");
        return;
    }

    let action = SignalAction {
        handler: on_alarm as usize,
        sa_flags: 0x1000_0000, // SA_RESTART
        sa_mask: 0,
    };

    if sigaction(SIGALRM, Some(&action), None) < 0 {
        println!("sigaction failed");
        return;
    }

    let pid = fork();
    if pid == 0 {
        // 子进程晚一点写数据，确保父进程先卡在 read 上
        println!("Child: sleeping for 2 seconds before writing");
        sleep(2000);
        println!("Child: writing to pipe");
        let n = write(fds[1] as usize, b"ok");
        println!("Child: write returned {}", n);
        println!("Child: write done, exiting");
        return;
    }

    let timer = Itimerval {
        it_value: TimeVal { sec: 1, usec: 0 },
        it_interval: TimeVal { sec: 0, usec: 0 },
    };
    let _ = setitimer(ITIMER_REAL, Some(&timer), None);

    let mut buf = [0u8; 8];
    println!("Parent: going to read");
    let n = read(fds[0] as usize, &mut buf);

    println!("read returned {}", n);
}
