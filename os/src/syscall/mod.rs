//! Implementation of syscalls
//!
//! The single entry point to all system calls, [`syscall()`], is called
//! whenever userspace wishes to perform a system call using the `ecall`
//! instruction. In this case, the processor raises an 'Environment call from
//! U-mode' exception, which is handled as one of the cases in
//! [`crate::trap::trap_handler`].
//!
//! For clarity, each single syscall is implemented as its own function, named
//! `sys_` then the name of the syscall. You can find functions like this in
//! submodules, and you should also implement syscalls this way.

/// getcwd syscall
pub const SYSCALL_GETCWD: usize = 17;
/// eventfd2 syscall
pub const SYSCALL_EVENTFD2: usize = 19;
/// epoll_create1 syscall
pub const SYSCALL_EPOLL_CREATE1: usize = 20;
/// dup syscall
pub const SYSCALL_DUP: usize = 23;
/// dup2 syscall
pub const SYSCALL_DUP2: usize = 24;
/// fcntl syscall
pub const SYSCALL_FCNTL: usize = 25;
/// inotify_init1 syscall
pub const SYSCALL_INOTIFY_INIT1: usize = 26;
/// ioctl syscall
pub const SYSCALL_IOCTL: usize = 29;
/// mkdirat syscall
pub const SYSCALL_MKDIRAT: usize = 34;
/// unlinkat syscall
pub const SYSCALL_UNLINKAT: usize = 35;
/// symlinkat syscall
pub const SYSCALL_SYMLINKAT: usize = 36;
/// linkat syscall
pub const SYSCALL_LINKAT: usize = 37;
/// umount syscall
pub const SYSCALL_UMOUNT: usize = 39;
/// mount syscall
pub const SYSCALL_MOUNT: usize = 40;
/// statfs64 syscall
pub const SYSCALL_STATFS64: usize = 43;
/// fstatfs64 syscall
pub const SYSCALL_FSTATFS64: usize = 44;
/// truncate syscall
pub const SYSCALL_TRUNCATE: usize = 45;
/// ftruncate syscall
pub const SYSCALL_FTRUNCATE: usize = 46;
/// fallocate syscall
pub const SYSCALL_FALLOCATE: usize = 47;
/// faccessat syscall
pub const SYSCALL_FACCESSAT: usize = 48;
/// chdir syscall
pub const SYSCALL_CHDIR: usize = 49;
/// fchmod syscall
pub const SYSCALL_FCHMOD: usize = 52;
/// fchmodat syscall
pub const SYSCALL_FCHMODAT: usize = 53;
/// fchownat syscall
pub const SYSCALL_FCHOWNAT: usize = 54;
/// fchown syscall
pub const SYSCALL_FCHOWN: usize = 55;
/// openat syscall
pub const SYSCALL_OPENAT: usize = 56;
/// close syscall
pub const SYSCALL_CLOSE: usize = 57;
/// pipe syscall
pub const SYSCALL_PIPE2: usize = 59;
/// getdents64 syscall
pub const SYSCALL_GETDENTS64: usize = 61;
/// llseek syscall
pub const SYSCALL_LSEEK: usize = 62;
/// read syscall
pub const SYSCALL_READ: usize = 63;
/// write syscall
pub const SYSCALL_WRITE: usize = 64;
/// readv syscall
pub const SYSCALL_READV: usize = 65;
/// writev syscall
pub const SYSCALL_WRITEV: usize = 66;
/// pread64 syscall
pub const SYSCALL_PREAD64: usize = 67;
/// pwrite64 syscall
pub const SYSCALL_PWRITE64: usize = 68;
/// preadv syscall
pub const SYSCALL_PREADV: usize = 69;
/// pwritev syscall
pub const SYSCALL_PWRITEV: usize = 70;
/// sendfile64 syscall
pub const SYSCALL_SENDFILE64: usize = 71;
/// splice syscall
pub const SYSCALL_SPLICE: usize = 76;
/// pselect6_time32 syscall
pub const SYSCALL_PSELECT6_TIME32: usize = 72;
/// ppoll_time32 syscall
pub const SYSCALL_PPOLL_TIME32: usize = 73;
/// signalfd4 syscall
pub const SYSCALL_SIGNALFD4: usize = 74;
/// readlinkat syscall
pub const SYSCALL_READLINKAT: usize = 78;
/// newfstatat syscall
pub const SYSCALL_NEWFSTATAT: usize = 79;
/// timerfd_create syscall
pub const SYSCALL_TIMERFD_CREATE: usize = 85;
/// utimensat syscall
pub const SYSCALL_UTIMENSAT: usize = 88;
/// acct syscall
pub const SYSCALL_ACCT: usize = 89;
/// capget syscall
pub const SYSCALL_CAPGET: usize = 90;
/// capset syscall
pub const SYSCALL_CAPSET: usize = 91;
/// fstat syscall
pub const SYSCALL_FSTAT: usize = 80;
/// sync syscall
pub const SYSCALL_SYNC: usize = 81;
/// fsync syscall
pub const SYSCALL_FSYNC: usize = 82;
/// fdatasync syscall
pub const SYSCALL_FDATASYNC: usize = 83;
/// exit syscall
pub const SYSCALL_EXIT: usize = 93;
/// exit group syscall
pub const SYSCALL_EXIT_GROUP: usize = 94;
/// set tid address syscall
pub const SYSCALL_SET_TID_ADDRESS: usize = 96;
/// unshare syscall
pub const SYSCALL_UNSHARE: usize = 97;
/// futex syscall
pub const SYSCALL_FUTEX: usize = 98;
/// set robust list syscall
pub const SYSCALL_SET_ROBUST_LIST: usize = 99;
/// get robust list syscall
pub const SYSCALL_GET_ROBUST_LIST: usize = 100;
/// sleep syscall
pub const SYSCALL_NANOSLEEP: usize = 101;
/// getitimer syscall
pub const SYSCALL_GETITIMER: usize = 102;
/// setitimer syscall
pub const SYSCALL_SETITIMER: usize = 103;
/// clock_settime syscall
pub const SYSCALL_CLOCK_SETTIME: usize = 112;
/// clock_gettime syscall
pub const SYSCALL_CLOCK_GETTIME: usize = 113;
/// clock_getres syscall
pub const SYSCALL_CLOCK_GETRES: usize = 114;
/// clock_nanosleep syscall
pub const SYSCALL_CLOCK_NANOSLEEP: usize = 115;
/// syslog syscall
pub const SYSCALL_SYSLOG: usize = 116;
/// sched_setscheduler syscall
pub const SYSCALL_SCHED_SETSCHEDULER: usize = 119;
/// sched_getscheduler syscall
pub const SYSCALL_SCHED_GETSCHEDULER: usize = 120;
/// sched_getparam syscall
pub const SYSCALL_SCHED_GETPARAM: usize = 121;
/// sched_setaffinity syscall
pub const SYSCALL_SCHED_SETAFFINITY: usize = 122;
/// sched_getaffinity syscall
pub const SYSCALL_SCHED_GETAFFINITY: usize = 123;
/// yield syscall
pub const SYSCALL_YIELD: usize = 124;
/// kill syscall
pub const SYSCALL_KILL: usize = 129;
/// tkill syscall
pub const SYSCALL_TKILL: usize = 130;
/// tgkill syscall
pub const SYSCALL_TGKILL: usize = 131;
/// sigaltstack syscall
pub const SYSCALL_SIGALTSTACK: usize = 132;
/// sigsuspend syscall
pub const SYSCALL_SIGSUSPEND: usize = 133;
/// sigaction syscall
pub const SYSCALL_SIGACTION: usize = 134;
/// sigprocmask syscall
pub const SYSCALL_SIGPROCMASK: usize = 135;
/// rt_sigtimedwait_time32 syscall
pub const SYSCALL_RT_SIGTIMEDWAIT_TIME32: usize = 137;
/// sigreturn syscall
pub const SYSCALL_SIGRETURN: usize = 139;
/// set priority syscall
pub const SYSCALL_SET_PRIORITY: usize = 140;
/// get priority syscall
pub const SYSCALL_GET_PRIORITY: usize = 141;
/// setregid syscall
pub const SYSCALL_SETREGID: usize = 143;
/// setgid syscall
pub const SYSCALL_SETGID: usize = 144;
/// setreuid syscall
pub const SYSCALL_SETREUID: usize = 145;
/// setuid syscall
pub const SYSCALL_SETUID: usize = 146;
/// setresuid syscall
pub const SYSCALL_SETRESUID: usize = 147;
/// setresgid syscall
pub const SYSCALL_SETRESGID: usize = 149;
/// times syscall
pub const SYSCALL_TIMES: usize = 153;
/// setpgid syscall
pub const SYSCALL_SETPGID: usize = 154;
/// getpgid syscall
pub const SYSCALL_GETPGID: usize = 155;
/// getsid syscall
pub const SYSCALL_GETSID: usize = 156;
/// setsid syscall
pub const SYSCALL_SETSID: usize = 157;
/// uname syscall
pub const SYSCALL_UNAME: usize = 160;
/// getrlimit syscall
pub const SYSCALL_GETRLIMIT: usize = 163;
/// setrlimit syscall
pub const SYSCALL_SETRLIMIT: usize = 164;
/// getrusage syscall
pub const SYSCALL_GETRUSAGE: usize = 165;
/// umask syscall
pub const SYSCALL_UMASK: usize = 166;
/// getcpu
pub const SYSCALL_GETCPU: usize = 168;
/// gettimeofday syscall
pub const SYSCALL_GETTIMEOFDAY: usize = 169;
/// settimeofday syscall
pub const SYSCALL_SETTIMEOFDAY: usize = 170;
/// adjtimex syscall
pub const SYSCALL_ADJTIMEX: usize = 171;
/// getpid syscall
pub const SYSCALL_GETPID: usize = 172;
/// getppid syscall
pub const SYSCALL_GETPPID: usize = 173;
/// getuid syscall
pub const SYSCALL_GETUID: usize = 174;
/// geteuid syscall
pub const SYSCALL_GETEUID: usize = 175;
/// getgid syscall
pub const SYSCALL_GETGID: usize = 176;
/// getegid syscall
pub const SYSCALL_GETEGID: usize = 177;
/// sysinfo syscall
pub const SYSCALL_SYSINFO: usize = 179;
/// gettid syscall
pub const SYSCALL_GETTID: usize = 178;
/// shmget syscall
pub const SYSCALL_SHMGET: usize = 194;
/// shmctl syscall
pub const SYSCALL_SHMCTL: usize = 195;
/// shmat syscall
pub const SYSCALL_SHMAT: usize = 196;
/// shmdt syscall
pub const SYSCALL_SHMDT: usize = 197;
/// socket syscall
pub const SYSCALL_SOCKET: usize = 198;
/// setns syscall
pub const SYSCALL_SETNS: usize = 268;
/// socketpair syscall
pub const SYSCALL_SOCKETPAIR: usize = 199;
/// bind syscall
pub const SYSCALL_BIND: usize = 200;
/// listen syscall
pub const SYSCALL_LISTEN: usize = 201;
/// accept syscall
pub const SYSCALL_ACCEPT: usize = 202;
/// connect syscall
pub const SYSCALL_CONNECT: usize = 203;
/// perf_event_open syscall
pub const SYSCALL_PERF_EVENT_OPEN: usize = 241;
/// accept4 syscall
pub const SYSCALL_ACCEPT4: usize = 242;
/// getsockname syscall
pub const SYSCALL_GETSOCKNAME: usize = 204;
/// getpeername syscall
pub const SYSCALL_GETPEERNAME: usize = 205;
/// sendto syscall
pub const SYSCALL_SENDTO: usize = 206;
/// recvfrom syscall
pub const SYSCALL_RECVFROM: usize = 207;
/// setsockopt syscall
pub const SYSCALL_SETSOCKOPT: usize = 208;
/// getsockopt syscall
pub const SYSCALL_GETSOCKOPT: usize = 209;
/// shutdown syscall
pub const SYSCALL_SHUTDOWN: usize = 210;
/// sendmsg syscall
pub const SYSCALL_SENDMSG: usize = 211;
/// recvmsg syscall
pub const SYSCALL_RECVMSG: usize = 212;
/// brk syscall
pub const SYSCALL_BRK: usize = 214;
/// add_key syscall
pub const SYSCALL_ADD_KEY: usize = 217;
/// keyctl syscall
pub const SYSCALL_KEYCTL: usize = 219;
/// munmap syscall
pub const SYSCALL_MUNMAP: usize = 215;
/// clone syscall
pub const SYSCALL_CLONE: usize = 220;
/// execve syscall
pub const SYSCALL_EXECVE: usize = 221;
/// mmap syscall
pub const SYSCALL_MMAP: usize = 222;
/// fadvise64 syscall
pub const SYSCALL_FADVISE64: usize = 223;
/// mprotect syscall
pub const SYSCALL_MPROTECT: usize = 226;
/// msync syscall
pub const SYSCALL_MSYNC: usize = 227;
/// mlock syscall
pub const SYSCALL_MLOCK: usize = 228;
/// munlock syscall
pub const SYSCALL_MUNLOCK: usize = 229;
/// mlockall syscall
pub const SYSCALL_MLOCKALL: usize = 230;
/// munlockall syscall
pub const SYSCALL_MUNLOCKALL: usize = 231;
/// madvise syscall
pub const SYSCALL_MADVISE: usize = 233;
/// get_mempolicy syscall
pub const SYSCALL_GET_MEMPOLICY: usize = 236;
/// waitpid syscall
pub const SYSCALL_WAIT4: usize = 260;
/// prlimit64 syscall
pub const SYSCALL_PRLIMIT64: usize = 261;
/// fanotify_init syscall
pub const SYSCALL_FANOTIFY_INIT: usize = 262;
/// syncfs syscall
pub const SYSCALL_SYNCFS: usize = 267;
/// clock_adjtime syscall
pub const SYSCALL_CLOCK_ADJTIME: usize = 266;
/// sched_setattr syscall
pub const SYSCALL_SCHED_SETATTR: usize = 274;
/// sched_getattr syscall
pub const SYSCALL_SCHED_GETATTR: usize = 275;
/// renameat2 syscall
pub const SYSCALL_RENAMEAT2: usize = 276;
/// getrandom syscall
pub const SYSCALL_GETRANDOM: usize = 278;
/// memfd_create syscall
pub const SYSCALL_MEMFD_CREATE: usize = 279;
/// bpf syscall
pub const SYSCALL_BPF: usize = 280;
/// userfaultfd syscall
pub const SYSCALL_USERFAULTFD: usize = 282;
/// statx syscall
pub const SYSCALL_STATX: usize = 291;
/// spawn syscall
pub const SYSCALL_SPAWN: usize = 400;
/// clock_adjtime64 syscall
pub const SYSCALL_CLOCK_ADJTIME64: usize = 405;
/// io_uring_setup syscall
pub const SYSCALL_IO_URING_SETUP: usize = 425;
/// open_tree syscall
pub const SYSCALL_OPEN_TREE: usize = 428;
/// fsopen syscall
pub const SYSCALL_FSOPEN: usize = 430;
/// fspick syscall
pub const SYSCALL_FSPICK: usize = 433;
/// pidfd_open syscall
pub const SYSCALL_PIDFD_OPEN: usize = 434;
/// faccessat2 syscall
pub const SYSCALL_FACCESSAT2: usize = 439;
/// memfd_secret syscall
pub const SYSCALL_MEMFD_SECRET: usize = 447;
/*
/// mail read syscall
pub const SYSCALL_MAIL_READ: usize = 401;
/// mail write syscall
pub const SYSCALL_MAIL_WRITE: usize = 402;
*/

