use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall::{write_pod_to_user, Pod};
use crate::syscall_body;
use crate::{
    fs::{open_file, open_file_at, File, OpenFlags},
    mm::{
        translated_ref, translated_refmut, translated_str,
    },
    task::{
        current_process, current_task, current_user_token, exit_current_and_run_next,
        pid2process, suspend_current_and_run_next, ExitReason, SignalAction, SignalFlags,
        WaitReason,
    },
};

use alloc::{string::String, vec::Vec};
/// exit syscall
///
/// exit the current task and run the next task in task list
pub fn sys_exit(exit_code: i32) -> ! {
    trace!(
        "kernel:pid[{}] sys_exit",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    exit_current_and_run_next(ExitReason::Exit(exit_code));
    panic!("Unreachable in sys_exit!");
}

/// 临时实现
pub fn sys_exit_group(exit_code: i32) -> ! {
    sys_exit(exit_code);
}

/// yield syscall
pub fn sys_yield() -> isize {
    //trace!("kernel: sys_yield");
    suspend_current_and_run_next();
    0
}
/// getpid syscall
pub fn sys_getpid() -> isize {
    trace!(
        "kernel: sys_getpid pid:{}",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    current_task().unwrap().process.upgrade().unwrap().getpid() as isize
}

/// getppid syscall
pub fn sys_getppid() -> isize {
    trace!(
        "kernel: sys_getppid pid:{}",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let parent = process.inner_exclusive_access().parent.clone();
    if let Some(parent) = parent.and_then(|parent| parent.upgrade()) {
        parent.getpid() as isize
    } else {
        0
    }
}

/// fork child process syscall
pub fn sys_fork() -> isize {
    trace!(
        "kernel:pid[{}] sys_fork",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let current_process = current_process();
    let new_process = current_process.fork();
    let new_pid = new_process.getpid();
    // modify trap context of new_task, because it returns immediately after switching
    let new_process_inner = new_process.inner_exclusive_access();
    let task = new_process_inner.tasks[0].as_ref().unwrap();
    let trap_cx = task.inner_exclusive_access().get_trap_cx();
    // we do not have to move to next instruction since we have done it before
    // for child process, fork returns 0
    trap_cx.x[10] = 0;
    new_pid as isize
}
/// sys_execve
pub fn sys_execve(path: *const u8, mut args: *const usize, mut envp: *const usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_execve",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        let mut args_vec: Vec<String> = Vec::new();
        loop {
            let arg_str_ptr = *translated_ref(token, args).or_errno(ERRNO::EFAULT)?;
            if arg_str_ptr == 0 {
                break;
            }
            args_vec.push(translated_str(token, arg_str_ptr as *const u8).or_errno(ERRNO::EFAULT)?);
            unsafe {
                args = args.add(1);
            }
        }
        // TODO：当前内核尚未实现进程环境变量表，这里先完成 ABI 级别的解析与校验。
        loop {
            let env_str_ptr = *translated_ref(token, envp).or_errno(ERRNO::EFAULT)?;
            if env_str_ptr == 0 {
                break;
            }
            translated_str(token, env_str_ptr as *const u8).or_errno(ERRNO::EFAULT)?;
            unsafe {
                envp = envp.add(1);
            }
        }

        let process = current_process();
        let cwd = process.inner_exclusive_access().cwd.clone();
        let app_inode =
            open_file_at(cwd.as_str(), path.as_str(), OpenFlags::RDONLY).or_errno(ERRNO::ENOENT)?;
        if app_inode.is_dir() {
            return Err(ERRNO::EISDIR);
        }
        let all_data = app_inode.read_all();
        let argc = args_vec.len();
        process
            .exec(all_data.as_slice(), args_vec)
            .or_errno(ERRNO::ENOEXEC)?;
        // trap 返回路径会覆盖 a0，这里返回 argc 以保持新程序入口参数正确。
        Ok(argc as isize)
    })
}

const WNOHANG: isize = 1;

/// waitpid syscall
///
/// If there is not a child process whose pid is same as given, return -ECHILD.
/// Else if there is a child process but it is still running, return -EAGAIN.
pub fn sys_wait4(pid: isize, exit_status_ptr: *mut i32, options: isize) -> isize {
    trace!("kernel: sys_wait4");
    let process = current_process();
    syscall_body!({
        if options & !WNOHANG != 0 {
            return Err(ERRNO::EINVAL);
        }

        loop {
            let mut inner = process.inner_exclusive_access();

            // 1) 没有任何匹配的子进程
            let has_target_child = inner
                .children
                .iter()
                .any(|p| pid == -1 || pid as usize == p.getpid());
            if !has_target_child {
                return Err(ERRNO::ECHILD);
            }

            // 2) 查找已经退出的目标子进程
            let zombie_idx = inner.children.iter().position(|p| {
                let p_inner = p.inner_exclusive_access();
                p_inner.is_zombie && (pid == -1 || pid as usize == p.getpid())
            });

            if let Some(idx) = zombie_idx {
                let child = inner.children.remove(idx);
                let found_pid = child.getpid();
                let child_inner = child.inner_exclusive_access();
                // 编码为wstatus
               let exit_status = match child_inner.exit_reason {
                    ExitReason::Exit(code) => (code & 0xff) << 8,
                    ExitReason::Signal(signum) => (signum & 0x7f) as i32,
                };
                inner.child_user_time = inner
                    .child_user_time
                    .saturating_add(child_inner.user_time)
                    .saturating_add(child_inner.child_user_time);
                inner.child_kernel_time = inner
                    .child_kernel_time
                    .saturating_add(child_inner.kernel_time)
                    .saturating_add(child_inner.child_kernel_time);
                let token = inner.memory_set.token();
                drop(child_inner);
                drop(inner);

                if !exit_status_ptr.is_null() {
                    if let Some(slot) = translated_refmut(token, exit_status_ptr) {
                        *slot = exit_status;
                    } else {
                        return Err(ERRNO::EFAULT);
                    }
                }

                return Ok(found_pid as isize);
            }

            // 3) 有目标子进程，但目前没有 zombie
            if options & WNOHANG != 0 {
                return Ok(0);
            }

            // 4) 阻塞等待；这里必须先释放 inner，再睡眠
            drop(inner);

            process
                .wait_exit_queue
                .wait_with_reason_or_skip(WaitReason::ProcessWaitExit, || {
                    let inner = process.inner_exclusive_access();
                    let has_target_child = inner
                        .children
                        .iter()
                        .any(|p| pid == -1 || pid as usize == p.getpid());
                    let has_target_zombie = inner.children.iter().any(|p| {
                        let p_inner = p.inner_exclusive_access();
                        p_inner.is_zombie && (pid == -1 || pid as usize == p.getpid())
                    });
                    !has_target_child || has_target_zombie
                });
        }
    })
}

/// kill syscall
pub fn sys_kill(pid: usize, signal: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_kill",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let process = pid2process(pid).or_errno(ERRNO::ESRCH)?;
        let flag = SignalFlags::from_signum(signal).or_errno(ERRNO::EINVAL)?;
        process.inner_exclusive_access().pending_signals |= flag;
        Ok(0)
    })
}

