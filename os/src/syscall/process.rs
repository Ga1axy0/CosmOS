use crate::mm::{MapPermission, USER_SPACE_END, VirtAddr};
use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall::{translated_byte_buffer_with_access, write_pod_to_user, Pod};
use crate::syscall_body;
use crate::task::yield_current_and_run_next;
use crate::timer::get_time_ns;
use crate::{
    fs::{canonicalize, open_file, open_file_at, File, OpenFlags},
    hart::hartid,
    ipc::{self, IPC_RMID},
    mm::{translated_ref, translated_str, PageFaultAccess},
    task::{
        current_process, current_task, current_trap_cx, current_user_token,
        exit_current_and_run_next, exit_group_current_and_run_next, thread_id2task, ExitReason,
        ProcessControlBlock, ShmAttachment, SigInfo, SignalBit, WaitReason,
    },
};
use crate::sched::{add_task, list_pids, pid2process, remove_from_pid2process};

use alloc::{string::String, sync::Arc, vec, vec::Vec};
/// `execve` 在解析脚本后得到的最终执行目标。
struct ResolvedExecImage {
    /// 最终需要交给 ELF 装载器处理的字节内容。
    elf_data: Vec<u8>,
    /// 按 shebang 规则重写后的参数列表。
    argv: Vec<String>,
    /// 最终执行映像的绝对路径。
    exec_path: String,
}

/// shebang 首行解析结果。
struct ShebangInfo {
    /// 解释器的绝对路径。
    interpreter: String,
    /// shebang 中附带的单个可选参数。
    optional_arg: Option<String>,
}

/// 允许脚本解释器递归重写的最大层数，避免循环依赖。
const EXEC_INTERPRETER_MAX_DEPTH: usize = 4;
/// `execve` 探测文件类型时预读的前缀长度。
const EXEC_PROBE_SIZE: usize = 256;
/// ELF 文件头魔数。
const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
/// 判断当前文件是否为 ELF 映像。
fn is_elf_image(file_data: &[u8]) -> bool {
    file_data.starts_with(ELF_MAGIC)
}

/// 解析脚本首行 shebang，提取解释器路径和附加参数。
fn parse_shebang_line(file_data: &[u8]) -> Result<Option<ShebangInfo>, ERRNO> {
    if !file_data.starts_with(b"#!") {
        return Ok(None);
    }

    let line_end = file_data
        .iter()
        .position(|&ch| ch == b'\n')
        .unwrap_or(file_data.len());
    let line = core::str::from_utf8(&file_data[2..line_end]).or_errno(ERRNO::ENOEXEC)?;
    let line = line.strip_suffix('\r').unwrap_or(line);
    let line = line.trim_matches(|ch| ch == ' ' || ch == '\t');
    if line.is_empty() {
        return Err(ERRNO::ENOEXEC);
    }

    let mut parts = line.splitn(2, |ch: char| ch == ' ' || ch == '\t');
    let interpreter = parts.next().unwrap();
    if interpreter.is_empty() {
        return Err(ERRNO::ENOEXEC);
    }
    // TODO: 当前仅支持 shebang 中的单个附加参数，不处理引号和转义。
    let optional_arg = parts
        .next()
        .map(|rest| rest.trim_matches(|ch| ch == ' ' || ch == '\t'))
        .filter(|rest| !rest.is_empty())
        .map(String::from);

    Ok(Some(ShebangInfo {
        interpreter: String::from(interpreter),
        optional_arg,
    }))
}

