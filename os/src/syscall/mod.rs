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
/// dup syscall
pub const SYSCALL_DUP: usize = 23;
/// dup2 syscall
pub const SYSCALL_DUP2: usize = 24;
/// fcntl syscall
pub const SYSCALL_FCNTL: usize = 25;
/// ioctl syscall
pub const SYSCALL_IOCTL: usize = 29;
/// mkdirat syscall
pub const SYSCALL_MKDIRAT: usize = 34;
/// unlinkat syscall
pub const SYSCALL_UNLINKAT: usize = 35;
/// linkat syscall
pub const SYSCALL_LINKAT: usize = 37;
/// umount syscall
pub const SYSCALL_UMOUNT: usize = 39;
/// mount syscall
pub const SYSCALL_MOUNT: usize = 40;
/// faccessat syscall
pub const SYSCALL_FACCESSAT: usize = 48;
/// chdir syscall
pub const SYSCALL_CHDIR: usize = 49;
/// fchmod syscall
pub const SYSCALL_FCHMOD: usize = 52;
/// fchmodat syscall
pub const SYSCALL_FCHMODAT: usize = 53;
/// openat syscall
pub const SYSCALL_OPENAT: usize = 56;
/// close syscall
pub const SYSCALL_CLOSE: usize = 57;
/// pipe syscall
pub const SYSCALL_PIPE2: usize = 59;
/// getdents64 syscall
pub const SYSCALL_GETDENTS64: usize = 61;
/// read syscall
pub const SYSCALL_READ: usize = 63;
/// write syscall
pub const SYSCALL_WRITE: usize = 64;
/// writev syscall
pub const SYSCALL_WRITEV: usize = 66;
/// ppoll_time32 syscall
pub const SYSCALL_PPOLL_TIME32: usize = 73;
/// newfstatat syscall
pub const SYSCALL_NEWFSTATAT: usize = 79;
/// utimensat syscall
pub const SYSCALL_UTIMENSAT: usize = 88;
/// fstat syscall
pub const SYSCALL_FSTAT: usize = 80;
/// exit syscall
pub const SYSCALL_EXIT: usize = 93;
/// exit group syscall
pub const SYSCALL_EXIT_GROUP: usize = 94;
/// set tid address syscall
pub const SYSCALL_SET_TID_ADDRESS: usize = 96;
/// set robust list syscall
pub const SYSCALL_SET_ROBUST_LIST: usize = 99;
/// get robust list syscall
pub const SYSCALL_GET_ROBUST_LIST: usize = 100;
/// sleep syscall
pub const SYSCALL_NANOSLEEP: usize = 101;
/// clock_settime syscall
pub const SYSCALL_CLOCK_SETTIME: usize = 112;
/// clock_gettime syscall
pub const SYSCALL_CLOCK_GETTIME: usize = 113;
/// syslog syscall
pub const SYSCALL_SYSLOG: usize = 116;
/// yield syscall
pub const SYSCALL_YIELD: usize = 124;
/// kill syscall
pub const SYSCALL_KILL: usize = 129;
/// tkill syscall
pub const SYSCALL_TKILL: usize = 130;
/// tgkill syscall
pub const SYSCALL_TGKILL: usize = 131;
/// sigaction syscall
pub const SYSCALL_SIGACTION: usize = 134;
/// sigprocmask syscall
pub const SYSCALL_SIGPROCMASK: usize = 135;
/// sigreturn syscall
pub const SYSCALL_SIGRETURN: usize = 139;
/// set priority syscall
pub const SYSCALL_SET_PRIORITY: usize = 140;
/// times syscall
pub const SYSCALL_TIMES: usize = 153;
/// uname syscall
pub const SYSCALL_UNAME: usize = 160;
/// gettimeofday syscall
pub const SYSCALL_GETTIMEOFDAY: usize = 169;
/// settimeofday syscall
pub const SYSCALL_SETTIMEOFDAY: usize = 170;
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
/// gettid syscall
pub const SYSCALL_GETTID: usize = 178;
/// brk syscall
pub const SYSCALL_BRK: usize = 214;
/// munmap syscall
pub const SYSCALL_MUNMAP: usize = 215;
/// fork syscall
pub const SYSCALL_FORK: usize = 220;
/// execve syscall
pub const SYSCALL_EXECVE: usize = 221;
/// mmap syscall
pub const SYSCALL_MMAP: usize = 222;
/// mprotect syscall
pub const SYSCALL_MPROTECT: usize = 226;
/// waitpid syscall
pub const SYSCALL_WAIT4: usize = 260;
/// renameat2 syscall
pub const SYSCALL_RENAMEAT2: usize = 276;
/// getrandom syscall
pub const SYSCALL_GETRANDOM: usize = 278;
/// spawn syscall
pub const SYSCALL_SPAWN: usize = 400;
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