/// sigaction 系统调用
///
/// 为当前进程的指定信号安装/读取用户态处理动作。
/// 当前仅完成动作表的存取与基础参数校验，还没有把用户态 handler
/// 真正接入 trap 返回路径。
pub fn sys_sigaction(
    signum: i32,
    action: *const SignalAction,
    old_action: *mut SignalAction,
) -> isize {
    syscall_body!({
        let signum = signum as u32;
        if signum == 0 || signum as usize > crate::task::MAX_SIG {
            return Err(ERRNO::EINVAL);
        }
        if signum == 9 || signum == 19 {
            return Err(ERRNO::EINVAL);
        }
        let token = current_user_token();
        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        let slot = &mut inner.signal_actions.table[signum as usize];
        if !old_action.is_null() {
            let old = translated_refmut(token, old_action).or_errno(ERRNO::EFAULT)?;
            *old = *slot;
        }
        if !action.is_null() {
            // TODO: 接入用户态 signal handler 分发后，需要在这里补充
            // 对 handler/mask 组合语义的进一步约束校验。
            let new_action = *translated_ref(token, action).or_errno(ERRNO::EFAULT)?;
            *slot = new_action;
        }
        Ok(0)
    })
}

/// sigprocmask 系统调用
///
/// 设置当前进程的信号屏蔽字。当前实现采用“整值覆盖”语义，
/// 尚未支持 Linux 中的 SIG_BLOCK / SIG_UNBLOCK / SIG_SETMASK 三种模式。
pub fn sys_sigprocmask(mask: u32) -> isize {
    syscall_body!({
        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        // TODO: 若后续补齐完整 Linux 语义，这里应改为根据 how 参数
        // 对旧 mask 做增量更新，并向用户态返回旧值。
        inner.signal_mask = SignalFlags::from_bits(mask).or_errno(ERRNO::EINVAL)?;
        Ok(0)
    })
}