/// 解析 `execve` 目标，必要时按 shebang 规则递归展开到最终 ELF。
fn resolve_exec_image(
    cwd: &str,
    path: &str,
    argv: Vec<String>,
    depth: usize,
) -> Result<ResolvedExecImage, ERRNO> {
    debug!("Resolving exec image: path='{}', argv={:?}, depth={}", path, argv, depth);

    if depth >= EXEC_INTERPRETER_MAX_DEPTH {
        return Err(ERRNO::ELOOP);
    }

    let abs_path = canonicalize(cwd, path);
    let inode = open_file_at(cwd, path, OpenFlags::RDONLY).or_errno(ERRNO::ENOENT)?;
    if inode.is_dir() {
        return Err(ERRNO::EISDIR);
    }

    // 先仅读取首行，避免在 shebang 脚本路径上无谓地把整个文件搬进内核内存。
    let (first_line, first_line_complete) = inode.read_first_line_limited(EXEC_PROBE_SIZE);
    debug!(
        "First line of exec target: {:?}, complete={}",
        core::str::from_utf8(&first_line).unwrap_or("<invalid utf-8>"),
        first_line_complete
    );
    if is_elf_image(&first_line) {
        let file_data = inode.read_all();
        return Ok(ResolvedExecImage {
            elf_data: file_data,
            argv,
            exec_path: abs_path,
        });
    }

    // 首行超过限制时直接拒绝，避免 shebang 解析继续处理不完整输入。
    if !first_line_complete {
        return Err(ERRNO::ENOEXEC);
    }

    if let Some(shebang) = parse_shebang_line(&first_line)? {
        // shebang 语义要求解释器路径必须是绝对路径。
        if !shebang.interpreter.starts_with('/') {
            return Err(ERRNO::ENOEXEC);
        }

        // 按 Linux 语义重写 argv：解释器、可选参数、脚本绝对路径、原 argv[1..]。
        let mut next_argv = Vec::with_capacity(argv.len() + 2);
        next_argv.push(shebang.interpreter.clone());
        if let Some(optional_arg) = shebang.optional_arg {
            next_argv.push(optional_arg);
        }
        next_argv.push(abs_path);
        next_argv.extend(argv.into_iter().skip(1));
        return resolve_exec_image(cwd, shebang.interpreter.as_str(), next_argv, depth + 1);
    }

    Err(ERRNO::ENOEXEC)
}
/// exit syscall
///
/// exit the current task and run the next task in task list
pub fn sys_exit(exit_code: i32) -> ! {
    trace!(
        "kernel:pid[{}] sys_exit - time {}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        get_time_ns()
    );
    exit_current_and_run_next(ExitReason::Exit(exit_code));
    panic!("Unreachable in sys_exit!");
}

/// 临时实现
pub fn sys_exit_group(exit_code: i32) -> ! {
    trace!(
        "kernel:pid[{}] sys_exit_group - time {}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        get_time_ns()
    );
    exit_group_current_and_run_next(ExitReason::Exit(exit_code));
    panic!("Unreachable in sys_exit_group!");
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

/// getuid syscall
pub fn sys_getuid() -> isize {
    let process = current_process();
    trace!("kernel: sys_getuid pid:{}", process.getpid());
    process.getuid() as isize
}

/// geteuid syscall
pub fn sys_geteuid() -> isize {
    let process = current_process();
    trace!("kernel: sys_geteuid pid:{}", process.getpid());
    process.geteuid() as isize
}

/// getgid syscall
pub fn sys_getgid() -> isize {
    let process = current_process();
    trace!("kernel: sys_getgid pid:{}", process.getpid());
    process.getgid() as isize
}

/// getegid syscall
pub fn sys_getegid() -> isize {
    let process = current_process();
    trace!("kernel: sys_getegid pid:{}", process.getpid());
    process.getegid() as isize
}

/// umask syscall
pub fn sys_umask(mask: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_umask",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let process = current_process();
        let old = process.umask();
        process.set_umask(mask as u32);
        Ok(old as isize)
    })
}

/// SysV `shmget`.
pub fn sys_shmget(key: i32, size: usize, flag: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_shmget",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let shmid = ipc::shmget(key, size, flag)?;
        Ok(shmid as isize)
    })
}

/// SysV `shmat`.
pub fn sys_shmat(shmid: usize, shmaddr: usize, shmflg: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_shmat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        if shmflg != 0 {
            return Err(ERRNO::ENOSYS);
        }
        let segment = ipc::attach_segment(shmid)?;
        let (size, desc) = {
            let seg = segment.lock();
            (seg.size, Arc::clone(&seg.desc))
        };
        let len_aligned = size
            .checked_add(crate::config::PAGE_SIZE - 1)
            .ok_or(ERRNO::EOVERFLOW)?
            & !(crate::config::PAGE_SIZE - 1);
        let process = current_process();
        let map_addr = if shmaddr == 0 {
            let (chosen, chosen_end) = {
                let mut inner = process.inner_exclusive_access();
                inner.ensure_address_space_capacity(len_aligned)?;
                let hint = inner.vm_layout.mmap_hint;
                let base = inner.vm_layout.mmap_base;
                let chosen = inner
                    .memory_set
                    .find_free_mmap_area(hint, base, len_aligned)
                    .ok_or(ERRNO::ENOMEM)?;
                let chosen_end = chosen.checked_add(len_aligned).ok_or(ERRNO::EOVERFLOW)?;
                let mapped = inner.memory_set.mmap_file(
                    crate::mm::VirtAddr::from(chosen),
                    crate::mm::VirtAddr::from(chosen_end),
                    crate::mm::MapPermission::R | crate::mm::MapPermission::W | crate::mm::MapPermission::U,
                    Arc::clone(&desc),
                    0,
                    true,
                );
                mapped.map_err(|_| ERRNO::ENOMEM)?;
                inner.vm_layout.mmap_hint = chosen_end;
                (chosen, chosen_end)
            };
            if let Some(inode) = desc.backing_inode() {
                crate::mm::register_file_mapping(&inode, &process);
            }
            let _ = chosen_end;
            chosen
        } else {
            if !shmaddr.is_multiple_of(crate::config::PAGE_SIZE) {
                ipc::detach_segment(shmid);
                return Err(ERRNO::EINVAL);
            }
            let end = shmaddr.checked_add(len_aligned).ok_or(ERRNO::EOVERFLOW)?;
            if shmaddr >= USER_SPACE_END || end > USER_SPACE_END {
                ipc::detach_segment(shmid);
                return Err(ERRNO::ENOMEM);
            }
            let ok = process.mmap_file(
                VirtAddr::from(shmaddr),
                VirtAddr::from(end),
                MapPermission::R | MapPermission::W | MapPermission::U,
                desc,
                0,
                true,
            );
            if let Err(err) = ok {
                ipc::detach_segment(shmid);
                return Err(err);
            }
            shmaddr
        };
        process.add_shm_attachment(ShmAttachment {
            shmid,
            addr: map_addr,
            size,
        });
        Ok(map_addr as isize)
    })
}

