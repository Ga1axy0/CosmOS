use crate::SignalAction;

use super::{Itimerval, Stat, TimeVal};

pub const SYSCALL_GETCWD: usize = 17;
pub const SYSCALL_DUP: usize = 23;
pub const SYSCALL_FCNTL: usize = 25;
pub const SYSCALL_MKDIRAT: usize = 34;
pub const SYSCALL_UNLINKAT: usize = 35;
pub const SYSCALL_SYMLINKAT: usize = 36;
pub const SYSCALL_LINKAT: usize = 37;
pub const SYSCALL_STATFS64: usize = 43;
pub const SYSCALL_FSTATFS64: usize = 44;
pub const SYSCALL_TRUNCATE: usize = 45;
pub const SYSCALL_FTRUNCATE: usize = 46;
pub const SYSCALL_CHDIR: usize = 49;
pub const SYSCALL_OPENAT: usize = 56;
pub const SYSCALL_CLOSE: usize = 57;
pub const SYSCALL_PIPE: usize = 59;
pub const SYSCALL_GETDENTS64: usize = 61;
pub const SYSCALL_READ: usize = 63;
pub const SYSCALL_WRITE: usize = 64;
pub const SYSCALL_READLINKAT: usize = 78;
pub const SYSCALL_NEWFSTATAT: usize = 79;
pub const SYSCALL_FSTAT: usize = 80;
pub const SYSCALL_EXIT: usize = 93;
pub const SYSCALL_SLEEP: usize = 101;
pub const SYSCALL_GETITIMER: usize = 102;
pub const SYSCALL_SETITIMER: usize = 103;
pub const SYSCALL_YIELD: usize = 124;
pub const SYSCALL_KILL: usize = 129;
pub const SYSCALL_SIGACTION: usize = 134;
pub const SYSCALL_SIGPROCMASK: usize = 135;
pub const SYSCALL_SIGRETURN: usize = 139;
pub const SYSCALL_SET_PRIORITY: usize = 140;
pub const SYSCALL_GET_PRIORITY: usize = 141;
pub const SYSCALL_SCHED_SETSCHEDULER: usize = 119;
pub const SYSCALL_SCHED_GETSCHEDULER: usize = 120;
pub const SYSCALL_SCHED_GETPARAM: usize = 121;
pub const SYSCALL_SCHED_SETAFFINITY: usize = 122;
pub const SYSCALL_SCHED_GETAFFINITY: usize = 123;
pub const SYSCALL_GETPGID: usize = 154;
pub const SYSCALL_SETPGID: usize = 155;
pub const SYSCALL_GETSID: usize = 156;
pub const SYSCALL_SETSID: usize = 157;
pub const SYSCALL_GETTIMEOFDAY: usize = 169;
pub const SYSCALL_GETPID: usize = 172;
pub const SYSCALL_SHMGET: usize = 194;
pub const SYSCALL_SHMCTL: usize = 195;
pub const SYSCALL_SHMAT: usize = 196;
pub const SYSCALL_SHMDT: usize = 197;
pub const SYSCALL_SOCKET: usize = 198;
pub const SYSCALL_SOCKETPAIR: usize = 199;
pub const SYSCALL_BIND: usize = 200;
pub const SYSCALL_LISTEN: usize = 201;
pub const SYSCALL_ACCEPT: usize = 202;
pub const SYSCALL_CONNECT: usize = 203;
pub const SYSCALL_ACCEPT4: usize = 242;
pub const SYSCALL_GETSOCKNAME: usize = 204;
pub const SYSCALL_GETPEERNAME: usize = 205;
pub const SYSCALL_SENDTO: usize = 206;
pub const SYSCALL_RECVFROM: usize = 207;
pub const SYSCALL_SHUTDOWN: usize = 210;
pub const SYSCALL_SENDMSG: usize = 211;
pub const SYSCALL_RECVMSG: usize = 212;
pub const SYSCALL_GETTID: usize = 178;
pub const SYSCALL_CLONE: usize = 220;
pub const SYSCALL_EXECVE: usize = 221;
pub const SYSCALL_WAITPID: usize = 260;
pub const SYSCALL_BRK: usize = 214;
pub const SYSCALL_MUNMAP: usize = 215;
pub const SYSCALL_MMAP: usize = 222;
pub const SYSCALL_SPAWN: usize = 400;
pub const SYSCALL_MAIL_READ: usize = 401;
pub const SYSCALL_MAIL_WRITE: usize = 402;
pub const SYSCALL_TRACE: usize = 410;
pub const SYSCALL_THREAD_CREATE: usize = 460;
pub const SYSCALL_WAITTID: usize = 462;
pub const SYSCALL_MUTEX_CREATE: usize = 463;
pub const SYSCALL_MUTEX_LOCK: usize = 464;
pub const SYSCALL_MUTEX_UNLOCK: usize = 466;
pub const SYSCALL_SEMAPHORE_CREATE: usize = 467;
pub const SYSCALL_SEMAPHORE_UP: usize = 468;
pub const SYSCALL_ENABLE_DEADLOCK_DETECT: usize = 469;
pub const SYSCALL_SEMAPHORE_DOWN: usize = 470;
pub const SYSCALL_CONDVAR_CREATE: usize = 471;
pub const SYSCALL_CONDVAR_SIGNAL: usize = 472;
pub const SYSCALL_CONDVAR_WAIT: usize = 473;

