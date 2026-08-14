//! Optional per-system-call invocation counters.

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

// xxOS currently uses syscall numbers through 473. Keep a little headroom for
// ABI additions while still using a fixed, allocation-free hot-path counter.
const MAX_SYSCALL_NR: usize = 512;

static COUNTS: [AtomicU64; MAX_SYSCALL_NR] = [const { AtomicU64::new(0) }; MAX_SYSCALL_NR];

// Keep the names next to the counter implementation so /proc remains useful
// without requiring userspace to duplicate xxOS's syscall-number table.
const SYSCALL_NAMES: &[(usize, &str)] = &[
    (super::SYSCALL_GETCWD, "getcwd"),
    (super::SYSCALL_EVENTFD2, "eventfd2"),
    (super::SYSCALL_EPOLL_CREATE1, "epoll_create1"),
    (super::SYSCALL_EPOLL_CTL, "epoll_ctl"),
    (super::SYSCALL_EPOLL_PWAIT, "epoll_pwait"),
    (super::SYSCALL_DUP, "dup"),
    (super::SYSCALL_DUP2, "dup2"),
    (super::SYSCALL_FCNTL, "fcntl"),
    (super::SYSCALL_INOTIFY_INIT1, "inotify_init1"),
    (super::SYSCALL_IOCTL, "ioctl"),
    (super::SYSCALL_FLOCK, "flock"),
    (super::SYSCALL_MKDIRAT, "mkdirat"),
    (super::SYSCALL_UNLINKAT, "unlinkat"),
    (super::SYSCALL_SYMLINKAT, "symlinkat"),
    (super::SYSCALL_LINKAT, "linkat"),
    (super::SYSCALL_UMOUNT, "umount"),
    (super::SYSCALL_MOUNT, "mount"),
    (super::SYSCALL_PIVOT_ROOT, "pivot_root"),
    (super::SYSCALL_STATFS64, "statfs64"),
    (super::SYSCALL_FSTATFS64, "fstatfs64"),
    (super::SYSCALL_TRUNCATE, "truncate"),
    (super::SYSCALL_FTRUNCATE, "ftruncate"),
    (super::SYSCALL_FALLOCATE, "fallocate"),
    (super::SYSCALL_FACCESSAT, "faccessat"),
    (super::SYSCALL_CHDIR, "chdir"),
    (super::SYSCALL_FCHDIR, "fchdir"),
    (super::SYSCALL_CHROOT, "chroot"),
    (super::SYSCALL_FCHMOD, "fchmod"),
    (super::SYSCALL_FCHMODAT, "fchmodat"),
    (super::SYSCALL_FCHOWNAT, "fchownat"),
    (super::SYSCALL_FCHOWN, "fchown"),
    (super::SYSCALL_OPENAT, "openat"),
    (super::SYSCALL_CLOSE, "close"),
    (super::SYSCALL_PIPE2, "pipe2"),
    (super::SYSCALL_GETDENTS64, "getdents64"),
    (super::SYSCALL_LSEEK, "lseek"),
    (super::SYSCALL_READ, "read"),
    (super::SYSCALL_WRITE, "write"),
    (super::SYSCALL_READV, "readv"),
    (super::SYSCALL_WRITEV, "writev"),
    (super::SYSCALL_PREAD64, "pread64"),
    (super::SYSCALL_PWRITE64, "pwrite64"),
    (super::SYSCALL_PREADV, "preadv"),
    (super::SYSCALL_PWRITEV, "pwritev"),
    (super::SYSCALL_SENDFILE64, "sendfile64"),
    (super::SYSCALL_SPLICE, "splice"),
    (super::SYSCALL_PSELECT6, "pselect6"),
    (super::SYSCALL_PPOLL, "ppoll"),
    (super::SYSCALL_SIGNALFD4, "signalfd4"),
    (super::SYSCALL_READLINKAT, "readlinkat"),
    (super::SYSCALL_NEWFSTATAT, "newfstatat"),
    (super::SYSCALL_TIMERFD_CREATE, "timerfd_create"),
    (super::SYSCALL_UTIMENSAT, "utimensat"),
    (super::SYSCALL_ACCT, "acct"),
    (super::SYSCALL_CAPGET, "capget"),
    (super::SYSCALL_CAPSET, "capset"),
    (super::SYSCALL_FSTAT, "fstat"),
    (super::SYSCALL_SYNC, "sync"),
    (super::SYSCALL_FSYNC, "fsync"),
    (super::SYSCALL_FDATASYNC, "fdatasync"),
    (super::SYSCALL_EXIT, "exit"),
    (super::SYSCALL_EXIT_GROUP, "exit_group"),
    (super::SYSCALL_SET_TID_ADDRESS, "set_tid_address"),
    (super::SYSCALL_UNSHARE, "unshare"),
    (super::SYSCALL_FUTEX, "futex"),
    (super::SYSCALL_SET_ROBUST_LIST, "set_robust_list"),
    (super::SYSCALL_GET_ROBUST_LIST, "get_robust_list"),
    (super::SYSCALL_NANOSLEEP, "nanosleep"),
    (super::SYSCALL_GETITIMER, "getitimer"),
    (super::SYSCALL_SETITIMER, "setitimer"),
    (super::SYSCALL_TIMER_CREATE, "timer_create"),
    (super::SYSCALL_TIMER_SETTIME, "timer_settime"),
    (super::SYSCALL_CLOCK_SETTIME, "clock_settime"),
    (super::SYSCALL_CLOCK_GETTIME, "clock_gettime"),
    (super::SYSCALL_CLOCK_GETRES, "clock_getres"),
    (super::SYSCALL_CLOCK_NANOSLEEP, "clock_nanosleep"),
    (super::SYSCALL_SYSLOG, "syslog"),
    (super::SYSCALL_SCHED_SETSCHEDULER, "sched_setscheduler"),
    (super::SYSCALL_SCHED_GETSCHEDULER, "sched_getscheduler"),
    (super::SYSCALL_SCHED_GETPARAM, "sched_getparam"),
    (super::SYSCALL_SCHED_SETAFFINITY, "sched_setaffinity"),
    (super::SYSCALL_SCHED_GETAFFINITY, "sched_getaffinity"),
    (super::SYSCALL_YIELD, "yield"),
    (super::SYSCALL_KILL, "kill"),
    (super::SYSCALL_TKILL, "tkill"),
    (super::SYSCALL_TGKILL, "tgkill"),
    (super::SYSCALL_SIGALTSTACK, "sigaltstack"),
    (super::SYSCALL_SIGSUSPEND, "sigsuspend"),
    (super::SYSCALL_SIGACTION, "sigaction"),
    (super::SYSCALL_SIGPROCMASK, "sigprocmask"),
    (super::SYSCALL_RT_SIGPENDING, "rt_sigpending"),
    (super::SYSCALL_RT_SIGTIMEDWAIT, "rt_sigtimedwait"),
    (super::SYSCALL_SIGRETURN, "sigreturn"),
    (super::SYSCALL_SET_PRIORITY, "set_priority"),
    (super::SYSCALL_GET_PRIORITY, "get_priority"),
    (super::SYSCALL_SETREGID, "setregid"),
    (super::SYSCALL_SETGID, "setgid"),
    (super::SYSCALL_SETREUID, "setreuid"),
    (super::SYSCALL_SETUID, "setuid"),
    (super::SYSCALL_SETRESUID, "setresuid"),
    (super::SYSCALL_GETRESUID, "getresuid"),
    (super::SYSCALL_SETRESGID, "setresgid"),
    (super::SYSCALL_GETRESGID, "getresgid"),
    (super::SYSCALL_TIMES, "times"),
    (super::SYSCALL_SETPGID, "setpgid"),
    (super::SYSCALL_GETPGID, "getpgid"),
    (super::SYSCALL_GETSID, "getsid"),
    (super::SYSCALL_SETSID, "setsid"),
    (super::SYSCALL_GETGROUPS, "getgroups"),
    (super::SYSCALL_SETGROUPS, "setgroups"),
    (super::SYSCALL_UNAME, "uname"),
    (super::SYSCALL_GETRLIMIT, "getrlimit"),
    (super::SYSCALL_SETRLIMIT, "setrlimit"),
    (super::SYSCALL_GETRUSAGE, "getrusage"),
    (super::SYSCALL_UMASK, "umask"),
    (super::SYSCALL_PRCTL, "prctl"),
    (super::SYSCALL_GETCPU, "getcpu"),
    (super::SYSCALL_GETTIMEOFDAY, "gettimeofday"),
    (super::SYSCALL_SETTIMEOFDAY, "settimeofday"),
    (super::SYSCALL_ADJTIMEX, "adjtimex"),
    (super::SYSCALL_GETPID, "getpid"),
    (super::SYSCALL_GETPPID, "getppid"),
    (super::SYSCALL_GETUID, "getuid"),
    (super::SYSCALL_GETEUID, "geteuid"),
    (super::SYSCALL_GETGID, "getgid"),
    (super::SYSCALL_GETEGID, "getegid"),
    (super::SYSCALL_SYSINFO, "sysinfo"),
    (super::SYSCALL_GETTID, "gettid"),
    (super::SYSCALL_SHMGET, "shmget"),
    (super::SYSCALL_SHMCTL, "shmctl"),
    (super::SYSCALL_SHMAT, "shmat"),
    (super::SYSCALL_SHMDT, "shmdt"),
    (super::SYSCALL_SOCKET, "socket"),
    (super::SYSCALL_SETNS, "setns"),
    (super::SYSCALL_SOCKETPAIR, "socketpair"),
    (super::SYSCALL_BIND, "bind"),
    (super::SYSCALL_LISTEN, "listen"),
    (super::SYSCALL_ACCEPT, "accept"),
    (super::SYSCALL_CONNECT, "connect"),
    (super::SYSCALL_PERF_EVENT_OPEN, "perf_event_open"),
    (super::SYSCALL_ACCEPT4, "accept4"),
    (super::SYSCALL_GETSOCKNAME, "getsockname"),
    (super::SYSCALL_GETPEERNAME, "getpeername"),
    (super::SYSCALL_SENDTO, "sendto"),
    (super::SYSCALL_RECVFROM, "recvfrom"),
    (super::SYSCALL_SETSOCKOPT, "setsockopt"),
    (super::SYSCALL_GETSOCKOPT, "getsockopt"),
    (super::SYSCALL_SHUTDOWN, "shutdown"),
    (super::SYSCALL_SENDMSG, "sendmsg"),
    (super::SYSCALL_RECVMSG, "recvmsg"),
    (super::SYSCALL_BRK, "brk"),
    (super::SYSCALL_MREMAP, "mremap"),
    (super::SYSCALL_ADD_KEY, "add_key"),
    (super::SYSCALL_KEYCTL, "keyctl"),
    (super::SYSCALL_MUNMAP, "munmap"),
    (super::SYSCALL_CLONE, "clone"),
    (super::SYSCALL_CLONE3, "clone3"),
    (super::SYSCALL_EXECVE, "execve"),
    (super::SYSCALL_MMAP, "mmap"),
    (super::SYSCALL_FADVISE64, "fadvise64"),
    (super::SYSCALL_MPROTECT, "mprotect"),
    (super::SYSCALL_MSYNC, "msync"),
    (super::SYSCALL_MLOCK, "mlock"),
    (super::SYSCALL_MUNLOCK, "munlock"),
    (super::SYSCALL_MLOCKALL, "mlockall"),
    (super::SYSCALL_MUNLOCKALL, "munlockall"),
    (super::SYSCALL_MADVISE, "madvise"),
    (super::SYSCALL_GET_MEMPOLICY, "get_mempolicy"),
    (super::SYSCALL_WAIT4, "wait4"),
    (super::SYSCALL_PRLIMIT64, "prlimit64"),
    (super::SYSCALL_FANOTIFY_INIT, "fanotify_init"),
    (super::SYSCALL_SYNCFS, "syncfs"),
    (super::SYSCALL_CLOCK_ADJTIME, "clock_adjtime"),
    (super::SYSCALL_SCHED_SETATTR, "sched_setattr"),
    (super::SYSCALL_SCHED_GETATTR, "sched_getattr"),
    (super::SYSCALL_RENAMEAT2, "renameat2"),
    (super::SYSCALL_GETRANDOM, "getrandom"),
    (super::SYSCALL_MEMFD_CREATE, "memfd_create"),
    (super::SYSCALL_BPF, "bpf"),
    (super::SYSCALL_USERFAULTFD, "userfaultfd"),
    (super::SYSCALL_COPY_FILE_RANGE, "copy_file_range"),
    (super::SYSCALL_STATX, "statx"),
    (super::SYSCALL_SPAWN, "spawn"),
    (super::SYSCALL_CLOCK_ADJTIME64, "clock_adjtime64"),
    (super::SYSCALL_IO_URING_SETUP, "io_uring_setup"),
    (super::SYSCALL_PIDFD_SEND_SIGNAL, "pidfd_send_signal"),
    (super::SYSCALL_OPEN_TREE, "open_tree"),
    (super::SYSCALL_FSOPEN, "fsopen"),
    (super::SYSCALL_FSPICK, "fspick"),
    (super::SYSCALL_PIDFD_OPEN, "pidfd_open"),
    (super::SYSCALL_CLOSE_RANGE, "close_range"),
    (super::SYSCALL_FACCESSAT2, "faccessat2"),
    (super::SYSCALL_EPOLL_PWAIT2, "epoll_pwait2"),
    (super::SYSCALL_MEMFD_SECRET, "memfd_secret"),
    (super::SYSCALL_THREAD_CREATE, "thread_create"),
    (super::SYSCALL_WAITTID, "waittid"),
    (super::SYSCALL_MUTEX_CREATE, "mutex_create"),
    (super::SYSCALL_MUTEX_LOCK, "mutex_lock"),
    (super::SYSCALL_MUTEX_UNLOCK, "mutex_unlock"),
    (super::SYSCALL_SEMAPHORE_CREATE, "semaphore_create"),
    (super::SYSCALL_SEMAPHORE_UP, "semaphore_up"),
    (
        super::SYSCALL_ENABLE_DEADLOCK_DETECT,
        "enable_deadlock_detect",
    ),
    (super::SYSCALL_SEMAPHORE_DOWN, "semaphore_down"),
    (super::SYSCALL_CONDVAR_CREATE, "condvar_create"),
    (super::SYSCALL_CONDVAR_SIGNAL, "condvar_signal"),
    (super::SYSCALL_CONDVAR_WAIT, "condvar_wait"),
];

fn syscall_name(syscall_nr: usize) -> Option<&'static str> {
    SYSCALL_NAMES
        .iter()
        .find_map(|(number, name)| (*number == syscall_nr).then_some(*name))
}

/// Increment the counter for one syscall entry.
#[inline]
pub(crate) fn record(syscall_nr: usize) {
    if let Some(counter) = COUNTS.get(syscall_nr) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Clear all syscall counters.
pub(crate) fn reset() {
    for counter in &COUNTS {
        counter.store(0, Ordering::Relaxed);
    }
}

/// Render the counters in a procfs-friendly table.
pub(crate) fn render() -> String {
    let mut out = String::new();
    let _ = writeln!(&mut out, "syscall_nr name count");
    for (syscall_nr, counter) in COUNTS.iter().enumerate() {
        let count = counter.load(Ordering::Relaxed);
        if let Some(name) = syscall_name(syscall_nr) {
            let _ = writeln!(&mut out, "{} {} {}", syscall_nr, name, count);
        } else if count != 0 {
            let _ = writeln!(&mut out, "{} unknown {}", syscall_nr, count);
        }
    }
    out
}
