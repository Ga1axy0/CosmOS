use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall_body;
use crate::{
    config::PAGE_SIZE_BITS,
    fs::{open_file, open_file_at, File, OpenFlags},
    mm::{
        translated_byte_buffer, translated_ref, translated_refmut, translated_str, MapPermission,
        VirtAddr,
    },
    task::{
        current_process, current_task, current_user_token, exit_current_and_run_next,
        mmap_current_process, munmap_current_process, pid2process, suspend_current_and_run_next,
        SignalFlags,
    },
    timer::get_time_us,
};

use alloc::{string::String, sync::Arc, vec::Vec};
use core::mem::size_of;
use core::slice;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

/// exit syscall
///
/// exit the current task and run the next task in task list
pub fn sys_exit(exit_code: i32) -> ! {
    trace!(
        "kernel:pid[{}] sys_exit",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
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

/// waitpid syscall
///
/// If there is not a child process whose pid is same as given, return -ECHILD.
/// Else if there is a child process but it is still running, return -EAGAIN.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    trace!("kernel: sys_waitpid");
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    syscall_body!({
        // no matching child at all
        if !inner
            .children
            .iter()
            .any(|p| pid == -1 || pid as usize == p.getpid())
        {
            return Err(ERRNO::ECHILD);
        }
        let pair = inner.children.iter().enumerate().find(|(_, p)| {
            p.inner_exclusive_access().is_zombie && (pid == -1 || pid as usize == p.getpid())
        });
        if let Some((idx, _)) = pair {
            let child = inner.children.remove(idx);
            assert_eq!(Arc::strong_count(&child), 1);
            let found_pid = child.getpid();
            let exit_code = child.inner_exclusive_access().exit_code;
            // write exit code into user space; bad pointer → EFAULT
            if !exit_code_ptr.is_null() {
                if let Some(slot) = translated_refmut(inner.memory_set.token(), exit_code_ptr) {
                    *slot = exit_code;
                } else {
                    return Err(ERRNO::EFAULT);
                }
            }
            Ok(found_pid as isize)
        } else {
            // child exists but not yet zombie
            Err(ERRNO::EAGAIN)
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
        let flag = SignalFlags::from_bits(signal).or_errno(ERRNO::EINVAL)?;
        process.inner_exclusive_access().signals |= flag;
        Ok(0)
    })
}

/// get_time syscall
pub fn sys_get_time(_ts: *mut TimeVal, _tz: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_get_time",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let time_us = get_time_us();
        let timeval = TimeVal {
            sec: time_us / 1_000_000,
            usec: time_us % 1_000_000,
        };
        let timeval_bytes = unsafe {
            slice::from_raw_parts(
                &timeval as *const TimeVal as *const u8,
                size_of::<TimeVal>(),
            )
        };
        let mut buffers =
            translated_byte_buffer(current_user_token(), _ts as *const u8, size_of::<TimeVal>())
                .or_errno(ERRNO::EFAULT)?;
        let mut copied = 0usize;
        for buffer in buffers.iter_mut() {
            let len = buffer.len();
            buffer.copy_from_slice(&timeval_bytes[copied..copied + len]);
            copied += len;
        }
        Ok(0)
    })
}

/// mmap syscall
pub fn sys_mmap(_start: usize, _len: usize, _port: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_mmap",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        if _start & ((1 << PAGE_SIZE_BITS) - 1) != 0 {
            return Err(ERRNO::EINVAL); // start not page-aligned
        }
        if _port & !0x7 != 0 {
            return Err(ERRNO::EINVAL); // unknown permission bits
        }
        if _port & 0x7 == 0 {
            return Err(ERRNO::EINVAL); // no access at all is meaningless
        }
        if _len == 0 {
            return Err(ERRNO::EINVAL);
        }
        let end = _start.checked_add(_len).ok_or(ERRNO::EINVAL)?;

        let mut perm = MapPermission::U;
        if _port & 0x1 != 0 {
            perm |= MapPermission::R;
        }
        if _port & 0x2 != 0 {
            perm |= MapPermission::W;
        }
        if _port & 0x4 != 0 {
            perm |= MapPermission::X;
        }

        if mmap_current_process(VirtAddr::from(_start), VirtAddr::from(end), perm) {
            Ok(0)
        } else {
            Err(ERRNO::ENOMEM)
        }
    })
}

/// munmap syscall
pub fn sys_munmap(_start: usize, _len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_munmap",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        if _start & ((1 << PAGE_SIZE_BITS) - 1) != 0 {
            return Err(ERRNO::EINVAL); // start not page-aligned
        }
        if _len == 0 {
            return Err(ERRNO::EINVAL);
        }
        let end = _start.checked_add(_len).ok_or(ERRNO::EINVAL)?;
        if munmap_current_process(VirtAddr::from(_start), VirtAddr::from(end)) {
            Ok(0)
        } else {
            Err(ERRNO::EINVAL) // range not fully mapped as anonymous
        }
    })
}

/// change data segment size
// pub fn sys_sbrk(size: i32) -> isize {
//     trace!("kernel:pid[{}] sys_sbrk", current_task().unwrap().process.upgrade().unwrap().getpid());
//     if let Some(old_brk) = current_task().unwrap().change_program_brk(size) {
//         old_brk as isize
//     } else {
//     -1
// }

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