pub fn syscall(id: usize, args: [usize; 3]) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!(
            "ecall",
            inlateout("x10") args[0] => ret,
            in("x11") args[1],
            in("x12") args[2],
            in("x17") id
        );
    }
    ret
}

pub fn syscall6(id: usize, args: [usize; 6]) -> isize {
    let mut ret: isize;
    unsafe {
        core::arch::asm!("ecall",
            inlateout("x10") args[0] => ret,
            in("x11") args[1],
            in("x12") args[2],
            in("x13") args[3],
            in("x14") args[4],
            in("x15") args[5],
            in("x17") id
        );
    }
    ret
}

pub fn sys_openat(dirfd: usize, path: &str, flags: u32, mode: u32) -> isize {
    syscall6(
        SYSCALL_OPENAT,
        [
            dirfd,
            path.as_ptr() as usize,
            flags as usize,
            mode as usize,
            0,
            0,
        ],
    )
}

pub fn sys_close(fd: usize) -> isize {
    syscall(SYSCALL_CLOSE, [fd, 0, 0])
}

pub fn sys_read(fd: usize, buffer: &mut [u8]) -> isize {
    syscall(
        SYSCALL_READ,
        [fd, buffer.as_mut_ptr() as usize, buffer.len()],
    )
}

pub fn sys_write(fd: usize, buffer: &[u8]) -> isize {
    syscall(SYSCALL_WRITE, [fd, buffer.as_ptr() as usize, buffer.len()])
}

pub fn sys_linkat(
    old_dirfd: usize,
    old_path: &str,
    new_dirfd: usize,
    new_path: &str,
    flags: usize,
) -> isize {
    syscall6(
        SYSCALL_LINKAT,
        [
            old_dirfd,
            old_path.as_ptr() as usize,
            new_dirfd,
            new_path.as_ptr() as usize,
            flags,
            0,
        ],
    )
}

pub fn sys_symlinkat(target: &str, new_dirfd: usize, linkpath: &str) -> isize {
    syscall(
        SYSCALL_SYMLINKAT,
        [target.as_ptr() as usize, new_dirfd, linkpath.as_ptr() as usize],
    )
}

pub fn sys_readlinkat(dirfd: usize, path: &str, buffer: &mut [u8]) -> isize {
    syscall6(
        SYSCALL_READLINKAT,
        [
            dirfd,
            path.as_ptr() as usize,
            buffer.as_mut_ptr() as usize,
            buffer.len(),
            0,
            0,
        ],
    )
}