/// thread_create syscall
pub const SYSCALL_THREAD_CREATE: usize = 460;
/// waittid syscall
pub const SYSCALL_WAITTID: usize = 462;
/// mutex_create syscall
pub const SYSCALL_MUTEX_CREATE: usize = 463;
/// mutex_lock syscall
pub const SYSCALL_MUTEX_LOCK: usize = 464;
/// mutex_unlock syscall
pub const SYSCALL_MUTEX_UNLOCK: usize = 466;
/// semaphore_create syscall
pub const SYSCALL_SEMAPHORE_CREATE: usize = 467;
/// semaphore_up syscall
pub const SYSCALL_SEMAPHORE_UP: usize = 468;
/// enable deadlock detect syscall
pub const SYSCALL_ENABLE_DEADLOCK_DETECT: usize = 469;
/// semaphore_down syscall
pub const SYSCALL_SEMAPHORE_DOWN: usize = 470;
/// condvar_create syscall
pub const SYSCALL_CONDVAR_CREATE: usize = 471;
/// condvar_signal syscall
pub const SYSCALL_CONDVAR_SIGNAL: usize = 472;
/// condvar_wait syscallca
pub const SYSCALL_CONDVAR_WAIT: usize = 473;

/// Conservative whitelist of syscalls that may be transparently restarted
/// after a caught signal when the handler uses SA_RESTART.
///
/// Keep timeout-/signal-waiting syscalls out of this list; they must surface
/// EINTR to userspace to preserve Linux semantics for pause/sigsuspend/ppoll.
pub fn syscall_supports_sa_restart(syscall_id: usize) -> bool {
    matches!(
        syscall_id,
        SYSCALL_READ
            | SYSCALL_WRITE
            | SYSCALL_READV
            | SYSCALL_WRITEV
            | SYSCALL_WAIT4
            | SYSCALL_ACCEPT
            | SYSCALL_ACCEPT4
            | SYSCALL_RECVFROM
            | SYSCALL_RECVMSG
            | SYSCALL_SENDTO
            | SYSCALL_SENDMSG
    )
}

