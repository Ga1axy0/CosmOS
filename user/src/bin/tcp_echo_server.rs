#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

extern crate alloc;

use user_lib::{
    accept, bind, close, condvar_create, condvar_signal, condvar_wait, listen, mutex_blocking_create, mutex_create, mutex_lock, mutex_unlock, net::{AF_INET, SOCK_STREAM, SockAddrIn}, read, socket, thread_create, write, yield_
};

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

const WORKERS: usize = 12;
const QUEUE_SIZE: usize = 32;

struct FdQueue {
    buf: [usize; QUEUE_SIZE],
    head: usize,
    tail: usize,
    len: usize,
}

impl FdQueue {
    const fn new() -> Self {
        Self {
            buf: [0; QUEUE_SIZE],
            head: 0,
            tail: 0,
            len: 0,
        }
    }

    fn is_full(&self) -> bool {
        self.len == QUEUE_SIZE
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, fd: usize) {
        self.buf[self.tail] = fd;
        self.tail = (self.tail + 1) % QUEUE_SIZE;
        self.len += 1;
    }

    fn pop(&mut self) -> usize {
        let fd = self.buf[self.head];
        self.head = (self.head + 1) % QUEUE_SIZE;
        self.len -= 1;
        fd
    }
}

// Protected by a kernel mutex (created at runtime in main).
struct SharedQueue(UnsafeCell<FdQueue>);
// Safety: all accesses to the inner queue are synchronized by `QUEUE_MUTEX_ID`.
unsafe impl Sync for SharedQueue {}

static FD_QUEUE: SharedQueue = SharedQueue(UnsafeCell::new(FdQueue::new()));
// IDs are set before spawning any worker threads.
static mut QUEUE_MUTEX_ID: usize = 0;
static mut QUEUE_CONDVAR_ID: usize = 0;

struct WorkerCtx {
    idx: usize,
    tid: AtomicUsize,
}

fn fmt_peer(peer: &SockAddrIn) -> ([u8; 4], u16) {
    let ip = peer.sin_addr.to_be_bytes();
    let port = u16::from_be(peer.sin_port);
    (ip, port)
}

extern "C" fn worker_entry(arg: usize) -> isize {
    let ctx = unsafe { &*(arg as *const WorkerCtx) };

    // Wait until main thread stores the real kernel tid.
    let tid = loop {
        let t = ctx.tid.load(Ordering::Acquire);
        if t != 0 {
            break t;
        }
        yield_();
    };

    println!("tcp_echo_server: worker#{} started (tid={})", ctx.idx, tid);

    let mut buf = [0u8; 512];
    loop {
        // Dequeue one connection fd.
        unsafe {
            mutex_lock(QUEUE_MUTEX_ID);
            while (&*FD_QUEUE.0.get()).is_empty() {
                // Atomically release mutex and sleep.
                condvar_wait(QUEUE_CONDVAR_ID, QUEUE_MUTEX_ID);
            }
            let fd = (&mut *FD_QUEUE.0.get()).pop();
            mutex_unlock(QUEUE_MUTEX_ID);

            println!("tcp_echo_server: tid={} handling cfd={}", tid, fd);

            // Echo loop.
            loop {
                let n = read(fd, &mut buf);
                if n <= 0 {
                    break;
                }
                let n = n as usize;
                let wn = write(fd, &buf[..n]);
                if wn <= 0 {
                    break;
                }
            }

            let _ = close(fd);
            println!("tcp_echo_server: tid={} finished cfd={}", tid, fd);
        }
    }
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    // A minimal multi-connection echo server (thread pool).
    //
    // Note: with QEMU user networking (slirp), inbound connections from the host
    // typically require explicit port forwarding (hostfwd). Otherwise, you can
    // test by connecting from another guest program.
    let listen_addr = SockAddrIn::from_ipv4_port([0, 0, 0, 0], 7777);

    let fd = socket(AF_INET, SOCK_STREAM, 0);
    if fd < 0 {
        println!("tcp_echo_server: socket() failed");
        return -1;
    }
    let fd = fd as usize;

    if bind(fd, &listen_addr) < 0 {
        println!("tcp_echo_server: bind() failed");
        let _ = close(fd);
        return -1;
    }

    if listen(fd, 1) < 0 {
        println!("tcp_echo_server: listen() failed");
        let _ = close(fd);
        return -1;
    }

    println!("tcp_echo_server: listening on 0.0.0.0:7777");

    // Init shared queue sync primitives.
    let mid = mutex_blocking_create();
    if mid < 0 {
        println!("tcp_echo_server: mutex_create() failed: returned {}", mid);
        let _ = close(fd);
        return -1;
    }
    let cid = condvar_create();
    if cid < 0 {
        println!("tcp_echo_server: condvar_create() failed: returned {}", cid);
        let _ = close(fd);
        return -1;
    }
    unsafe {
        QUEUE_MUTEX_ID = mid as usize;
        QUEUE_CONDVAR_ID = cid as usize;
    }

    // Spawn worker threads.
    for i in 0..WORKERS {
        let ctx = Box::into_raw(Box::new(WorkerCtx {
            idx: i,
            tid: AtomicUsize::new(0),
        }));
        let tid = thread_create(worker_entry as usize, ctx as usize);
        if tid <= 0 {
            println!("tcp_echo_server: thread_create() failed for worker#{}", i);
            // Leak ctx on failure; process is about to exit anyway.
            let _ = close(fd);
            return -1;
        }
        unsafe {
            (*ctx).tid.store(tid as usize, Ordering::Release);
        }
    }

    // Main thread: accept connections and enqueue them.
    loop {
        let mut peer = SockAddrIn::default();
        let cfd = accept(fd, Some(&mut peer));
        if cfd < 0 {
            // Avoid busy loop.
            yield_();
            continue;
        }
        let cfd = cfd as usize;
        let (ip, port) = fmt_peer(&peer);
        println!(
            "tcp_echo_server: accepted cfd={} from {}.{}.{}.{}:{}",
            cfd, ip[0], ip[1], ip[2], ip[3], port
        );

        unsafe {
            mutex_lock(QUEUE_MUTEX_ID);
            if (&*FD_QUEUE.0.get()).is_full() {
                mutex_unlock(QUEUE_MUTEX_ID);
                println!("tcp_echo_server: queue full, dropping cfd={}", cfd);
                let _ = close(cfd);
                continue;
            }
            (&mut *FD_QUEUE.0.get()).push(cfd);
            condvar_signal(QUEUE_CONDVAR_ID);
            mutex_unlock(QUEUE_MUTEX_ID);
        }
    }
}