pub fn sys_unlinkat(dirfd: usize, path: &str, flags: usize) -> isize {
    syscall(SYSCALL_UNLINKAT, [dirfd, path.as_ptr() as usize, flags])
}

/// `truncate` 用户态封装：按路径调整文件长度。
pub fn sys_truncate(path: &str, len: isize) -> isize {
    syscall(SYSCALL_TRUNCATE, [path.as_ptr() as usize, len as usize, 0])
}

/// `ftruncate` 用户态封装：按文件描述符调整文件长度。
pub fn sys_ftruncate(fd: usize, len: isize) -> isize {
    syscall(SYSCALL_FTRUNCATE, [fd, len as usize, 0])
}

pub fn sys_fstat(fd: usize, st: &mut Stat) -> isize {
    syscall(SYSCALL_FSTAT, [fd, st as *const _ as usize, 0])
}

/// `newfstatat` 用户态封装：按目录 fd 与路径查询文件状态。
pub fn sys_newfstatat(dirfd: usize, path: &str, st: &mut Stat, flags: i32) -> isize {
    syscall6(
        SYSCALL_NEWFSTATAT,
        [dirfd, path.as_ptr() as usize, st as *const _ as usize, flags as usize, 0, 0],
    )
}

pub fn sys_mail_read(buffer: &mut [u8]) -> isize {
    syscall(
        SYSCALL_MAIL_READ,
        [buffer.as_ptr() as usize, buffer.len(), 0],
    )
}

pub fn sys_mail_write(pid: usize, buffer: &[u8]) -> isize {
    syscall(
        SYSCALL_MAIL_WRITE,
        [pid, buffer.as_ptr() as usize, buffer.len()],
    )
}

pub fn sys_exit(exit_code: i32) -> ! {
    syscall(SYSCALL_EXIT, [exit_code as usize, 0, 0]);
    panic!("sys_exit never returns!");
}

pub fn sys_sleep(sleep_ms: usize) -> isize {
    syscall(SYSCALL_SLEEP, [sleep_ms, 0, 0])
}

pub fn sys_getitimer(which: i32, value: *mut Itimerval) -> isize {
    syscall(SYSCALL_GETITIMER, [which as usize, value as usize, 0])
}

pub fn sys_setitimer(which: i32, value: *const Itimerval, ovalue: *mut Itimerval) -> isize {
    syscall(
        SYSCALL_SETITIMER,
        [which as usize, value as usize, ovalue as usize],
    )
}