mod fs;
mod key;
mod net;
mod sched;
mod process;
mod sync;
mod thread;
mod random;
mod mman;
mod signal;
mod times;
mod utils;
mod resource;

/// Standard error numbers and conversion traits
pub mod errno;

use fs::*;
use key::*;
use net::*;
use sched::*;
use process::*;
use sync::*;
use thread::*;
use crate::syscall::random::*;
use mman::*;
use signal::*;
use times::*;
use resource::*;
pub(crate) use resource::{rlimit, ResourceLimits};
pub(crate) use utils::{
    read_bytes_from_user, read_cstring_from_user, read_pod_from_process_user, read_pod_from_user,
    translated_byte_buffer_with_access, write_bytes_to_user,
    write_pod_to_process_user, write_pod_to_user, Pod,
};
pub(crate) use fs::{bpf_prog_is_socket_filter, bpf_run_socket_filter_prog, write_process_accounting_on_exit};
pub use times::Timespec;


use crate::{fs::Stat, net::SockAddrIn, syscall::errno::ERRNO};
use crate::klog::*;

/// Execute a syscall body that returns `Result<isize, ERRNO>`, automatically
/// converting `Err(e)` into `-(e as isize)`.  Use with the `?` operator and
/// `OrErrno` to propagate errors cleanly:
///
/// ```rust
/// syscall_body!({
///     let path = translated_str(token, ptr).or_errno(ERRNO::EFAULT)?;
///     Ok(0)
/// })
/// ```
#[macro_export]
macro_rules! syscall_body {
    ($body:block) => {{
        let result: Result<isize, ERRNO> = (|| -> Result<isize, ERRNO> { $body })();
        match result {
            Ok(v) => v,
            Err(e) => -(e as isize),
        }
    }};
}