/// SysV `shmdt`.
pub fn sys_shmdt(shmaddr: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_shmdt",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let process = current_process();
        let attachment = process
            .remove_shm_attachment_by_addr(shmaddr)
            .ok_or(ERRNO::EINVAL)?;
        if !process.munmap(
            crate::mm::VirtAddr::from(attachment.addr),
            crate::mm::VirtAddr::from(attachment.addr.checked_add(attachment.size).ok_or(ERRNO::EOVERFLOW)?),
        ) {
            process.add_shm_attachment(attachment);
            return Err(ERRNO::EINVAL);
        }
        ipc::detach_segment(attachment.shmid);
        Ok(0)
    })
}

/// SysV `shmctl`.
pub fn sys_shmctl(shmid: usize, cmd: i32, _buf: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_shmctl",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        match cmd {
            IPC_RMID => {
                ipc::mark_segment_for_removal(shmid)?;
                Ok(0)
            }
            _ => Err(ERRNO::ENOSYS),
        }
    })
}

pub fn sys_getsid() -> isize {
    let process = current_process();
    trace!("kernel: sys_getsid pid:{}", process.getpid());
    process.getsid() as isize
}

pub fn sys_getpgid(pid: isize) -> isize {
    trace!("kernel: sys_getpgid pid:{}", current_process().getpid());
    syscall_body!({
        if pid < 0 {
            return Err(ERRNO::EINVAL);
        }
        let process = if pid == 0 { current_process() } else {
            pid2process(pid as usize).or_errno(ERRNO::ESRCH)?
        };
        Ok(process.getpgid() as isize)
    })
}

pub fn sys_setpgid(pid: isize, pgid: isize) -> isize {
    trace!("kernel: sys_setpgid pid:{}", current_process().getpid());
    syscall_body!({
        if pid < 0 || pgid < 0 {
            return Err(ERRNO::EINVAL);
        }

        let current = current_process();
        let current_pid = current.getpid();
        let target = if pid == 0 { current.clone() } else {
            pid2process(pid as usize).or_errno(ERRNO::ESRCH)?
        };
        let target_pid = target.getpid();

        if target_pid != current_pid {
            let parent_pid = target
                .inner_exclusive_access()
                .parent
                .clone()
                .and_then(|parent| parent.upgrade())
                .map(|parent| parent.getpid());
            if parent_pid != Some(current_pid) {
                return Err(ERRNO::EPERM);
            }
        }

        let target_sid = target.getsid();
        if target_sid != current.getsid() {
            return Err(ERRNO::EPERM);
        }

        if target_pid as u32 == target_sid {
            return Err(ERRNO::EPERM);
        }

        let new_pgid = if pgid == 0 {
            target_pid
        } else {
            pgid as usize
        };
        if new_pgid > u32::MAX as usize {
            return Err(ERRNO::EINVAL);
        }

        if new_pgid != target_pid {
            let mut found = false;
            for pid in list_pids() {
                if let Some(process) = pid2process(pid) {
                    if process.getsid() == target_sid && process.getpgid() as usize == new_pgid {
                        found = true;
                        break;
                    }
                }
            }
            if !found {
                return Err(ERRNO::EINVAL);
            }
        }

        target.setpgid(new_pgid as u32);
        Ok(0)
    })
}

pub fn sys_setsid() -> isize {
    trace!("kernel: sys_setsid pid:{}", current_process().getpid());
    syscall_body!({
        let process = current_process();
        let pid = process.getpid() as u32;
        if process.getpgid() == pid {
            return Err(ERRNO::EPERM);
        }
        process.setsid(pid);
        process.setpgid(pid);
        Ok(pid as isize)
    })
}

/// `clone` 支持的退出信号：当前仅实现 basic 测试需要的 SIGCHLD。
const CLONE_EXIT_SIGNAL_SIGCHLD: usize = 17;
/// Linux clone flags 中低 8 位保存退出信号。
const CLONE_EXIT_SIGNAL_MASK: usize = 0xff;