pub fn sys_yield() -> isize {
    syscall(SYSCALL_YIELD, [0, 0, 0])
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SchedParam {
    pub sched_priority: i32,
}

pub fn sys_sched_setscheduler(pid: isize, policy: i32, param: &SchedParam) -> isize {
    syscall(
        SYSCALL_SCHED_SETSCHEDULER,
        [pid as usize, policy as usize, param as *const _ as usize],
    )
}

pub fn sys_sched_getscheduler(pid: isize) -> isize {
    syscall(SYSCALL_SCHED_GETSCHEDULER, [pid as usize, 0, 0])
}

pub fn sys_sched_getparam(pid: isize, param: &mut SchedParam) -> isize {
    syscall(
        SYSCALL_SCHED_GETPARAM,
        [pid as usize, param as *mut _ as usize, 0],
    )
}

pub fn sys_get_time(time: &mut TimeVal, tz: usize) -> isize {
    syscall(SYSCALL_GETTIMEOFDAY, [time as *const _ as usize, tz, 0])
}

pub fn sys_getpid() -> isize {
    syscall(SYSCALL_GETPID, [0, 0, 0])
}

pub fn sys_getpgid(pid: isize) -> isize {
    syscall(SYSCALL_GETPGID, [pid as usize, 0, 0])
}

pub fn sys_setpgid(pid: isize, pgid: isize) -> isize {
    syscall(SYSCALL_SETPGID, [pid as usize, pgid as usize, 0])
}

pub fn sys_getsid() -> isize {
    syscall(SYSCALL_GETSID, [0, 0, 0])
}

pub fn sys_setsid() -> isize {
    syscall(SYSCALL_SETSID, [0, 0, 0])
}

pub fn sys_socket(domain: usize, socket_type: usize, protocol: usize) -> isize {
    syscall(SYSCALL_SOCKET, [domain, socket_type, protocol])
}

pub fn sys_socketpair(
    domain: usize,
    socket_type: usize,
    protocol: usize,
    sv: *mut i32,
) -> isize {
    syscall6(
        SYSCALL_SOCKETPAIR,
        [domain, socket_type, protocol, sv as usize, 0, 0],
    )
}

pub fn sys_bind(fd: usize, addr: *const crate::net::SockAddrIn, addrlen: usize) -> isize {
    syscall(SYSCALL_BIND, [fd, addr as usize, addrlen])
}

pub fn sys_listen(fd: usize, backlog: usize) -> isize {
    syscall(SYSCALL_LISTEN, [fd, backlog, 0])
}

pub fn sys_accept(fd: usize, addr: *mut crate::net::SockAddrIn, addrlen: *mut i32) -> isize {
    syscall(SYSCALL_ACCEPT, [fd, addr as usize, addrlen as usize])
}

pub fn sys_accept4(
    fd: usize,
    addr: *mut crate::net::SockAddrIn,
    addrlen: *mut i32,
    flags: usize,
) -> isize {
    syscall6(SYSCALL_ACCEPT4, [fd, addr as usize, addrlen as usize, flags, 0, 0])
}

pub fn sys_connect(fd: usize, addr: *const crate::net::SockAddrIn, addrlen: usize) -> isize {
    syscall(SYSCALL_CONNECT, [fd, addr as usize, addrlen])
}

pub fn sys_getsockname(fd: usize, addr: *mut crate::net::SockAddrIn, addrlen: usize) -> isize {
    syscall(SYSCALL_GETSOCKNAME, [fd, addr as usize, addrlen])
}

pub fn sys_getpeername(fd: usize, addr: *mut crate::net::SockAddrIn, addrlen: usize) -> isize {
    syscall(SYSCALL_GETPEERNAME, [fd, addr as usize, addrlen])
}

pub fn sys_sendto(
    fd: usize,
    buf: *const u8,
    len: usize,
    flags: usize,
    addr: *const crate::net::SockAddrIn,
    addrlen: usize,
) -> isize {
    syscall6(
        SYSCALL_SENDTO,
        [fd, buf as usize, len, flags, addr as usize, addrlen],
    )
}

pub fn sys_recvfrom(
    fd: usize,
    buf: *mut u8,
    len: usize,
    flags: usize,
    addr: *mut crate::net::SockAddrIn,
    addrlen: usize,
) -> isize {
    syscall6(
        SYSCALL_RECVFROM,
        [fd, buf as usize, len, flags, addr as usize, addrlen],
    )
}

pub fn sys_shutdown(fd: usize, how: usize) -> isize {
    syscall(SYSCALL_SHUTDOWN, [fd, how, 0])
}

pub fn sys_sendmsg(fd: usize, msg: *const crate::net::MsgHdr, flags: usize) -> isize {
    syscall(SYSCALL_SENDMSG, [fd, msg as usize, flags])
}

pub fn sys_recvmsg(fd: usize, msg: *mut crate::net::MsgHdr, flags: usize) -> isize {
    syscall(SYSCALL_RECVMSG, [fd, msg as usize, flags])
}

pub fn sys_shmget(key: i32, size: usize, flags: i32) -> isize {
    syscall(SYSCALL_SHMGET, [key as usize, size, flags as usize])
}

pub fn sys_shmctl(shmid: usize, cmd: i32, buf: usize) -> isize {
    syscall(SYSCALL_SHMCTL, [shmid, cmd as usize, buf])
}

pub fn sys_shmat(shmid: usize, shmaddr: usize, shmflg: i32) -> isize {
    syscall(SYSCALL_SHMAT, [shmid, shmaddr, shmflg as usize])
}

pub fn sys_shmdt(shmaddr: usize) -> isize {
    syscall(SYSCALL_SHMDT, [shmaddr, 0, 0])
}

/// Linux `clone` 系统调用封装。
pub fn sys_clone(
    flags: usize,
    stack: usize,
    parent_tid: usize,
    tls: usize,
    child_tid: usize,
) -> isize {
    syscall6(
        SYSCALL_CLONE,
        [flags, stack, parent_tid, tls, child_tid, 0],
    )
}

pub fn sys_execve(path: &str, args: &[*const u8], envp: &[*const u8]) -> isize {
    syscall(
        SYSCALL_EXECVE,
        [path.as_ptr() as usize, args.as_ptr() as usize, envp.as_ptr() as usize],
    )
}

pub fn sys_waitpid(pid: isize, xstatus: *mut i32) -> isize {
    syscall(SYSCALL_WAITPID, [pid as usize, xstatus as usize, 0])
}

pub fn sys_setpriority(which: i32, who: usize, prio: i32) -> isize {
    syscall(SYSCALL_SET_PRIORITY, [which as usize, who, prio as usize])
}

pub fn sys_getpriority(which: i32, who: usize) -> isize {
    syscall(SYSCALL_GET_PRIORITY, [which as usize, who, 0])
}

pub fn sys_set_priority(prio: isize) -> isize {
    sys_setpriority(0, 0, prio as i32)
}

pub fn sys_brk(addr: usize) -> isize {
    syscall(SYSCALL_BRK, [addr, 0, 0])
}

pub fn sys_mmap(start: usize, len: usize, prot: usize) -> isize {
    syscall6(SYSCALL_MMAP, [start, len, prot, 0, 0, 0])
}

/// `mmap` 用户态完整封装，支持文件映射所需的 6 参数形式。
pub fn sys_mmap_full(
    start: usize,
    len: usize,
    prot: usize,
    flags: usize,
    fd: usize,
    offset: usize,
) -> isize {
    syscall6(SYSCALL_MMAP, [start, len, prot, flags, fd, offset])
}

pub fn sys_munmap(start: usize, len: usize) -> isize {
    syscall(SYSCALL_MUNMAP, [start, len, 0])
}

pub fn sys_spawn(path: &str) -> isize {
    syscall(SYSCALL_SPAWN, [path.as_ptr() as usize, 0, 0])
}

pub fn sys_dup(fd: usize) -> isize {
    syscall(SYSCALL_DUP, [fd, 0, 0])
}

pub fn sys_fcntl(fd: usize, cmd: i32, arg: i32) -> isize {
    syscall(SYSCALL_FCNTL, [fd, cmd as usize, arg as usize])
}

pub fn sys_pipe(pipe: &mut [usize]) -> isize {
    syscall(SYSCALL_PIPE, [pipe.as_mut_ptr() as usize, 0, 0])
}

pub fn sys_trace(trace_request: usize, id: usize, data: usize) -> isize {
    syscall(SYSCALL_TRACE, [trace_request, id, data])
}

pub fn sys_thread_create(entry: usize, arg: usize) -> isize {
    syscall(SYSCALL_THREAD_CREATE, [entry, arg, 0])
}

pub fn sys_gettid() -> isize {
    syscall(SYSCALL_GETTID, [0; 3])
}

pub fn sys_waittid(tid: usize) -> isize {
    syscall(SYSCALL_WAITTID, [tid, 0, 0])
}

pub fn sys_mutex_create(blocking: bool) -> isize {
    syscall(SYSCALL_MUTEX_CREATE, [blocking as usize, 0, 0])
}

pub fn sys_mutex_lock(id: usize) -> isize {
    syscall(SYSCALL_MUTEX_LOCK, [id, 0, 0])
}

pub fn sys_mutex_unlock(id: usize) -> isize {
    syscall(SYSCALL_MUTEX_UNLOCK, [id, 0, 0])
}

pub fn sys_semaphore_create(res_count: usize) -> isize {
    syscall(SYSCALL_SEMAPHORE_CREATE, [res_count, 0, 0])
}

pub fn sys_semaphore_up(sem_id: usize) -> isize {
    syscall(SYSCALL_SEMAPHORE_UP, [sem_id, 0, 0])
}

pub fn sys_enable_deadlock_detect(enabled: usize) -> isize {
    syscall(SYSCALL_ENABLE_DEADLOCK_DETECT, [enabled, 0, 0])
}

pub fn sys_semaphore_down(sem_id: usize) -> isize {
    syscall(SYSCALL_SEMAPHORE_DOWN, [sem_id, 0, 0])
}

pub fn sys_condvar_create(_arg: usize) -> isize {
    syscall(SYSCALL_CONDVAR_CREATE, [_arg, 0, 0])
}

pub fn sys_condvar_signal(condvar_id: usize) -> isize {
    syscall(SYSCALL_CONDVAR_SIGNAL, [condvar_id, 0, 0])
}

pub fn sys_condvar_wait(condvar_id: usize, mutex_id: usize) -> isize {
    syscall(SYSCALL_CONDVAR_WAIT, [condvar_id, mutex_id, 0])
}

pub fn sys_sigaction(
    signum: i32,
    action: *const SignalAction,
    old_action: *mut SignalAction,
) -> isize {
    syscall6(
        SYSCALL_SIGACTION,
        [
            signum as usize,
            action as usize,
            old_action as usize,
            core::mem::size_of::<u64>(),
            0,
            0,
        ],
    )
}

/// New rt_sigprocmask ABI wrapper: (how, set, oset, sigsetsize)
pub fn sys_rt_sigprocmask(how: i32, set: *const u64, oset: *mut u64, sigsetsize: usize) -> isize {
    syscall6(
        SYSCALL_SIGPROCMASK,
        [how as usize, set as usize, oset as usize, sigsetsize, 0, 0],
    )
}

/// Compatibility helper that sets the mask to `mask` (SIG_SETMASK semantics).
pub fn sys_sigprocmask(mask: u64) -> isize {
    // SIG_SETMASK == 2
    let size = core::mem::size_of::<u64>();
    sys_rt_sigprocmask(2, &mask as *const u64, core::ptr::null_mut(), size)
}

pub fn sys_sigreturn() -> isize {
    syscall(SYSCALL_SIGRETURN, [0, 0, 0])
}

pub fn sys_kill(pid: usize, signal: i32) -> isize {
    syscall(SYSCALL_KILL, [pid, signal as usize, 0])
}

pub fn sys_getcwd(buffer: &mut [u8]) -> isize {
    syscall(
        SYSCALL_GETCWD,
        [buffer.as_mut_ptr() as usize, buffer.len(), 0],
    )
}

pub fn sys_mkdirat(dirfd: usize, path: &str, mode: u32) -> isize {
    syscall6(
        SYSCALL_MKDIRAT,
        [dirfd, path.as_ptr() as usize, mode as usize, 0, 0, 0],
    )
}

pub fn sys_chdir(path: &str) -> isize {
    syscall(SYSCALL_CHDIR, [path.as_ptr() as usize, 0, 0])
}

pub fn sys_getdents64(fd: usize, buffer: &mut [u8]) -> isize {
    syscall(
        SYSCALL_GETDENTS64,
        [fd, buffer.as_mut_ptr() as usize, buffer.len()],
    )
}

pub fn sys_statfs64(path: &str, buf: &mut super::StatFs64) -> isize {
    syscall6(
        SYSCALL_STATFS64,
        [
            path.as_ptr() as usize,
            buf as *mut _ as usize,
            0,
            0,
            0,
            0,
        ],
    )
}

pub fn sys_fstatfs64(fd: usize, buf: &mut super::StatFs64) -> isize {
    syscall6(
        SYSCALL_FSTATFS64,
        [
            fd,
            buf as *mut _ as usize,
            0,
            0,
            0,
            0,
        ],
    )
}