mod fs;
mod process;
mod sync;
mod thread;
mod random;
mod mman;
mod times;
mod utils;

/// Standard error numbers and conversion traits
pub mod errno;

use fs::*;
use process::*;
use sync::*;
use thread::*;
use crate::syscall::random::*;
use mman::*;
use times::*;
pub(crate) use utils::{write_bytes_to_user, write_pod_to_user, Pod};


use crate::{fs::Stat, syscall::errno::ERRNO};
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

/// 系统调用分发入口：根据 `syscall_id` 将参数路由到具体 `sys_*` 实现。
pub fn syscall(syscall_id: usize, args: [usize; 6]) -> isize {
    match syscall_id {
        SYSCALL_DUP => sys_dup(args[0] as u32),
        SYSCALL_DUP2 => sys_dup2(args[0] as u32, args[1] as u32),
        SYSCALL_FCNTL => sys_fcntl(args[0] as u32, args[1] as i32, args[2]),
        SYSCALL_IOCTL => sys_ioctl(args[0] as u32, args[1], args[2]),
        SYSCALL_UNLINKAT => sys_unlinkat(args[0] as isize, args[1] as *const u8, args[2] as u32),
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
        SYSCALL_FACCESSAT => sys_faccessat(args[0] as isize, args[1] as *const u8, args[2] as i32),
        SYSCALL_FCHMOD => sys_fchmod(args[0] as u32, args[1] as u32),
        SYSCALL_FCHMODAT => sys_fchmodat(args[0] as isize, args[1] as *const u8, args[2] as u32, args[3] as i32),
        SYSCALL_OPENAT => sys_open(args[0] as isize, args[1] as *const u8, args[2] as i32, args[3] as u32),
        SYSCALL_CLOSE => sys_close(args[0] as u32),
        SYSCALL_PIPE2 => sys_pipe2(args[0] as *mut i32, args[1] as i32),
        SYSCALL_READ => sys_read(args[0] as u32, args[1] as *const u8, args[2]),
        SYSCALL_WRITE => sys_write(args[0] as u32, args[1] as *const u8, args[2]),
        SYSCALL_WRITEV => sys_writev(args[0] as u32, args[1] as *const IoVec, args[2] as i32),
        SYSCALL_NEWFSTATAT => sys_newfstatat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut Stat,
            args[3] as i32,
        ),
        SYSCALL_UTIMENSAT => sys_utimensat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *const Timespec,
            args[3] as i32,
        ),
        SYSCALL_PPOLL_TIME32 => sys_ppoll_time32(
            args[0] as *mut PollFd,
            args[1] as u32,
            args[2] as *const OldTimespec32,
            args[3] as *const u8,
            args[4],
        ),
        SYSCALL_FSTAT => sys_fstat(args[0] as u32, args[1] as *mut Stat),
        SYSCALL_GETRANDOM => sys_getrandom(args[0] as *mut u8, args[1], args[2]),
        SYSCALL_GETCWD => sys_getcwd(args[0] as *mut u8, args[1]),
        SYSCALL_MKDIRAT => sys_mkdirat(args[0] as isize, args[1] as *const u8, args[2] as u32),
        SYSCALL_CHDIR => sys_chdir(args[0] as *const u8),
        SYSCALL_GETDENTS64 => sys_getdents64(args[0] as u32, args[1] as *mut u8, args[2]),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_EXIT_GROUP => sys_exit_group(args[0] as i32),
        SYSCALL_SET_TID_ADDRESS => sys_set_tid_address(args[0] as *mut i32),
        SYSCALL_SET_ROBUST_LIST => sys_set_robust_list(args[0], args[1]),
        SYSCALL_GET_ROBUST_LIST => sys_get_robust_list(args[0] as i32, args[1] as *mut usize, args[2] as *mut usize),
        SYSCALL_NANOSLEEP => sys_nanosleep(args[0] as *const Timespec, args[1] as *mut Timespec),
        SYSCALL_CLOCK_SETTIME => sys_clock_settime(args[0] as ClockId, args[1] as *const Timespec),
        SYSCALL_CLOCK_GETTIME => sys_clock_gettime(args[0] as ClockId, args[1] as *mut Timespec),
        SYSCALL_SYSLOG => sys_syslog(args[0] as usize, args[1] as *mut u8, args[2] as usize),
        SYSCALL_YIELD => sys_yield(),
        SYSCALL_UNAME => sys_uname(args[0] as *mut UtsName),
        SYSCALL_GETPID => sys_getpid(),
        SYSCALL_GETPPID => sys_getppid(),
        SYSCALL_GETUID => sys_getuid(),
        SYSCALL_GETEUID => sys_geteuid(),
        SYSCALL_GETGID => sys_getgid(),
        SYSCALL_GETEGID => sys_getegid(),
        SYSCALL_GETTID => sys_gettid(),
        SYSCALL_FORK => sys_fork(),
        SYSCALL_EXECVE => sys_execve(
            args[0] as *const u8,
            args[1] as *const usize,
            args[2] as *const usize,
        ),
        SYSCALL_WAIT4 => sys_wait4(args[0] as isize, args[1] as *mut i32, args[2] as isize),
        SYSCALL_GETTIMEOFDAY => sys_get_time_of_day(args[0] as *mut TimeVal, args[1]),
        SYSCALL_SETTIMEOFDAY => sys_set_time_of_day(args[0] as *const TimeVal, args[1]),
        SYSCALL_TIMES => sys_times(args[0] as *mut Tms),
        SYSCALL_BRK => sys_brk(args[0]),
        SYSCALL_MMAP => sys_mmap(args[0], args[1], args[2], args[3], args[4], args[5]),
        SYSCALL_MPROTECT => sys_mprotect(args[0], args[1], args[2]),
        SYSCALL_MUNMAP => sys_munmap(args[0], args[1]),
        SYSCALL_SET_PRIORITY => sys_set_priority(args[0] as isize),
        SYSCALL_RENAMEAT2 => sys_renameat2(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as isize,
            args[3] as *const u8,
            args[4] as u32,
        ),
        SYSCALL_SIGACTION => sys_sigaction(args[0] as i32, args[1] as *const crate::task::SignalAction, args[2] as *mut crate::task::SignalAction),
        SYSCALL_SIGPROCMASK => sys_sigprocmask(args[0] as i32, args[1] as *const u32, args[2] as *mut u32, args[3]),
        SYSCALL_SIGRETURN => sys_sigreturn(),
        SYSCALL_SPAWN => sys_spawn(args[0] as *const u8),
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
        SYSCALL_KILL => sys_kill(args[0], args[1] as u32),
        SYSCALL_TKILL => sys_tkill(args[0], args[1] as u32),
        SYSCALL_TGKILL => sys_tgkill(args[0], args[1], args[2] as u32),
        _ => sys_nisyscall(syscall_id, args),
    }
}

/// Syscalls that are invalid or not implemented yet
fn sys_nisyscall(syscall_id: usize, args: [usize; 6]) -> isize {
    syscall_body!({  
        error!("unknown syscall: id = {}, args = {:?}", syscall_id, args);
        Err(ERRNO::ENOSYS)
    })
}