bitflags! {
    /// Linux `clone` 的功能标志位，不包含低 8 位退出信号。
    struct CloneFlags: usize {
        /// 共享地址空间标志，当前暂不支持。
        const CLONE_VM = 0x0000_0100;
        /// 共享文件系统上下文标志，当前暂不支持。
        const CLONE_FS = 0x0000_0200;
        /// 共享文件描述符表标志，当前暂不支持。
        const CLONE_FILES = 0x0000_0400;
        /// 共享信号处理表标志，当前暂不支持。
        const CLONE_SIGHAND = 0x0000_0800;
        /// vfork 语义标志，当前暂不支持。
        const CLONE_VFORK = 0x0000_4000;
        /// 线程组标志，当前暂不支持。
        const CLONE_THREAD = 0x0001_0000;
        /// 共享 SysV semaphore undo 状态；当前没有 SysV semaphore，线程路径中按 no-op 处理。
        const CLONE_SYSVSEM = 0x0004_0000;
        /// 设置子任务 TLS 指针，RISC-V 上对应 tp(x4)。
        const CLONE_SETTLS = 0x0008_0000;
        /// 在父地址空间写入子 tid。
        const CLONE_PARENT_SETTID = 0x0010_0000;
        /// 子任务退出时清理 child_tid。
        const CLONE_CHILD_CLEARTID = 0x0020_0000;
        /// 历史遗留标志，Linux 已忽略；这里也按 no-op 处理。
        const CLONE_DETACHED = 0x0040_0000;
        /// 在子地址空间写入子 tid。
        const CLONE_CHILD_SETTID = 0x0100_0000;
    }
}