fn errno_name(errno: isize) -> &'static str {
    match errno as i32 {
        x if x == ERRNO::EPERM as i32 => "EPERM",
        x if x == ERRNO::ENOENT as i32 => "ENOENT",
        x if x == ERRNO::ESRCH as i32 => "ESRCH",
        x if x == ERRNO::EINTR as i32 => "EINTR",
        x if x == ERRNO::EIO as i32 => "EIO",
        x if x == ERRNO::ENXIO as i32 => "ENXIO",
        x if x == ERRNO::E2BIG as i32 => "E2BIG",
        x if x == ERRNO::ENOEXEC as i32 => "ENOEXEC",
        x if x == ERRNO::EBADF as i32 => "EBADF",
        x if x == ERRNO::ECHILD as i32 => "ECHILD",
        x if x == ERRNO::EAGAIN as i32 => "EAGAIN",
        x if x == ERRNO::ENOMEM as i32 => "ENOMEM",
        x if x == ERRNO::EACCES as i32 => "EACCES",
        x if x == ERRNO::EFAULT as i32 => "EFAULT",
        x if x == ERRNO::EBUSY as i32 => "EBUSY",
        x if x == ERRNO::EEXIST as i32 => "EEXIST",
        x if x == ERRNO::ENODEV as i32 => "ENODEV",
        x if x == ERRNO::ENOTDIR as i32 => "ENOTDIR",
        x if x == ERRNO::EISDIR as i32 => "EISDIR",
        x if x == ERRNO::EINVAL as i32 => "EINVAL",
        x if x == ERRNO::EMFILE as i32 => "EMFILE",
        x if x == ERRNO::ENOTTY as i32 => "ENOTTY",
        x if x == ERRNO::EPIPE as i32 => "EPIPE",
        x if x == ERRNO::ENAMETOOLONG as i32 => "ENAMETOOLONG",
        x if x == ERRNO::ENOSYS as i32 => "ENOSYS",
        x if x == ERRNO::ENOTEMPTY as i32 => "ENOTEMPTY",
        x if x == ERRNO::ELOOP as i32 => "ELOOP",
        x if x == ERRNO::EOVERFLOW as i32 => "EOVERFLOW",
        x if x == ERRNO::ENOTSOCK as i32 => "ENOTSOCK",
        x if x == ERRNO::EOPNOTSUPP as i32 => "EOPNOTSUPP",
        x if x == ERRNO::EADDRINUSE as i32 => "EADDRINUSE",
        x if x == ERRNO::EADDRNOTAVAIL as i32 => "EADDRNOTAVAIL",
        x if x == ERRNO::ECONNRESET as i32 => "ECONNRESET",
        x if x == ERRNO::ENOTCONN as i32 => "ENOTCONN",
        x if x == ERRNO::ETIMEDOUT as i32 => "ETIMEDOUT",
        x if x == ERRNO::ECONNREFUSED as i32 => "ECONNREFUSED",
        x if x == ERRNO::ECANCELED as i32 => "ECANCELED",
        _ => "UNKNOWN",
    }
}

