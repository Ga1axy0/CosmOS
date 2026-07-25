#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use user_lib::{
    exit, pipe, read, sys_ppoll_time32, thread_create, write, yield_, OldTimespec32, PollFd,
};

const DEFAULT_ROUNDS: usize = 2_000;
const PIPE_PAYLOAD_SIZE: usize = 4 * 1024;
const POLLIN: i16 = 0x001;
const POLL_TIMEOUT_NS: i32 = 100_000_000;

static WRITE_FD: AtomicIsize = AtomicIsize::new(-1);
static START_ROUND: AtomicUsize = AtomicUsize::new(0);
static FINISHED_ROUND: AtomicUsize = AtomicUsize::new(0);
static WRITER_ERROR: AtomicIsize = AtomicIsize::new(0);

fn parse_rounds(value: &str) -> usize {
    let mut parsed = 0usize;
    for byte in value.bytes() {
        if !byte.is_ascii_digit() {
            return DEFAULT_ROUNDS;
        }
        parsed = parsed
            .saturating_mul(10)
            .saturating_add((byte - b'0') as usize);
    }
    parsed.max(1)
}

extern "C" fn writer(rounds: usize) -> isize {
    let fd = WRITE_FD.load(Ordering::Acquire);
    if fd < 0 {
        WRITER_ERROR.store(-1, Ordering::Release);
        exit(1);
    }

    let payload = [0x5au8; PIPE_PAYLOAD_SIZE];
    for round in 1..=rounds {
        while START_ROUND.load(Ordering::Acquire) < round {
            yield_();
        }

        let mut written = 0usize;
        while written < payload.len() {
            let ret = write(fd as usize, &payload[written..]);
            if ret <= 0 {
                WRITER_ERROR.store(ret, Ordering::Release);
                exit(2);
            }
            written += ret as usize;
        }
        FINISHED_ROUND.store(round, Ordering::Release);
    }
    exit(0);
}

#[no_mangle]
fn main(argc: usize, argv: &[&str]) -> i32 {
    let rounds = if argc > 1 {
        parse_rounds(argv[1])
    } else {
        DEFAULT_ROUNDS
    };

    let mut pipe_fds = [-1i32; 2];
    if pipe(&mut pipe_fds) < 0 {
        println!("ppoll_pipe_lost_wakeup: pipe failed");
        return 1;
    }
    WRITE_FD.store(pipe_fds[1] as isize, Ordering::Release);

    let tid = thread_create(writer as usize, rounds);
    if tid <= 0 {
        println!("ppoll_pipe_lost_wakeup: thread_create failed: {}", tid);
        return 2;
    }

    let mut pollfds = [PollFd {
        fd: pipe_fds[0],
        events: POLLIN,
        revents: 0,
    }];
    let timeout = OldTimespec32 {
        tv_sec: 0,
        tv_nsec: POLL_TIMEOUT_NS,
    };
    let mut payload = [0u8; PIPE_PAYLOAD_SIZE];

    for round in 1..=rounds {
        START_ROUND.store(round, Ordering::Release);
        pollfds[0].revents = 0;
        let poll_ret = sys_ppoll_time32(&mut pollfds, Some(&timeout));
        if poll_ret != 1 || (pollfds[0].revents & POLLIN) == 0 {
            println!(
                "ppoll_pipe_lost_wakeup: round={} poll_ret={} revents={:#x} writer_error={}",
                round,
                poll_ret,
                pollfds[0].revents,
                WRITER_ERROR.load(Ordering::Acquire)
            );
            return 3;
        }

        let mut received = 0usize;
        while received < payload.len() {
            let ret = read(pipe_fds[0] as usize, &mut payload[received..]);
            if ret <= 0 {
                println!(
                    "ppoll_pipe_lost_wakeup: round={} read failed: {}",
                    round, ret
                );
                return 4;
            }
            received += ret as usize;
        }

        while FINISHED_ROUND.load(Ordering::Acquire) < round {
            if WRITER_ERROR.load(Ordering::Acquire) != 0 {
                println!(
                    "ppoll_pipe_lost_wakeup: round={} writer failed: {}",
                    round,
                    WRITER_ERROR.load(Ordering::Acquire)
                );
                return 5;
            }
            yield_();
        }
    }

    println!(
        "ppoll_pipe_lost_wakeup: PASS rounds={} bytes={}",
        rounds,
        rounds.saturating_mul(PIPE_PAYLOAD_SIZE)
    );
    0
}
