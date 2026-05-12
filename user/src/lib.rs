#![no_std]
#![feature(linkage)]
#![feature(panic_info_message)]
#![feature(alloc_error_handler)]

#[macro_use]
pub mod console;
pub mod net;
mod lang_items;
mod syscall;

extern crate alloc;
extern crate core;
#[macro_use]
extern crate bitflags;

use alloc::{string::String, vec::Vec};
use buddy_system_allocator::LockedHeap;
use core::arch::global_asm;
pub use console::{flush, STDIN, STDOUT};
pub use syscall::*;

const USER_HEAP_SIZE: usize = 128 * 1024;
const EAGAIN: isize = -11;

static mut HEAP_SPACE: [u8; USER_HEAP_SIZE] = [0; USER_HEAP_SIZE];

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

#[alloc_error_handler]
pub fn handle_alloc_error(layout: core::alloc::Layout) -> ! {
    panic!("Heap allocation error, layout = {:?}", layout);
}

fn clear_bss() {
    extern "C" {
        fn start_bss();
        fn end_bss();
    }
    unsafe {
        core::slice::from_raw_parts_mut(
            start_bss as usize as *mut u8,
            end_bss as usize - start_bss as usize,
        )
        .fill(0);
    }
}

global_asm!(
    r#"
    .section .text.entry
    .globl _start
_start:
    mv a0, sp
    call __user_start
"#
);

/// 用户程序入口：从 Linux ABI 初始栈解析 argc/argv 后调用 main。
#[no_mangle]
pub extern "C" fn __user_start(user_sp: usize) -> ! {
    clear_bss();
    unsafe {
        HEAP.lock()
            .init(HEAP_SPACE.as_ptr() as usize, USER_HEAP_SIZE);
    }
    let argc = unsafe { (user_sp as *const usize).read_volatile() };
    let argv = user_sp + core::mem::size_of::<usize>();
    let mut v: Vec<&'static str> = Vec::new();
    for i in 0..argc {
        let str_start =
            unsafe { ((argv + i * core::mem::size_of::<usize>()) as *const usize).read_volatile() };
        let len = (0usize..)
            .find(|i| unsafe { ((str_start + *i) as *const u8).read_volatile() == 0 })
            .unwrap();
        v.push(
            core::str::from_utf8(unsafe {
                core::slice::from_raw_parts(str_start as *const u8, len)
            })
            .unwrap(),
        );
    }
    exit(main(argc, v.as_slice()));
}

#[linkage = "weak"]
#[no_mangle]
fn main(_argc: usize, _argv: &[&str]) -> i32 {
    panic!("Cannot find main!");
}

bitflags! {
    pub struct OpenFlags: u32 {
        const RDONLY = 0x000;
        const WRONLY = 0x001;
        const RDWR = 0x002;
        const CREATE = 0x40;
        const TRUNC = 0x200;
        const DIRECTORY = 0x10000;
    }
}

bitflags! {
    /// `mmap` 标志位。
    pub struct MMapFlags: usize {
        /// 共享映射。
        const MAP_SHARED = 0x1;
        /// 私有映射。
        const MAP_PRIVATE = 0x2;
        /// 匿名映射。
        const MAP_ANONYMOUS = 0x20;
    }
}

