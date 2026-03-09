use crate::{
    config::PAGE_SIZE_BITS,
    fs::{open_file, open_file_at, OpenFlags},
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
/// 解析用户态传入的 `char **`（以 NULL 结尾）为 Rust 字符串数组。
fn parse_user_cstr_array(token: usize, mut arr: *const usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    loop {
        let str_ptr = *translated_ref(token, arr);
        if str_ptr == 0 {
            break;
        }
        out.push(translated_str(token, str_ptr as *const u8));
        // SAFETY: 逐个读取用户态指针数组元素，直到遇到 NULL 结束。
        unsafe {
            arr = arr.add(1);
        }
    }
    out
}

/// sys_execve
pub fn sys_execve(path: *const u8, args: *const usize, envp: *const usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_execve",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    let path = translated_str(token, path);
    let args_vec = parse_user_cstr_array(token, args);

    // 当前内核尚未实现进程环境变量表，这里先完成 ABI 级别的解析与校验。
    let _envp_vec = parse_user_cstr_array(token, envp);

    let process = current_process();
    // 关键：execve 路径解析与 open/chdir 统一，支持相对路径与绝对路径。
    let cwd = process.inner_exclusive_access().cwd.clone();
    if let Some(app_inode) = open_file_at(cwd.as_str(), path.as_str(), OpenFlags::RDONLY) {
        let all_data = app_inode.read_all();
        let argc = args_vec.len();
        process.exec(all_data.as_slice(), args_vec);
        // trap 返回路径会覆盖 a0，这里返回 argc 以保持新程序入口参数正确。
        argc as isize
    } else {
        -1
    }
}

/// waitpid syscall
///
/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    //trace!("kernel: sys_waitpid");
    let process = current_process();
    // find a child process

    let mut inner = process.inner_exclusive_access();
    if !inner
        .children
        .iter()
        .any(|p| pid == -1 || pid as usize == p.getpid())
    {
        return -1;
        // ---- release current PCB
    }
    let pair = inner.children.iter().enumerate().find(|(_, p)| {
        // ++++ temporarily access child PCB exclusively
        p.inner_exclusive_access().is_zombie && (pid == -1 || pid as usize == p.getpid())
        // ++++ release child PCB
    });
    if let Some((idx, _)) = pair {
        let child = inner.children.remove(idx);
        // confirm that child will be deallocated after being removed from children list
        assert_eq!(Arc::strong_count(&child), 1);
        let found_pid = child.getpid();
        // ++++ temporarily access child PCB exclusively
        let exit_code = child.inner_exclusive_access().exit_code;
        // ++++ release child PCB
        *translated_refmut(inner.memory_set.token(), exit_code_ptr) = exit_code;
        found_pid as isize
    } else {
        -2
    }
    // ---- release current PCB automatically
}

/// kill syscall
pub fn sys_kill(pid: usize, signal: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_kill",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    if let Some(process) = pid2process(pid) {
        if let Some(flag) = SignalFlags::from_bits(signal) {
            process.inner_exclusive_access().signals |= flag;
            0
        } else {
            -1
        }
    } else {
        -1
    }
}

/// get_time syscall
///
/// YOUR JOB: get time with second and microsecond
/// HINT: You might reimplement it with virtual memory management.
/// HINT: What if [`TimeVal`] is splitted by two pages ?
pub fn sys_get_time(_ts: *mut TimeVal, _tz: usize) -> isize {
    trace!("kernel:pid[{}] sys_get_time",current_task().unwrap().process.upgrade().unwrap().getpid());
    let time_us = get_time_us();
    let timeval = TimeVal {
        sec: time_us / 1_000_000,
        usec: time_us % 1_000_000,
    };
    let timeval_bytes = unsafe {
        slice::from_raw_parts(&timeval as *const TimeVal as *const u8, size_of::<TimeVal>())
    };
    let mut buffers = translated_byte_buffer(current_user_token(), _ts as *const u8, size_of::<TimeVal>());
    let mut copied = 0usize;
    for buffer in buffers.iter_mut() {
        let len = buffer.len();
        buffer.copy_from_slice(&timeval_bytes[copied..copied + len]);
        copied += len;
    }
    0
}

/// mmap syscall
///
/// YOUR JOB: Implement mmap.
pub fn sys_mmap(_start: usize, _len: usize, _port: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_mmap",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );

    if _start & ((1 << PAGE_SIZE_BITS) - 1) != 0 {
        return -1;
    }
    if _port & !0x7 != 0 {
        return -1;
    }
    if _port & 0x7 == 0 {
        return -1;
    }

    if _len == 0 {
        return -1;
        //这里对于错误类型其实还没有文件去规范，理应该有一个专门
        //的错误类型来区分不同的错误，但现在先简单地返回-1
    }

    let Some(end) = _start.checked_add(_len) else {
        return -1;
    };

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
        0
    } else {
        -1
    }
}

/// munmap syscall
///
/// YOUR JOB: Implement munmap.
pub fn sys_munmap(_start: usize, _len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_munmap",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    if _start & ((1 << PAGE_SIZE_BITS) - 1) != 0 {
        return -1;
    }

    if _len == 0 {
        return -1;
        //这里对于错误类型其实还没有文件去规范，理应该有一个专门的错误类
        //型来区分不同的错误，但现在先简单地返回-1
    }

    let Some(end) = _start.checked_add(_len) else {
        return -1;
    };
    if munmap_current_process(VirtAddr::from(_start), VirtAddr::from(end)) {
        0
    } else {
        -1
    }
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
/// YOUR JOB: Implement spawn.
/// HINT: fork + exec =/= spawn
pub fn sys_spawn(_path: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_spawn",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );

    let token = current_user_token();
    let path = translated_str(token, _path);
    if let Some(app_inode) = open_file(path.as_str(), OpenFlags::RDONLY) {
        let parent = current_process();
        let all_data = app_inode.read_all();
        let child = parent.spawn(all_data.as_slice());
        child.getpid() as isize
    } else {
        -1
    }
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