/// Linux `clone` syscall。
///
/// 当前支持 fork-like 进程创建，以及 musl pthread 使用的 CLONE_VM 线程创建子集。
pub fn sys_clone(
    flags: usize,
    stack: usize,
    parent_tid: usize,
    tls: usize,
    child_tid: usize,
) -> isize {
    syscall_body!({
        let clone_flags_arg = flags;
        trace!(
            "kernel:pid[{}] sys_clone flags={:#x} stack={:#x}",
            current_task().unwrap().process.upgrade().unwrap().getpid(),
            flags,
            stack,
        );
        debug!(
            "kernel: sys_clone enter flags={:#x} parent_tid={:#x} child_tid={:#x}",
            clone_flags_arg,
            parent_tid,
            child_tid
        );

        let exit_signal = flags & CLONE_EXIT_SIGNAL_MASK;
        let raw_clone_flags = flags & !CLONE_EXIT_SIGNAL_MASK;
        let flags = CloneFlags::from_bits_truncate(raw_clone_flags);
        let unsupported_flags = raw_clone_flags & !CloneFlags::all().bits();
        if unsupported_flags != 0 {
            warn!(
                "kernel: sys_clone unknown clone flags {:#x}",
                unsupported_flags
            );
            return Err(ERRNO::EINVAL);
        }
        if exit_signal != 0 && exit_signal != CLONE_EXIT_SIGNAL_SIGCHLD {
            warn!(
                "kernel: sys_clone unsupported exit signal {}, only SIGCHLD/0 is implemented",
                exit_signal
            );
            return Err(ERRNO::EINVAL);
        }
        let thread_clone = flags.contains(CloneFlags::CLONE_VM);
        if thread_clone && !flags.contains(CloneFlags::CLONE_THREAD) {
            warn!("kernel: sys_clone CLONE_VM without CLONE_THREAD is unsupported");
            return Err(ERRNO::EINVAL);
        }
        if !thread_clone && flags.contains(CloneFlags::CLONE_SYSVSEM) {
            warn!("kernel: sys_clone unsupported process flag CLONE_SYSVSEM");
            return Err(ERRNO::EINVAL);
        }
        if !thread_clone && flags.contains(CloneFlags::CLONE_FS) {
            warn!("kernel: sys_clone unsupported flag CLONE_FS");
            return Err(ERRNO::EINVAL);
        }
        if !thread_clone && flags.contains(CloneFlags::CLONE_FILES) {
            warn!("kernel: sys_clone unsupported flag CLONE_FILES");
            return Err(ERRNO::EINVAL);
        }
        if !thread_clone && flags.contains(CloneFlags::CLONE_SIGHAND) {
            warn!("kernel: sys_clone unsupported flag CLONE_SIGHAND");
            return Err(ERRNO::EINVAL);
        }
        if flags.contains(CloneFlags::CLONE_VFORK) {
            warn!("kernel: sys_clone unsupported flag CLONE_VFORK");
            return Err(ERRNO::EINVAL);
        }
        if !thread_clone && flags.contains(CloneFlags::CLONE_THREAD) {
            warn!("kernel: sys_clone unsupported flag CLONE_THREAD");
            return Err(ERRNO::EINVAL);
        }
        if flags.contains(CloneFlags::CLONE_CHILD_CLEARTID) && child_tid == 0 {
            warn!("kernel: sys_clone CLONE_CHILD_CLEARTID with null child_tid");
            return Err(ERRNO::EFAULT);
        }
        let mut parent_set_tid = None;
        let mut child_set_tid = None;
        if flags.contains(CloneFlags::CLONE_PARENT_SETTID) {
            if parent_tid == 0 {
                warn!("kernel: sys_clone CLONE_PARENT_SETTID with null parent_tid");
                return Err(ERRNO::EFAULT);
            }
            parent_set_tid = Some(parent_tid);
        }
        if flags.contains(CloneFlags::CLONE_CHILD_SETTID) {
            if child_tid == 0 {
                warn!("kernel: sys_clone CLONE_CHILD_SETTID with null child_tid");
                return Err(ERRNO::EFAULT);
            }
            child_set_tid = Some(child_tid);
        }
        let mut child_tls = None;
        if flags.contains(CloneFlags::CLONE_SETTLS) {
            debug!("kernel: sys_clone set child TLS to {:#x}", tls);
            child_tls = Some(tls);
        }
        let current_process = current_process();
        if thread_clone {
            let parent_task = current_task().unwrap();
            let (ustack_base, sched_attr, affinity_mask, signal_mask) = {
                let inner = parent_task.inner_exclusive_access();
                (
                    inner.res.as_ref().unwrap().ustack_base(),
                    inner.sched_attr(),
                    inner.sched.cpu_affinity_mask,
                    inner.signal_mask,
                )
            };
            let inherited_cx = *current_trap_cx();
            let new_task = current_process
                .create_task(ustack_base, true, sched_attr)
                .map_err(|_| ERRNO::ENOMEM)?;
            let new_tid = new_task.inner_exclusive_access().res.as_ref().unwrap().thread_id() as i32;
            let new_inner_tid = new_task.inner_exclusive_access().res.as_ref().unwrap().tid;
            debug!(
                "kernel: sys_clone thread: new_tid(thread_id)={} inner_tid={} parent_set_tid_addr={:#x} child_set_tid_addr={:#x} clear_child_tid_addr={:#x}",
                new_tid,
                new_inner_tid,
                parent_set_tid.unwrap_or(0),
                child_set_tid.unwrap_or(0),
                if flags.contains(CloneFlags::CLONE_CHILD_CLEARTID) { child_tid } else { 0 }
            );
            {
                let mut new_inner = new_task.inner_exclusive_access();
                new_inner.sched.cpu_affinity_mask = affinity_mask;
                new_inner.signal_mask = signal_mask;
                if flags.contains(CloneFlags::CLONE_CHILD_CLEARTID) {
                    new_inner.clear_child_tid = child_tid;
                }
                let trap_cx = new_inner.get_trap_cx();
                *trap_cx = inherited_cx;
                trap_cx.kernel_sp = new_task.kstack.get_top();
                trap_cx.x[10] = 0;
                if stack != 0 {
                    trap_cx.x[2] = stack;
                }
                if let Some(tls) = child_tls {
                    trap_cx.x[4] = tls;
                }
                if let Some(ptr) = child_set_tid {
                    write_pod_to_user(ptr as *mut i32, &new_tid)?;
                }
                if let Some(ptr) = parent_set_tid {
                    write_pod_to_user(ptr as *mut i32, &new_tid)?;
                }
            }
            current_process.attach_task(Arc::clone(&new_task));
            add_task(new_task);
            Ok(new_tid as isize)
        } else {
            let new_process = current_process.clone_process(stack, child_tls, child_set_tid)?;
            let child_pid = new_process.getpid() as i32;
            debug!(
                "kernel: sys_clone result flags={:#x} parent_tid={:#x} child_tid={:#x} child_pid={}",
                clone_flags_arg,
                parent_tid,
                child_tid,
                child_pid
            );
            if let Some(ptr) = parent_set_tid {
                write_pod_to_user(ptr as *mut i32, &child_pid)?;
            }
            Ok(child_pid as isize)
        }
    })
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
        let mut envs_vec: Vec<String> = Vec::new();
        loop {
            let env_str_ptr = *translated_ref(token, envp).or_errno(ERRNO::EFAULT)?;
            if env_str_ptr == 0 {
                break;
            }
            envs_vec.push(translated_str(token, env_str_ptr as *const u8).or_errno(ERRNO::EFAULT)?);
            unsafe {
                envp = envp.add(1);
            }
        }

        let process = current_process();
        let cwd = process.inner_exclusive_access().cwd.clone();
        debug!(" ------------------- Resolve -----------------------");
        let resolved = resolve_exec_image(cwd.as_str(), path.as_str(), args_vec, 0)?;
        debug!(" ------------------- End Resolve -----------------------");
        let ResolvedExecImage {
            elf_data,
            argv,
            exec_path,
        } = resolved;
        process.exec(elf_data.as_slice(), argv, envs_vec, exec_path)?;
        // Linux execve succeeds by returning 0 through the trap return path.
        // RISC-V glibc reads argc/argv from the new user stack; a0 is rtld_fini.
        Ok(0)
    })
}