/// 系统调用分发入口：根据 `syscall_id` 将参数路由到具体 `sys_*` 实现。
pub fn syscall(syscall_id: usize, args: [usize; 6]) -> isize {
    let result = match syscall_id {
        SYSCALL_EVENTFD2 => sys_eventfd2(args[0] as u32, args[1] as i32),
        SYSCALL_EPOLL_CREATE1 => sys_epoll_create1(args[0] as i32),
        SYSCALL_DUP => sys_dup(args[0] as u32),
        SYSCALL_DUP2 => sys_dup2(args[0] as u32, args[1] as u32),
        SYSCALL_FCNTL => sys_fcntl(args[0] as u32, args[1] as i32, args[2]),
        SYSCALL_INOTIFY_INIT1 => sys_inotify_init1(args[0] as i32),
        SYSCALL_IOCTL => sys_ioctl(args[0] as u32, args[1], args[2]),
        SYSCALL_UNLINKAT => sys_unlinkat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_SYMLINKAT => sys_symlinkat(
            args[0] as *const u8,
            args[1] as isize,
            args[2] as *const u8,
        ),
        SYSCALL_LINKAT => sys_linkat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4] as u32,
        ),
        SYSCALL_UMOUNT => sys_umount(args[0] as *const u8, args[1]),
        SYSCALL_MOUNT => sys_mount(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
            args[4] as *const u8,
        ),
        SYSCALL_STATFS64 => sys_statfs64(args[0] as *const u8, args[1] as *mut u8),
        SYSCALL_FSTATFS64 => sys_fstatfs64(args[0] as u32, args[1] as *mut u8),
        SYSCALL_TRUNCATE => sys_truncate(args[0] as *const u8, args[1] as isize),
        SYSCALL_FTRUNCATE => sys_ftruncate(args[0] as u32, args[1] as isize),
        SYSCALL_FALLOCATE => sys_fallocate(args[0] as u32, args[1] as i32, args[2] as i64, args[3] as i64),
        SYSCALL_FACCESSAT => sys_faccessat(args[0] as isize, args[1] as *const u8, args[2] as i32),
        SYSCALL_FACCESSAT2 => {
            sys_faccessat2(args[0] as isize, args[1] as *const u8, args[2] as i32, args[3] as i32)
        }
        SYSCALL_FCHMOD => sys_fchmod(args[0] as u32, args[1] as u32),
        SYSCALL_FCHMODAT => sys_fchmodat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_FCHOWNAT => sys_fchownat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as u32,
            args[3] as u32,
            args[4] as i32,
        ),
        SYSCALL_FCHOWN => sys_fchown(args[0] as u32, args[1] as u32, args[2] as u32),
        SYSCALL_OPENAT => sys_open(args[0] as isize, args[1] as *const u8, args[2] as i32, args[3] as u32),
        SYSCALL_CLOSE => sys_close(args[0] as u32),
        SYSCALL_PIPE2 => sys_pipe2(args[0] as *mut i32, args[1] as i32),
        SYSCALL_LSEEK => sys_lseek(args[0] as u32, args[1], args[2] as u32),
        SYSCALL_READ => sys_read(args[0] as u32, args[1] as *const u8, args[2]),
        SYSCALL_WRITE => sys_write(args[0] as u32, args[1] as *const u8, args[2]),
        SYSCALL_READV => sys_readv(args[0], args[1] as *const crate::syscall::fs::IoVec, args[2] as i32),
        SYSCALL_WRITEV =>
            sys_writev(args[0], args[1] as *const crate::syscall::fs::IoVec, args[2] as i32),
        SYSCALL_PREAD64 =>
            sys_pread64(args[0] as u32, args[1] as *const u8, args[2], args[3] as i64),
        SYSCALL_PWRITE64 =>
            sys_pwrite64(args[0] as u32, args[1] as *const u8, args[2], args[3] as i64),
        SYSCALL_PREADV => sys_preadv(
            args[0],
            args[1] as *const crate::syscall::fs::IoVec,
            args[2] as i32,
            args[3],
            args[4],
        ),
        SYSCALL_PWRITEV => sys_pwritev(
            args[0],
            args[1] as *const crate::syscall::fs::IoVec,
            args[2] as i32,
            args[3],
            args[4],
        ),
        SYSCALL_SENDFILE64 =>
            sys_sendfile64(args[0] as i32, args[1] as i32, args[2] as *mut i64, args[3]),
        SYSCALL_SPLICE => sys_splice(
            args[0] as i32,
            args[1] as *mut i64,
            args[2] as i32,
            args[3] as *mut i64,
            args[4],
            args[5] as u32,
        ),
        SYSCALL_FADVISE64 => sys_fadvise64(args[0] as i32, args[1] as i64, args[2], args[3] as i32),
        SYSCALL_READLINKAT => sys_readlinkat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
        ),
        SYSCALL_NEWFSTATAT => sys_newfstatat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut Stat,
            args[3] as i32,
        ),
        SYSCALL_STATX => sys_statx(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as i32,
            args[3] as u32,
            args[4] as *mut crate::syscall::fs::Statx,
        ),
        SYSCALL_UTIMENSAT => sys_utimensat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *const Timespec,
            args[3] as i32,
        ),
        SYSCALL_ACCT => sys_acct(args[0] as *const u8),
        SYSCALL_PSELECT6_TIME32 => sys_pselect6_time32(
            args[0] as i32,
            args[1] as *mut usize,
            args[2] as *mut usize,
            args[3] as *mut usize,
            args[4] as *const OldTimespec32,
            args[5] as *const u8,
        ),
        SYSCALL_PPOLL_TIME32 => sys_ppoll_time32(
            args[0] as *mut PollFd,
            args[1] as u32,
            args[2] as *const OldTimespec32,
            args[3] as *const u8,
            args[4],
        ),
        SYSCALL_SIGNALFD4 => sys_signalfd4(
            args[0] as i32,
            args[1] as *const u8,
            args[2],
            args[3] as i32,
        ),
        SYSCALL_FSTAT => sys_fstat(args[0] as u32, args[1] as *mut Stat),
        SYSCALL_TIMERFD_CREATE => sys_timerfd_create(args[0] as i32, args[1] as i32),
        SYSCALL_GETRANDOM => sys_getrandom(args[0] as *mut u8, args[1], args[2]),
        SYSCALL_GETCWD => sys_getcwd(args[0] as *mut u8, args[1]),
        SYSCALL_MKDIRAT => sys_mkdirat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_CHDIR => sys_chdir(args[0] as *const u8),
        SYSCALL_GETDENTS64 => sys_getdents64(args[0] as u32, args[1] as *mut u8, args[2]),
        SYSCALL_SYNC => sys_sync(),
        SYSCALL_FSYNC => sys_fsync(args[0] as u32),
        SYSCALL_FDATASYNC => sys_fdatasync(args[0] as u32),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_EXIT_GROUP => sys_exit_group(args[0] as i32),
        SYSCALL_SET_TID_ADDRESS => sys_set_tid_address(args[0] as *mut i32),
        SYSCALL_FUTEX => sys_futex(
            args[0] as *const i32,
            args[1] as i32,
            args[2] as i32,
            args[3],
            args[4],
            args[5] as i32,
        ),
        SYSCALL_SET_ROBUST_LIST => sys_set_robust_list(args[0], args[1]),
        SYSCALL_GET_ROBUST_LIST => sys_get_robust_list(args[0] as i32, args[1] as *mut usize, args[2] as *mut usize),
        SYSCALL_NANOSLEEP => sys_nanosleep(args[0] as *const Timespec, args[1] as *mut Timespec),
        SYSCALL_GETITIMER => sys_getitimer(args[0] as i32, args[1] as *mut OldItimerval),
        SYSCALL_SETITIMER => sys_setitimer(
            args[0] as i32,
            args[1] as *const OldItimerval,
            args[2] as *mut OldItimerval,
        ),
        SYSCALL_CLOCK_SETTIME => sys_clock_settime(args[0] as ClockId, args[1] as *const Timespec),
        SYSCALL_CLOCK_GETTIME => sys_clock_gettime(args[0] as ClockId, args[1] as *mut Timespec),
        SYSCALL_CLOCK_GETRES => sys_clock_getres(args[0] as ClockId, args[1] as *mut Timespec),
        SYSCALL_CLOCK_NANOSLEEP => sys_clock_nanosleep(
            args[0] as ClockId,
            args[1] as i32,
            args[2] as *const Timespec,
            args[3] as *mut Timespec,
        ),
        SYSCALL_SYSLOG => sys_syslog(args[0] as usize, args[1] as *mut u8, args[2] as usize),
        SYSCALL_YIELD => sys_yield(),
        SYSCALL_GETPGID => sys_getpgid(args[0] as isize),
        SYSCALL_SETPGID => sys_setpgid(args[0] as isize, args[1] as isize),
        SYSCALL_GETSID => sys_getsid(),
        SYSCALL_SETSID => sys_setsid(),
        SYSCALL_UNAME => sys_uname(args[0] as *mut UtsName),
        SYSCALL_GETRUSAGE => sys_getrusage(args[0] as i32, args[1] as *mut RUsage),
        SYSCALL_GETRLIMIT => sys_getrlimit(args[0], args[1] as *mut rlimit),
        SYSCALL_SETRLIMIT => sys_setrlimit(args[0], args[1] as *const rlimit),
        SYSCALL_UMASK => sys_umask(args[0] as i32),
        SYSCALL_PRLIMIT64 => sys_prlimit64(args[0] as i32, args[1], args[2] as *const rlimit, args[3] as *mut rlimit),
        SYSCALL_CLOCK_ADJTIME | SYSCALL_CLOCK_ADJTIME64 =>
            sys_clock_adjtime(args[0] as ClockId, args[1] as *mut Timex),
        SYSCALL_SYNCFS => sys_syncfs(args[0] as u32),
        SYSCALL_GETCPU => sys_getcpu(args[0] as *mut u32, args[1] as *mut u32),
        SYSCALL_GETPID => sys_getpid(),
        SYSCALL_SOCKET => sys_socket(args[0] as i32, args[1] as i32, args[2] as i32),
        SYSCALL_SOCKETPAIR =>
            sys_socketpair(args[0] as i32, args[1] as i32, args[2] as i32, args[3] as *mut i32),
        SYSCALL_BIND => sys_bind(args[0] as i32, args[1] as *const SockAddrIn, args[2] as i32),
        SYSCALL_LISTEN => sys_listen(args[0] as i32, args[1] as i32),
        SYSCALL_ACCEPT => sys_accept(
            args[0] as i32,
            args[1] as *mut SockAddrIn,
            args[2] as *mut i32,
        ),
        SYSCALL_ACCEPT4 => sys_accept4(
            args[0] as i32,
            args[1] as *mut SockAddrIn,
            args[2] as *mut i32,
            args[3] as i32,
        ),
        SYSCALL_PERF_EVENT_OPEN => sys_perf_event_open(
            args[0],
            args[1] as isize,
            args[2] as isize,
            args[3] as isize,
            args[4] as u32,
        ),
        SYSCALL_CONNECT => sys_connect(args[0] as i32, args[1] as *const SockAddrIn, args[2] as i32),
        SYSCALL_GETSOCKNAME => sys_getsockname(args[0] as i32, args[1] as *mut SockAddrIn, args[2] as *mut i32),
        SYSCALL_GETPEERNAME => sys_getpeername(args[0] as i32, args[1] as *mut SockAddrIn, args[2] as *mut i32),
        SYSCALL_SENDTO => sys_sendto(
            args[0] as i32,
            args[1] as *const u8,
            args[2],
            args[3] as u32,
            args[4] as *const SockAddrIn,
            args[5] as i32,
        ),
        SYSCALL_RECVFROM => sys_recvfrom(
            args[0] as i32,
            args[1] as *mut u8,
            args[2],
            args[3] as u32,
            args[4] as *mut SockAddrIn,
            args[5] as *mut i32,
        ),
        SYSCALL_SETSOCKOPT => sys_setsockopt(
            args[0] as i32,
            args[1] as i32,
            args[2] as i32,
            args[3] as *const u8,
            args[4] as i32,
        ),
        SYSCALL_GETSOCKOPT => sys_getsockopt(
            args[0] as i32,
            args[1] as i32,
            args[2] as i32,
            args[3] as *mut u8,
            args[4] as *mut i32,
        ),
        SYSCALL_SHUTDOWN => sys_shutdown(args[0] as i32, args[1] as i32),
        SYSCALL_MADVISE => sys_madvise(args[0], args[1], args[2] as i32),
        SYSCALL_ADD_KEY => sys_add_key(
            args[0] as *const u8,
            args[1] as *const u8,
            args[2] as *const u8,
            args[3],
            args[4] as i32,
        ),
        SYSCALL_KEYCTL => sys_keyctl(
            args[0] as i32,
            args[1],
            args[2],
            args[3],
            args[4],
        ),
        SYSCALL_SENDMSG => sys_sendmsg(args[0] as i32, args[1] as *const MsgHdr, args[2] as u32),
        SYSCALL_RECVMSG => sys_recvmsg(args[0] as i32, args[1] as *mut MsgHdr, args[2] as u32),
        SYSCALL_GETPPID => sys_getppid(),
        SYSCALL_SETREGID => sys_setregid(args[0] as u32, args[1] as u32),
        SYSCALL_SETGID => sys_setgid(args[0] as u32),
        SYSCALL_SETREUID => sys_setreuid(args[0] as u32, args[1] as u32),
        SYSCALL_SETUID => sys_setuid(args[0] as u32),
        SYSCALL_SETRESUID => sys_setresuid(args[0] as u32, args[1] as u32, args[2] as u32),
        SYSCALL_SETRESGID => sys_setresgid(args[0] as u32, args[1] as u32, args[2] as u32),
        SYSCALL_GETUID => sys_getuid(),
        SYSCALL_GETEUID => sys_geteuid(),
        SYSCALL_GETGID => sys_getgid(),
        SYSCALL_GETEGID => sys_getegid(),
        SYSCALL_CAPGET => sys_capget(args[0] as *mut UserCapHeader, args[1] as *mut UserCapData),
        SYSCALL_CAPSET => sys_capset(args[0] as *const UserCapHeader, args[1] as *const UserCapData),
        SYSCALL_SYSINFO => sys_sysinfo(args[0] as *mut SysInfo),
        SYSCALL_GETTID => sys_gettid(),
        SYSCALL_SHMGET => sys_shmget(args[0] as i32, args[1], args[2] as i32),
        SYSCALL_SHMCTL => sys_shmctl(args[0], args[1] as i32, args[2]),
        SYSCALL_SHMAT => sys_shmat(args[0], args[1], args[2] as i32),
        SYSCALL_SHMDT => sys_shmdt(args[0]),
        SYSCALL_CLONE => sys_clone(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_UNSHARE => sys_unshare(args[0]),
        SYSCALL_SETNS => sys_setns(args[0] as i32, args[1] as i32),
        SYSCALL_EXECVE => sys_execve(
            args[0] as *const u8,
            args[1] as *const usize,
            args[2] as *const usize,
        ),
        SYSCALL_WAIT4 => sys_wait4(args[0] as isize, args[1] as *mut i32, args[2] as isize),
        SYSCALL_FANOTIFY_INIT => sys_fanotify_init(args[0] as u32, args[1] as u32),
        SYSCALL_GETTIMEOFDAY => sys_get_time_of_day(args[0] as *mut TimeVal, args[1]),
        SYSCALL_SETTIMEOFDAY => sys_set_time_of_day(args[0] as *const TimeVal, args[1]),
        SYSCALL_ADJTIMEX => sys_adjtimex(args[0] as *mut Timex),
        SYSCALL_TIMES => sys_times(args[0] as *mut Tms),
        SYSCALL_BRK => sys_brk(args[0]),
        SYSCALL_MMAP => sys_mmap(args[0], args[1], args[2], args[3], args[4], args[5]),
        SYSCALL_MPROTECT => sys_mprotect(args[0], args[1], args[2]),
        SYSCALL_MSYNC => sys_msync(args[0], args[1], args[2] as i32),
        SYSCALL_MLOCK => sys_mlock(args[0], args[1]),
        SYSCALL_MUNLOCK => sys_munlock(args[0], args[1]),
        SYSCALL_MLOCKALL => sys_mlockall(args[0] as i32),
        SYSCALL_MUNLOCKALL => sys_munlockall(),
        SYSCALL_GET_MEMPOLICY => sys_get_mempolicy(
            args[0] as *mut i32,
            args[1] as *mut u8,
            args[2],
            args[3],
            args[4] as u32,
        ),
        SYSCALL_MUNMAP => sys_munmap(args[0], args[1]),
        SYSCALL_SCHED_GETPARAM => sys_sched_getparam(args[0] as isize, args[1] as *mut SchedParam),
        SYSCALL_SCHED_SETSCHEDULER => {
            sys_sched_setscheduler(args[0] as isize, args[1] as i32, args[2] as *const SchedParam)
        }
        SYSCALL_SCHED_GETSCHEDULER => sys_sched_getscheduler(args[0] as isize),
        SYSCALL_SCHED_SETATTR => {
            sys_sched_setattr(args[0] as isize, args[1] as *const LinuxSchedAttr, args[2] as u32)
        }
        SYSCALL_SCHED_GETATTR => sys_sched_getattr(
            args[0] as isize,
            args[1] as *mut LinuxSchedAttr,
            args[2] as u32,
            args[3] as u32,
        ),
        SYSCALL_SCHED_SETAFFINITY => {
            sys_sched_setaffinity(args[0] as isize, args[1], args[2] as *const u8)
        }
        SYSCALL_SCHED_GETAFFINITY => {
            sys_sched_getaffinity(args[0] as isize, args[1], args[2] as *mut u8)
        }
        SYSCALL_SET_PRIORITY => sys_setpriority(args[0] as i32, args[1], args[2] as i32),
        SYSCALL_GET_PRIORITY => sys_getpriority(args[0] as i32, args[1]),
        SYSCALL_RENAMEAT2 => sys_renameat2(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4] as u32,
        ),
        SYSCALL_MEMFD_CREATE => sys_memfd_create(args[0] as *const u8, args[1] as u32),
        SYSCALL_BPF => sys_bpf(args[0] as u32, args[1], args[2] as u32),
        SYSCALL_USERFAULTFD => sys_userfaultfd(args[0] as i32),
        SYSCALL_SIGALTSTACK => {
            sys_sigaltstack(args[0] as *const SigAltStack, args[1] as *mut SigAltStack)
        }
        SYSCALL_SIGACTION => sys_sigaction(
            args[0] as i32,
            args[1] as *const UserSigAction,
            args[2] as *mut UserSigAction,
            args[3], // sigsetsize
        ),
        SYSCALL_SIGPROCMASK => sys_sigprocmask(args[0] as i32, args[1] as *const u64, args[2] as *mut u64, args[3]),
        SYSCALL_SIGSUSPEND => sys_sigsuspend(args[0] as *const u64, args[1]),
        SYSCALL_RT_SIGTIMEDWAIT_TIME32 => sys_rt_sigtimedwait_time32(
            args[0] as *const u64,
            args[1] as *mut crate::task::SigInfo,
            args[2] as *const OldTimespec32,
            args[3],
        ),
        SYSCALL_SIGRETURN => sys_sigreturn(),
        SYSCALL_IO_URING_SETUP => sys_io_uring_setup(args[0] as u32, args[1]),
        SYSCALL_OPEN_TREE => sys_open_tree(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_FSOPEN => sys_fsopen(args[0] as *const u8, args[1] as u32),
        SYSCALL_FSPICK => sys_fspick(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_PIDFD_OPEN => sys_pidfd_open(args[0] as isize, args[1] as u32),
        SYSCALL_SPAWN => sys_spawn(args[0] as *const u8),
        SYSCALL_MEMFD_SECRET => sys_memfd_secret(args[0] as u32),
        // Some LTP RISC-V builds fall back to `__LTP__NR_INVALID_SYSCALL` (-1)
        // when the userspace headers do not provide `__NR_memfd_secret`.
        x if x == usize::MAX => sys_memfd_secret(args[0] as u32),
        SYSCALL_THREAD_CREATE => sys_thread_create(args[0], args[1]),
        SYSCALL_WAITTID => sys_waittid(args[0]) as isize,
        SYSCALL_MUTEX_CREATE => sys_mutex_create(args[0] == 1),
        SYSCALL_MUTEX_LOCK => sys_mutex_lock(args[0]),
        SYSCALL_MUTEX_UNLOCK => sys_mutex_unlock(args[0]),
        SYSCALL_SEMAPHORE_CREATE => sys_semaphore_create(args[0]),
        SYSCALL_SEMAPHORE_UP => sys_semaphore_up(args[0]),
        SYSCALL_ENABLE_DEADLOCK_DETECT => sys_enable_deadlock_detect(args[0]),
        SYSCALL_SEMAPHORE_DOWN => sys_semaphore_down(args[0]),
        SYSCALL_CONDVAR_CREATE => sys_condvar_create(),
        SYSCALL_CONDVAR_SIGNAL => sys_condvar_signal(args[0]),
        SYSCALL_CONDVAR_WAIT => sys_condvar_wait(args[0], args[1]),
        SYSCALL_KILL => sys_kill(args[0] as isize, args[1] as u32),
        SYSCALL_TKILL => sys_tkill(args[0], args[1] as u32),
        SYSCALL_TGKILL => sys_tgkill(args[0], args[1], args[2] as u32),
        _ => sys_nisyscall(syscall_id, args),
    };
    if (-4095..0).contains(&result) {
        let errno = -result;
        warn!(
            "syscall error: id={} errno={}({}) result={} args=[{:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:#x}]",
            syscall_id,
            errno,
            errno_name(errno),
            result,
            args[0],
            args[1],
            args[2],
            args[3],
            args[4],
            args[5],
        );
    }
    result
}

/// Syscalls that are invalid or not implemented yet
fn sys_nisyscall(syscall_id: usize, args: [usize; 6]) -> isize {
    syscall_body!({  
        error!("unknown syscall: id = {}, args = {:?}", syscall_id, args);
        Err(ERRNO::ENOSYS)
    })
}