bitflags! {
    /// `mmap` 保护位。
    pub struct MMapProt: usize {
        /// 可读。
        const PROT_READ = 0x1;
        /// 可写。
        const PROT_WRITE = 0x2;
        /// 可执行。
        const PROT_EXEC = 0x4;
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

impl TimeVal {
    pub fn new() -> Self {
        Self::default()
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Itimerval {
    pub it_interval: TimeVal,
    pub it_value: TimeVal,
}

pub const ITIMER_REAL: i32 = 0;
pub const ITIMER_VIRTUAL: i32 = 1;
pub const ITIMER_PROF: i32 = 2;

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum TaskStatus {
    UnInit,
    Ready,
    Running,
    Exited,
}

#[repr(C)]
pub enum TraceRequest {
    Read,
    Write,
    Syscall,
}

#[repr(C)]
#[derive(Debug)]
pub struct Stat {
    /// ID of device containing file
    pub dev: u64,
    /// inode number
    pub ino: u64,
    /// file type and mode
    pub mode: StatMode,
    /// number of hard links
    pub nlink: u32,
    /// user ID of owner
    pub uid: u32,
    /// group ID of owner
    pub gid: u32,
    /// device ID (if special file)
    pub rdev: u64,
    /// padding to keep C ABI-compatible layout
    pub pad0: usize,
    /// total size, in bytes
    pub size: i64,
    /// preferred block size for I/O
    pub blksize: u32,
    /// padding to keep C ABI-compatible layout
    pub pad1: i32,
    /// number of 512-byte blocks allocated
    pub blocks: u64,
    /// time of last access (seconds)
    pub atime_sec: isize,
    /// time of last access (nanoseconds)
    pub atime_nsec: isize,
    /// time of last modification (seconds)
    pub mtime_sec: isize,
    /// time of last modification (nanoseconds)
    pub mtime_nsec: isize,
    /// time of last status change (seconds)
    pub ctime_sec: isize,
    /// time of last status change (nanoseconds)
    pub ctime_nsec: isize,
    /// reserved fields
    pub unused: [u32; 2],
}

impl Stat {
    pub fn new() -> Self {
        Stat {
            dev: 0,
            ino: 0,
            mode: StatMode::NULL,
            nlink: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
            pad0: 0,
            size: 0,
            blksize: 0,
            pad1: 0,
            blocks: 0,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
            unused: [0; 2],
        }
    }
}

impl Default for Stat {
    fn default() -> Self {
        Self::new()
    }
}

bitflags! {
    pub struct StatMode: u32 {
        const NULL  = 0;
        /// directory
        const DIR   = 0o040000;
        /// ordinary regular file
        const FILE  = 0o100000;
        /// socket file
        const SOCK  = 0o140000;
    }
}

pub const AT_FDCWD: isize = -100;
pub const AT_REMOVEDIR: usize = 0x200;
pub const AT_SYMLINK_NOFOLLOW: usize = 0x100;
pub const AT_EMPTY_PATH: usize = 0x1000;
pub const F_DUPFD: i32 = 0;
pub const F_GETFD: i32 = 1;
pub const F_SETFD: i32 = 2;
pub const F_GETFL: i32 = 3;
pub const F_SETFL: i32 = 4;
pub const F_DUPFD_CLOEXEC: i32 = 1030;
pub const FD_CLOEXEC: i32 = 0x1;

fn to_cstring(s: &str) -> String {
    if s.as_bytes().last() == Some(&0) {
        String::from(s)
    } else {
        let mut t = String::from(s);
        t.push('\0');
        t
    }
}

pub fn open(path: &str, flags: OpenFlags) -> isize {
    let path = to_cstring(path);
    sys_openat(AT_FDCWD as usize, path.as_str(), flags.bits, OpenFlags::RDWR.bits)
}

pub fn close(fd: usize) -> isize {
    if fd == STDOUT {
        console::flush();
    }
    sys_close(fd)
}

pub fn fcntl(fd: usize, cmd: i32, arg: i32) -> isize {
    sys_fcntl(fd, cmd, arg)
}

pub fn read(fd: usize, buf: &mut [u8]) -> isize {
    sys_read(fd, buf)
}

pub fn write(fd: usize, buf: &[u8]) -> isize {
    sys_write(fd, buf)
}

pub fn link(old_path: &str, new_path: &str) -> isize {
    let old_path = to_cstring(old_path);
    let new_path = to_cstring(new_path);
    sys_linkat(
        AT_FDCWD as usize,
        old_path.as_str(),
        AT_FDCWD as usize,
        new_path.as_str(),
        0,
    )
}

pub fn unlink(path: &str) -> isize {
    let path = to_cstring(path);
    sys_unlinkat(AT_FDCWD as usize, path.as_str(), 0)
}

/// 按路径调整常规文件长度。
pub fn truncate(path: &str, len: isize) -> isize {
    let path = to_cstring(path);
    sys_truncate(path.as_str(), len)
}

/// 按文件描述符调整常规文件长度。
pub fn ftruncate(fd: usize, len: isize) -> isize {
    sys_ftruncate(fd, len)
}

pub fn fstat(fd: usize, st: &mut Stat) -> isize {
    sys_fstat(fd, st)
}

/// 按目录 fd 与路径查询文件状态。
pub fn fstatat(dirfd: isize, path: &str, st: &mut Stat, flags: i32) -> isize {
    let path = to_cstring(path);
    sys_newfstatat(dirfd as usize, path.as_str(), st, flags)
}

pub fn mail_read(buf: &mut [u8]) -> isize {
    sys_mail_read(buf)
}

pub fn mail_write(pid: usize, buf: &[u8]) -> isize {
    sys_mail_write(pid, buf)
}

pub fn exit(exit_code: i32) -> ! {
    console::flush();
    sys_exit(exit_code);
}

pub fn yield_() -> isize {
    sys_yield()
}

pub fn get_time() -> isize {
    let mut time = TimeVal::new();
    match sys_get_time(&mut time, 0) {
        0 => ((time.sec & 0xffff) * 1000 + time.usec / 1000) as isize,
        _ => -1,
    }
}

pub fn getitimer(which: i32, value: &mut Itimerval) -> isize {
    sys_getitimer(which, value as *mut _)
}

pub fn setitimer(
    which: i32,
    value: Option<&Itimerval>,
    old_value: Option<&mut Itimerval>,
) -> isize {
    sys_setitimer(
        which,
        value.map_or(core::ptr::null(), |v| v as *const _),
        old_value.map_or(core::ptr::null_mut(), |v| v as *mut _),
    )
}

pub fn getpid() -> isize {
    sys_getpid()
}

pub fn socket(domain: usize, socket_type: usize, protocol: usize) -> isize {
    sys_socket(domain, socket_type, protocol)
}

pub fn socketpair(domain: usize, socket_type: usize, protocol: usize, sv: &mut [i32; 2]) -> isize {
    sys_socketpair(domain, socket_type, protocol, sv.as_mut_ptr())
}

pub fn bind(fd: usize, addr: &net::SockAddrIn) -> isize {
    sys_bind(fd, addr as *const _, core::mem::size_of::<net::SockAddrIn>())
}

pub fn listen(fd: usize, backlog: usize) -> isize {
    sys_listen(fd, backlog)
}

pub fn accept(fd: usize, addr_out: Option<&mut net::SockAddrIn>) -> isize {
    let addr_ptr = addr_out.map_or(core::ptr::null_mut(), |a| a as *mut _);
    sys_accept(fd, addr_ptr, core::mem::size_of::<net::SockAddrIn>())
}

pub fn connect(fd: usize, addr: &net::SockAddrIn) -> isize {
    sys_connect(fd, addr as *const _, core::mem::size_of::<net::SockAddrIn>())
}

pub fn sendto(fd: usize, buf: &[u8], flags: usize, addr: Option<&net::SockAddrIn>) -> isize {
    let addr_ptr = addr.map_or(core::ptr::null(), |a| a as *const _);
    let addrlen = if addr.is_some() {
        core::mem::size_of::<net::SockAddrIn>()
    } else {
        0
    };
    sys_sendto(fd, buf.as_ptr(), buf.len(), flags, addr_ptr, addrlen)
}

pub fn recvfrom(
    fd: usize,
    buf: &mut [u8],
    flags: usize,
    addr_out: Option<&mut net::SockAddrIn>,
) -> isize {
    let (addr_ptr, addrlen) = match addr_out {
        Some(a) => (a as *mut _, core::mem::size_of::<net::SockAddrIn>()),
        None => (core::ptr::null_mut(), 0),
    };
    sys_recvfrom(fd, buf.as_mut_ptr(), buf.len(), flags, addr_ptr, addrlen)
}

pub fn shutdown(fd: usize, how: usize) -> isize {
    sys_shutdown(fd, how)
}

pub fn sendmsg(fd: usize, msg: &net::MsgHdr, flags: usize) -> isize {
    sys_sendmsg(fd, msg as *const _, flags)
}

pub fn recvmsg(fd: usize, msg: &mut net::MsgHdr, flags: usize) -> isize {
    sys_recvmsg(fd, msg as *mut _, flags)
}

pub fn getsockname(fd: usize, addr_out: Option<&mut net::SockAddrIn>) -> isize {
    let addr_ptr = addr_out.map_or(core::ptr::null_mut(), |a| a as *mut _);
    sys_getsockname(fd, addr_ptr, core::mem::size_of::<net::SockAddrIn>())
}

pub fn getpeername(fd: usize, addr_out: Option<&mut net::SockAddrIn>) -> isize {
    let addr_ptr = addr_out.map_or(core::ptr::null_mut(), |a| a as *mut _);
    sys_getpeername(fd, addr_ptr, core::mem::size_of::<net::SockAddrIn>())
}

pub fn fork() -> isize {
    // fork 在 Linux/RISC-V 上由 clone(SIGCHLD, NULL, ...) 表达。
    sys_clone(SIGCHLD as usize, 0, 0, 0, 0)
}

/// 兼容接口：执行程序（不传环境变量），内部等价于 `execve(path, args, [NULL])`。
pub fn exec(path: &str, args: &[*const u8]) -> isize {
    let envp: [*const u8; 1] = [core::ptr::null()];
    sys_execve(path, args, &envp)
}

pub fn execve(path: &str, args: &[*const u8], envp: &[*const u8]) -> isize {
    sys_execve(path, args, envp)
}

pub fn set_priority(prio: isize) -> isize {
    sys_set_priority(prio)
}

pub fn wait(exit_code: &mut i32) -> isize {
    loop {
        match sys_waitpid(-1, exit_code as *mut _) {
            EAGAIN => {
                sys_yield();
            }
            n => {
                return n;
            }
        }
    }
}

pub fn waitpid(pid: usize, exit_code: &mut i32) -> isize {
    loop {
        match sys_waitpid(pid as isize, exit_code as *mut _) {
            EAGAIN => {
                sys_yield();
            }
            n => {
                return n;
            }
        }
    }
}

pub fn sleep_blocking(sleep_ms: usize) {
    sys_sleep(sleep_ms);
}

pub fn sleep(period_ms: usize) {
    let start = get_time();
    while get_time() < start + period_ms as isize {
        sys_yield();
    }
}
pub fn mmap(start: usize, len: usize, prot: usize) -> isize {
    sys_mmap(start, len, prot)
}

/// 完整的 `mmap` 用户态封装，支持文件映射与标志位控制。
pub fn mmap_full(
    start: usize,
    len: usize,
    prot: MMapProt,
    flags: MMapFlags,
    fd: usize,
    offset: usize,
) -> isize {
    sys_mmap_full(start, len, prot.bits(), flags.bits(), fd, offset)
}

pub fn munmap(start: usize, len: usize) -> isize {
    sys_munmap(start, len)
}

pub fn brk(addr: usize) -> isize {
    sys_brk(addr)
}

pub fn sbrk(size: i32) -> isize {
    let old_brk = brk(0);
    if old_brk < 0 {
        return -1;
    }
    let target = if size >= 0 {
        (old_brk as usize).saturating_add(size as usize)
    } else {
        (old_brk as usize).saturating_sub(size.unsigned_abs() as usize)
    };
    let new_brk = brk(target);
    if new_brk == target as isize {
        old_brk
    } else {
        -1
    }
}

pub fn spawn(path: &str) -> isize {
    sys_spawn(path)
}

pub fn dup(fd: usize) -> isize {
    sys_dup(fd)
}
pub fn pipe(pipe_fd: &mut [usize]) -> isize {
    sys_pipe(pipe_fd)
}

pub fn trace(request: TraceRequest, id: usize, data: usize) -> isize {
    sys_trace(request as usize, id, data)
}

pub fn trace_read(addr: *const u8) -> Option<u8> {
    match trace(TraceRequest::Read, addr as usize, 0) {
        -1 => None,
        data => Some(data as u8),
    }
}

pub fn trace_write(addr: *const u8, data: u8) -> isize {
    trace(TraceRequest::Write, addr as usize, data as usize)
}

pub fn count_syscall(id: usize) -> isize {
    trace(TraceRequest::Syscall, id, 0)
}

pub fn thread_create(entry: usize, arg: usize) -> isize {
    sys_thread_create(entry, arg)
}
pub fn gettid() -> isize {
    sys_gettid()
}
pub fn waittid(tid: usize) -> isize {
    loop {
        match sys_waittid(tid) {
            -2 => {
                yield_();
            }
            exit_code => return exit_code,
        }
    }
}

pub fn mutex_create() -> isize {
    sys_mutex_create(false)
}
pub fn mutex_blocking_create() -> isize {
    sys_mutex_create(true)
}
pub fn mutex_lock(mutex_id: usize) -> isize {
    sys_mutex_lock(mutex_id)
}
pub fn mutex_unlock(mutex_id: usize) {
    sys_mutex_unlock(mutex_id);
}
pub fn semaphore_create(res_count: usize) -> isize {
    sys_semaphore_create(res_count)
}
pub fn semaphore_up(sem_id: usize) {
    sys_semaphore_up(sem_id);
}
pub fn enable_deadlock_detect(enabled: bool) -> isize {
    sys_enable_deadlock_detect(enabled as usize)
}
pub fn semaphore_down(sem_id: usize) -> isize {
    sys_semaphore_down(sem_id)
}
pub fn condvar_create() -> isize {
    sys_condvar_create(0)
}
pub fn condvar_signal(condvar_id: usize) {
    sys_condvar_signal(condvar_id);
}
pub fn condvar_wait(condvar_id: usize, mutex_id: usize) {
    sys_condvar_wait(condvar_id, mutex_id);
}

/// Action for a signal
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub struct SignalAction {
    pub handler: usize,
    pub mask: SignalFlags,
}

impl Default for SignalAction {
    fn default() -> Self {
        Self {
            handler: 0,
            mask: SignalFlags::empty(),
        }
    }
}

pub const SIGDEF: i32 = 0; // Default signal handling
pub const SIGHUP: i32 = 1;
pub const SIGINT: i32 = 2;
pub const SIGQUIT: i32 = 3;
pub const SIGILL: i32 = 4;
pub const SIGTRAP: i32 = 5;
pub const SIGABRT: i32 = 6;
pub const SIGBUS: i32 = 7;
pub const SIGFPE: i32 = 8;
pub const SIGKILL: i32 = 9;
pub const SIGUSR1: i32 = 10;
pub const SIGSEGV: i32 = 11;
pub const SIGUSR2: i32 = 12;
pub const SIGPIPE: i32 = 13;
pub const SIGALRM: i32 = 14;
pub const SIGTERM: i32 = 15;
pub const SIGSTKFLT: i32 = 16;
pub const SIGCHLD: i32 = 17;
pub const SIGCONT: i32 = 18;
pub const SIGSTOP: i32 = 19;
pub const SIGTSTP: i32 = 20;
pub const SIGTTIN: i32 = 21;
pub const SIGTTOU: i32 = 22;
pub const SIGURG: i32 = 23;
pub const SIGXCPU: i32 = 24;
pub const SIGXFSZ: i32 = 25;
pub const SIGVTALRM: i32 = 26;
pub const SIGPROF: i32 = 27;
pub const SIGWINCH: i32 = 28;
pub const SIGIO: i32 = 29;
pub const SIGPWR: i32 = 30;
pub const SIGSYS: i32 = 31;

bitflags! {
    pub struct SignalFlags: i32 {
        const SIGDEF = 1; // Default signal handling
        const SIGHUP = 1 << 1;
        const SIGINT = 1 << 2;
        const SIGQUIT = 1 << 3;
        const SIGILL = 1 << 4;
        const SIGTRAP = 1 << 5;
        const SIGABRT = 1 << 6;
        const SIGBUS = 1 << 7;
        const SIGFPE = 1 << 8;
        const SIGKILL = 1 << 9;
        const SIGUSR1 = 1 << 10;
        const SIGSEGV = 1 << 11;
        const SIGUSR2 = 1 << 12;
        const SIGPIPE = 1 << 13;
        const SIGALRM = 1 << 14;
        const SIGTERM = 1 << 15;
        const SIGSTKFLT = 1 << 16;
        const SIGCHLD = 1 << 17;
        const SIGCONT = 1 << 18;
        const SIGSTOP = 1 << 19;
        const SIGTSTP = 1 << 20;
        const SIGTTIN = 1 << 21;
        const SIGTTOU = 1 << 22;
        const SIGURG = 1 << 23;
        const SIGXCPU = 1 << 24;
        const SIGXFSZ = 1 << 25;
        const SIGVTALRM = 1 << 26;
        const SIGPROF = 1 << 27;
        const SIGWINCH = 1 << 28;
        const SIGIO = 1 << 29;
        const SIGPWR = 1 << 30;
        const SIGSYS = 1 << 31;
    }
}

pub fn kill(pid: usize, signum: i32) -> isize {
    sys_kill(pid, signum)
}

pub fn sigaction(
    signum: i32,
    action: Option<&SignalAction>,
    old_action: Option<&mut SignalAction>,
) -> isize {
    sys_sigaction(
        signum,
        action.map_or(core::ptr::null(), |a| a),
        old_action.map_or(core::ptr::null_mut(), |a| a),
    )
}

pub fn sigprocmask(mask: u32) -> isize {
    sys_sigprocmask(mask)
}

pub fn sigreturn() -> isize {
    sys_sigreturn()
}

pub fn getcwd(buffer: &mut [u8]) -> isize {
    sys_getcwd(buffer)
}

pub fn mkdir(path: &str, mode: u32) -> isize {
    sys_mkdirat(AT_FDCWD as usize, to_cstring(path).as_str(), mode)
}

pub fn chdir(path: &str) -> isize {
    sys_chdir(to_cstring(path).as_str())
}

pub fn getdents64(fd: usize, buffer: &mut [u8]) -> isize {
    sys_getdents64(fd, buffer)
}