/// `wait4`/`waitpid` 选项位（Linux 语义）。
const WNOHANG: isize = 1;
/// 同时报告已停止的子进程（作业控制）。
const WUNTRACED: isize = 2;
/// 同时报告已继续运行的子进程（作业控制）。
const WCONTINUED: isize = 8;
/// 取走子进程状态但不回收（保留为可再次 wait 的状态）。
const WNOWAIT: isize = 0x0100_0000;
/// Linux 内核内部 clone/线程相关 wait 标志：`__WNOTHREAD | __WALL | __WCLONE`。
const W_INTERNAL_FLAGS: isize = 0x2000_0000 | 0x4000_0000 | 0x8000_0000;
/// `wait4` 可识别的全部选项位。
const WAIT_RECOGNIZED: isize = WNOHANG | WUNTRACED | WCONTINUED | WNOWAIT | W_INTERNAL_FLAGS;

fn snapshot_children_for_wait4(
    process: &Arc<ProcessControlBlock>,
    self_pid: usize,
    label: &'static str,
) -> Vec<Arc<ProcessControlBlock>> {
    loop {
        let child_count = {
            let inner = process.inner_exclusive_access();
            let child_count = inner.children.len();
            child_count
        };

        let mut children = Vec::with_capacity(child_count);
        let inner = process.inner_exclusive_access();
    
        if inner.children.len() > children.capacity() {
            continue;
        }
        for child in inner.children.iter() {
            children.push(Arc::clone(child));
        }
        return children;
    }
}