/// sigreturn 系统调用
///
/// 供用户态 signal handler 返回内核并恢复被中断现场。
/// 当前仅保留 syscall 框架，尚未实现 signal frame / trap context 恢复。
pub fn sys_sigreturn() -> isize {
    syscall_body!({
        // TODO: 实现用户态 signal frame 恢复，包括 trap context、
        // 屏蔽字与正在处理信号状态的回滚。
        Err(ERRNO::ENOSYS)?
    })
}

/// change data segment size
pub fn sys_brk(addr: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_brk",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    current_process().set_program_brk(addr) as isize
}

/// spawn syscall
pub fn sys_spawn(_path: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_spawn",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let path = translated_str(token, _path).or_errno(ERRNO::EFAULT)?;
        let app_inode = open_file(path.as_str(), OpenFlags::RDONLY).or_errno(ERRNO::ENOENT)?;
        let parent = current_process();
        let all_data = app_inode.read_all();
        let child = parent.spawn(all_data.as_slice()).or_errno(ERRNO::ENOEXEC)?;
        Ok(child.getpid() as isize)
    })
}

/// uname syscall
#[repr(C)]
#[derive(Debug, Clone)]
pub struct UtsName {
    pub sysname: [u8; 65],
    pub nodename: [u8; 65],
    pub release: [u8; 65],
    pub version: [u8; 65],
    pub machine: [u8; 65],
}

impl Pod for UtsName {}

impl UtsName {
    pub fn new() -> Self {
        // 按照 Linux 标准填充字段，可以根据实际情况修改
        let mut uname = UtsName {
            sysname: [0; 65],
            nodename: [0; 65],
            release: [0; 65],
            version: [0; 65],
            machine: [0; 65],
        };
        let sysname = b"xxOS";
        let nodename = b"xxNode";
        let release = b"0.1";
        let version = b"xxOS version 0.1";
        let machine = b"riscv64";
        uname.sysname[..sysname.len()].copy_from_slice(sysname);
        uname.nodename[..nodename.len()].copy_from_slice(nodename);
        uname.release[..release.len()].copy_from_slice(release);
        uname.version[..version.len()].copy_from_slice(version);
        uname.machine[..machine.len()].copy_from_slice(machine);
        uname
    }
}

/// uname syscall
pub fn sys_uname(utsname_ptr: *mut UtsName) -> isize {
    trace!(
        "kernel:pid[{}] sys_uname",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let uname = UtsName::new();
        write_pod_to_user(utsname_ptr, &uname)?;
        Ok(0)
    })
}

/// get_robust_list syscall
pub fn sys_get_robust_list(pid: i32, head_ptr: *mut usize, len_ptr: *mut usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_get_robust_list",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let process = pid2process(pid as usize).or_errno(ERRNO::ESRCH)?;
        let robust_list = &process.inner_exclusive_access().robust_list;
        let token = current_user_token();
        if !head_ptr.is_null() {
            translated_refmut(token, head_ptr).map(|slot| *slot = robust_list.head).ok_or(ERRNO::EFAULT)?;
        }
        if !len_ptr.is_null() {
            translated_refmut(token, len_ptr).map(|slot| *slot = robust_list.len).ok_or(ERRNO::EFAULT)?;
        }
        Ok(0)
    })
}

/// set_robust_list syscall
pub fn sys_set_robust_list(head: usize, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_set_robust_list",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        inner.robust_list.head = head;
        inner.robust_list.len = len;
        Ok(0)
    })
}

/// set priority syscall
///
/// YOUR JOB: Set task priority
pub fn sys_set_priority(_prio: isize) -> isize {
    trace!(
        "kernel:pid[{}] sys_set_priority NOT IMPLEMENTED",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    -1
}