/// waitpid syscall
///
/// If there is not a child process whose pid is same as given, return -ECHILD.
/// Else if there is a child process but it is still running, return -EAGAIN.
pub fn sys_wait4(pid: isize, exit_status_ptr: *mut i32, options: isize) -> isize {
    trace!("kernel: sys_wait4");
    let process = current_process();
    let self_pid = process.getpid();
    syscall_body!({
        // 只在低 32 位上校验选项，避免符号扩展把 `__WCLONE`(0x80000000) 误判为非法位。
        // `WUNTRACED`/`WCONTINUED` 当前没有额外的停止/继续状态可上报（本内核不实现
        // 作业控制停止），按 Linux 语义安全地忽略即可——这正是 shell 前台等待
        // `waitpid(pid, &status, WUNTRACED)` 所需要的。
        if (options & 0xffff_ffff) & !WAIT_RECOGNIZED != 0 {
            return Err(ERRNO::EINVAL);
        }

        loop {
            let children_snapshot =
                snapshot_children_for_wait4(&process, self_pid, "pid1_wait4");

            // 1) 没有任何匹配的子进程
            let has_target_child = children_snapshot
                .iter()
                .any(|p| pid == -1 || pid as usize == p.getpid());
            if !has_target_child {
                return Err(ERRNO::ECHILD);
            }

            // 2) 查找已经退出的目标子进程
            let zombie_pid = children_snapshot.iter().find_map(|p| {
                let p_inner = p.inner_exclusive_access();
                if p_inner.is_zombie && (pid == -1 || pid as usize == p.getpid()) {
                    Some(p.getpid())
                } else {
                    None
                }
            });

            if let Some(found_pid) = zombie_pid {
                let child = {
                    let mut inner = process.inner_exclusive_access();
                    let Some(idx) = inner.children.iter().position(|p| p.getpid() == found_pid) else {
                        continue;
                    };
                    inner.children.remove(idx)
                };
                debug!(
                    "sys_wait4: found zombie child self_pid={} child_pid={} hart={}",
                    self_pid,
                    found_pid,
                    hartid()
                );
                let (exit_status, add_user_time, add_kernel_time) = {
                    let child_inner = child.inner_exclusive_access();
                    let exit_status = match child_inner.exit_reason {
                        ExitReason::Exit(code) => (code & 0xff) << 8,
                        ExitReason::Signal(signum) => (signum & 0x7f) as i32,
                    };
                    let add_user_time = child_inner
                        .user_time
                        .saturating_add(child_inner.child_user_time);
                    let add_kernel_time = child_inner
                        .kernel_time
                        .saturating_add(child_inner.child_kernel_time);
                    (exit_status, add_user_time, add_kernel_time)
                };
                {
                    let mut inner = process.inner_exclusive_access();
                    inner.child_user_time = inner.child_user_time.saturating_add(add_user_time);
                    inner.child_kernel_time =
                        inner.child_kernel_time.saturating_add(add_kernel_time);
                }
                remove_from_pid2process(found_pid);

                if !exit_status_ptr.is_null() {
                    write_pod_to_user(exit_status_ptr, &exit_status)?;
                }

                return Ok(found_pid as isize);
            }

            // 3) 有目标子进程，但目前没有 zombie
            if options & WNOHANG != 0 {
                return Ok(0);
            }

            if self_pid == 1 {
                let child_states = children_snapshot
                    .iter()
                    .map(|child| {
                        let child_inner = child.inner_exclusive_access();
                        (
                            child.getpid(),
                            child_inner.is_zombie,
                            child_inner.pending_signals.bits(),
                        )
                    })
                    .collect::<Vec<_>>();
                warn!(
                    "sys_wait4: no zombie yet self_pid={} target_pid={} child_states={:?} hart={}",
                    self_pid,
                    pid,
                    child_states,
                    hartid()
                );
            }

            // 4) 阻塞等待
            if self_pid == 1 {
                warn!(
                    "sys_wait4: dropped process lock before wait self_pid={} target_pid={} hart={}",
                    self_pid,
                    pid,
                    hartid()
                );
            }

            process
                .wait_exit_queue
                .wait_with_reason_or_skip(WaitReason::ProcessWaitExit, || {
                    let children_snapshot =
                        snapshot_children_for_wait4(&process, self_pid, "pid1_wait4_predicate");
                    let has_target_child = children_snapshot
                        .iter()
                        .any(|p| pid == -1 || pid as usize == p.getpid());
                    let has_target_zombie = children_snapshot.iter().any(|p| {
                        let p_inner = p.inner_exclusive_access();
                        p_inner.is_zombie && (pid == -1 || pid as usize == p.getpid())
                    });
                    !has_target_child || has_target_zombie
                });

            // If woken by a deliverable user-handled signal, return EINTR so the trap handler can dispatch it.
            {
                let task = current_task().unwrap();
                debug!(
                    "sys_wait4: before EINTR check process lock self_pid={} target_pid={} hart={}",
                    self_pid,
                    pid,
                    hartid()
                );
                let inner = process.inner_exclusive_access();
                debug!(
                    "sys_wait4: acquired process lock for EINTR check self_pid={} target_pid={} hart={}",
                    self_pid,
                    pid,
                    hartid()
                );
                let task_inner = task.inner_exclusive_access();
                let pending_unmasked = (task_inner.pending_signals | inner.pending_signals)
                    & !task_inner.signal_mask.without_unblockable();
                let has_user_handler = (1..=crate::task::MAX_SIG).any(|signum| {
                    let Some(flag) = SignalBit::from_signum(signum as u32) else { return false; };
                    if !pending_unmasked.contains(flag) {
                        return false;
                    }
                    let handler = inner.signal_actions.table[signum].handler;
                    handler != crate::task::SIG_DFL && handler != crate::task::SIG_IGN
                });
                debug!(
                    "sys_wait4: EINTR check self_pid={} target_pid={} pending_unmasked={:#x} has_user_handler={} hart={}",
                    self_pid,
                    pid,
                    pending_unmasked.bits(),
                    has_user_handler,
                    hartid()
                );
                if has_user_handler {
                    return Err(ERRNO::EINTR);
                }
            }
        }
    })
}

/// kill syscall.
///
/// Implements the full `kill(2)` target selection:
/// - `pid > 0`  : the process with that pid.
/// - `pid == 0` : every process in the caller's process group.
/// - `pid == -1`: every process except the caller (broadcast).
/// - `pid < -1` : every process in process group `-pid`.
///
/// A `signal` of `0` performs only an existence/permission check (no signal is
/// delivered), returning `ESRCH` when no target matches.
pub fn sys_kill(pid: isize, signal: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_kill pid:{} signal:{}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        pid,
        signal
    );
    syscall_body!({
        let sender = current_process();
        // signal 0 仅做存在性检查；非 0 时校验信号编号合法。
        let flag = if signal == 0 {
            None
        } else {
            Some(SignalBit::from_signum(signal).or_errno(ERRNO::EINVAL)?)
        };
        let siginfo = SigInfo::for_kill(signal as i32, sender.getpid(), sender.getuid());
        let sender_pid = sender.getpid();

        let collect_pgrp = |pgrp: u32| -> Vec<Arc<ProcessControlBlock>> {
            list_pids()
                .into_iter()
                .filter_map(pid2process)
                .filter(|process| process.getpgid() == pgrp)
                .collect()
        };

        let targets: Vec<Arc<ProcessControlBlock>> = if pid > 0 {
            alloc::vec![pid2process(pid as usize).or_errno(ERRNO::ESRCH)?]
        } else if pid == 0 {
            collect_pgrp(sender.getpgid())
        } else if pid == -1 {
            list_pids()
                .into_iter()
                .filter_map(pid2process)
                .filter(|process| process.getpid() != sender.getpid())
                .collect()
        } else {
            collect_pgrp((-pid) as u32)
        };

        if targets.is_empty() {
            return Err(ERRNO::ESRCH);
        }
        if pid <= 0 {
            debug!(
                "sys_kill: sender_pid={} sender_pgid={} pid_arg={} signal={} matched_targets={}",
                sender_pid,
                sender_pgid,
                pid,
                signal,
                targets.len()
            );
        }
        if let Some(flag) = flag {
            for process in &targets {
                let (is_zombie, task_count) = {
                    let inner = process.inner_exclusive_access();
                    (inner.is_zombie, inner.tasks.len())
                };
                debug!(
                    "sys_kill: sender_pid={} target_pid={} target_pgid={} pid_arg={} signal={} target_is_zombie={} target_tasks={}",
                    sender_pid,
                    process.getpid(),
                    process.getpgid(),
                    pid,
                    signal,
                    is_zombie,
                    task_count
                );
                crate::task::add_signal_to_process_with_siginfo(process, flag, siginfo);
            }
        }
        Ok(0)
    })
}

/// tkill syscall
///
/// 当前实现按“进程内 tid 索引”最小语义处理，仅支持向当前进程中的线程投递信号。
pub fn sys_tkill(tid: usize, signal: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_tkill",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let task = thread_id2task(tid).or_errno(ERRNO::ESRCH)?;
        let process = task.process.upgrade().unwrap();
        if process.getpid() != current_task().unwrap().process.upgrade().unwrap().getpid() {
            return Err(ERRNO::ESRCH);
        }
        let flag = SignalBit::from_signum(signal).or_errno(ERRNO::EINVAL)?;
        let sender = current_process();
        let siginfo = SigInfo::for_tkill(signal as i32, sender.getpid(), sender.getuid());
        crate::task::add_signal_to_task_with_siginfo(&task, flag, siginfo);
        Ok(0)
    })
}

/// tgkill syscall
///
/// 当前实现按”进程号 + 进程内 tid 索引”最小语义处理。
pub fn sys_tgkill(tgid: usize, tid: usize, signal: u32) -> isize {
    syscall_body!({
        let task = thread_id2task(tid).or_errno(ERRNO::ESRCH)?;
        let process = task.process.upgrade().unwrap();
        if process.getpid() != tgid {
            return Err(ERRNO::ESRCH);
        }
        let flag = SignalBit::from_signum(signal).or_errno(ERRNO::EINVAL)?;
        let sender = current_process();
        let siginfo = SigInfo::for_tkill(signal as i32, sender.getpid(), sender.getuid());
        let target_inner_tid = task.inner_exclusive_access().res.as_ref().unwrap().tid;
        debug!(
            "sys_tgkill: tgid={} tid={} signal={} -> target_task inner_tid={}",
            tgid, tid, signal, target_inner_tid
        );
        crate::task::add_signal_to_task_with_siginfo(&task, flag, siginfo);
        Ok(0)
    })
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
        let exec_path = canonicalize("/", path.as_str());
        let child = parent
            .spawn(all_data.as_slice(), exec_path)
            .or_errno(ERRNO::ENOEXEC)?;
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
    pub domainname: [u8; 65],
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
            domainname: [0; 65],
        };
        // glibc 启动阶段会解析 release 进行最低内核版本判断，
        // 这里返回 Linux 风格且足够新的版本串，避免误报 "kernel too old"。
        let sysname = b"Linux";
        let nodename = b"localhost";
        let release = b"6.6.0";
        let version = b"#1 SMP PREEMPT cosmOS";
        let machine = b"riscv64";
        let domainname = b"localdomain";
        uname.sysname[..sysname.len()].copy_from_slice(sysname);
        uname.nodename[..nodename.len()].copy_from_slice(nodename);
        uname.release[..release.len()].copy_from_slice(release);
        uname.version[..version.len()].copy_from_slice(version);
        uname.machine[..machine.len()].copy_from_slice(machine);
        uname.domainname[..domainname.len()].copy_from_slice(domainname);
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
        if !head_ptr.is_null() {
            write_pod_to_user(head_ptr, &robust_list.head)?;
        }
        if !len_ptr.is_null() {
            write_pod_to_user(len_ptr, &robust_list.len)?;
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

pub fn sys_getcpu(cpu_ptr: *mut u32, node_ptr: *mut u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_getcpu",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let cpu = hartid() as u32;
        if !cpu_ptr.is_null() {
            write_pod_to_user(cpu_ptr, &cpu)?;
        }
        if !node_ptr.is_null() {
            write_pod_to_user(node_ptr, &0u32)?;
        }
        Ok(0)
    })
}
