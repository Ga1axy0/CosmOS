use crate::mm::{frame_allocator_stats, MapPermission, USER_SPACE_END, VirtAddr};
use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall::{translated_byte_buffer_with_access, write_pod_to_user, Pod};
use crate::syscall_body;
use crate::task::yield_current_and_run_next;
use crate::timer::get_time_ns;
use crate::{
    config::PAGE_SIZE,
    fs::{canonicalize, open_file, open_file_at, File, OpenFlags},
    hal::hartid,
    ipc::{self, IPC_RMID},
    mm::{translated_ref, translated_str, PageFaultAccess},
    task::{
        current_process, current_task, current_trap_cx, current_user_token,
        exit_current_and_run_next, thread_id2task, ExitReason, ProcessControlBlock, ShmAttachment,
        SigInfo, SignalBit, WaitReason,
    },
};
use crate::sched::{add_task, list_pids, pid2process, remove_from_pid2process};

use alloc::{string::String, sync::Arc, vec, vec::Vec};

const UID_NO_CHANGE: u32 = u32::MAX;

fn unprivileged_uid_change_allowed(current_uid: u32, current_euid: u32, current_suid: u32, new_uid: u32) -> bool {
    new_uid == current_uid || new_uid == current_euid || new_uid == current_suid
}

fn unprivileged_gid_change_allowed(current_gid: u32, current_egid: u32, current_sgid: u32, new_gid: u32) -> bool {
    new_gid == current_gid || new_gid == current_egid || new_gid == current_sgid
}
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

fn should_trace_exec(path: &str, argv: &[String]) -> bool {
    path.contains("acct02")
        || argv.iter().any(|arg| arg.contains("acct02"))
}

fn path_env_from_envs(envs: &[String]) -> Option<&str> {
    envs.iter()
        .find_map(|env| env.strip_prefix("PATH="))
}

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
    sys_exit(exit_code);
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

/// setuid syscall
pub fn sys_setuid(uid: u32) -> isize {
    let process = current_process();
    trace!("kernel: sys_setuid pid:{} uid={}", process.getpid(), uid);
    syscall_body!({
        let current_uid = process.getuid();
        let current_euid = process.geteuid();
        let current_suid = process.getsuid();
        if current_euid != 0 && !unprivileged_uid_change_allowed(current_uid, current_euid, current_suid, uid) {
            return Err(ERRNO::EPERM);
        }
        process.setuid_cred(uid);
        Ok(0)
    })
}

/// setreuid syscall
pub fn sys_setreuid(ruid: u32, euid: u32) -> isize {
    let process = current_process();
    trace!("kernel: sys_setreuid pid:{} ruid={} euid={}", process.getpid(), ruid, euid);
    syscall_body!({
        let mut inner = process.inner_exclusive_access();
        let cred = &mut inner.cred;
        let old_ruid = cred.uid;
        let old_euid = cred.euid;
        let old_suid = cred.suid;
        let privileged = old_euid == 0;

        if !privileged {
            if ruid != UID_NO_CHANGE
                && !unprivileged_uid_change_allowed(old_ruid, old_euid, old_suid, ruid)
            {
                return Err(ERRNO::EPERM);
            }
            if euid != UID_NO_CHANGE
                && !unprivileged_uid_change_allowed(old_ruid, old_euid, old_suid, euid)
            {
                return Err(ERRNO::EPERM);
            }
        }

        let new_ruid = if ruid == UID_NO_CHANGE { old_ruid } else { ruid };
        let new_euid = if euid == UID_NO_CHANGE { old_euid } else { euid };
        cred.uid = new_ruid;
        cred.euid = new_euid;
        if ruid != UID_NO_CHANGE || (euid != UID_NO_CHANGE && new_euid != old_ruid) {
            cred.suid = new_euid;
        }
        Ok(0)
    })
}

/// setresuid syscall
pub fn sys_setresuid(ruid: u32, euid: u32, suid: u32) -> isize {
    let process = current_process();
    trace!(
        "kernel: sys_setresuid pid:{} ruid={} euid={} suid={}",
        process.getpid(),
        ruid,
        euid,
        suid
    );
    syscall_body!({
        let mut inner = process.inner_exclusive_access();
        let cred = &mut inner.cred;
        let old_ruid = cred.uid;
        let old_euid = cred.euid;
        let old_suid = cred.suid;
        let privileged = old_euid == 0;

        if !privileged {
            for new_uid in [ruid, euid, suid] {
                if new_uid == UID_NO_CHANGE {
                    continue;
                }
                if !unprivileged_uid_change_allowed(old_ruid, old_euid, old_suid, new_uid) {
                    return Err(ERRNO::EPERM);
                }
            }
        }

        if ruid != UID_NO_CHANGE {
            cred.uid = ruid;
        }
        if euid != UID_NO_CHANGE {
            cred.euid = euid;
        }
        if suid != UID_NO_CHANGE {
            cred.suid = suid;
        }
        Ok(0)
    })
}

/// setgid syscall
pub fn sys_setgid(gid: u32) -> isize {
    let process = current_process();
    trace!("kernel: sys_setgid pid:{} gid={}", process.getpid(), gid);
    syscall_body!({
        let mut inner = process.inner_exclusive_access();
        let cred = &mut inner.cred;
        if cred.euid != 0 && !unprivileged_gid_change_allowed(cred.gid, cred.egid, cred.sgid, gid) {
            return Err(ERRNO::EPERM);
        }
        cred.gid = gid;
        cred.egid = gid;
        cred.sgid = gid;
        Ok(0)
    })
}

/// setregid syscall
pub fn sys_setregid(rgid: u32, egid: u32) -> isize {
    let process = current_process();
    trace!("kernel: sys_setregid pid:{} rgid={} egid={}", process.getpid(), rgid, egid);
    syscall_body!({
        let mut inner = process.inner_exclusive_access();
        let cred = &mut inner.cred;
        let old_rgid = cred.gid;
        let old_egid = cred.egid;
        let old_sgid = cred.sgid;
        let privileged = cred.euid == 0;

        if !privileged {
            if rgid != UID_NO_CHANGE
                && !unprivileged_gid_change_allowed(old_rgid, old_egid, old_sgid, rgid)
            {
                return Err(ERRNO::EPERM);
            }
            if egid != UID_NO_CHANGE
                && !unprivileged_gid_change_allowed(old_rgid, old_egid, old_sgid, egid)
            {
                return Err(ERRNO::EPERM);
            }
        }

        let new_rgid = if rgid == UID_NO_CHANGE { old_rgid } else { rgid };
        let new_egid = if egid == UID_NO_CHANGE { old_egid } else { egid };
        cred.gid = new_rgid;
        cred.egid = new_egid;
        if rgid != UID_NO_CHANGE || (egid != UID_NO_CHANGE && new_egid != old_rgid) {
            cred.sgid = new_egid;
        }
        Ok(0)
    })
}

/// setresgid syscall
pub fn sys_setresgid(rgid: u32, egid: u32, sgid: u32) -> isize {
    let process = current_process();
    trace!(
        "kernel: sys_setresgid pid:{} rgid={} egid={} sgid={}",
        process.getpid(),
        rgid,
        egid,
        sgid
    );
    syscall_body!({
        let mut inner = process.inner_exclusive_access();
        let cred = &mut inner.cred;
        let old_rgid = cred.gid;
        let old_egid = cred.egid;
        let old_sgid = cred.sgid;
        let privileged = cred.euid == 0;

        if !privileged {
            for new_gid in [rgid, egid, sgid] {
                if new_gid == UID_NO_CHANGE {
                    continue;
                }
                if !unprivileged_gid_change_allowed(old_rgid, old_egid, old_sgid, new_gid) {
                    return Err(ERRNO::EPERM);
                }
            }
        }

        if rgid != UID_NO_CHANGE {
            cred.gid = rgid;
        }
        if egid != UID_NO_CHANGE {
            cred.egid = egid;
        }
        if sgid != UID_NO_CHANGE {
            cred.sgid = sgid;
        }
        Ok(0)
    })
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
            if !shmaddr % crate::config::PAGE_SIZE == 0 {
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
        /// 创建新的 mount namespace；当前先按兼容 no-op 处理。
        const CLONE_NEWNS = 0x0002_0000;
        /// 创建新的 UTS namespace；当前先按兼容 no-op 处理。
        const CLONE_NEWUTS = 0x0400_0000;
        /// 创建新的 IPC namespace；当前先按兼容 no-op 处理。
        const CLONE_NEWIPC = 0x0800_0000;
        /// 创建新的 user namespace；当前先按兼容 no-op 处理。
        const CLONE_NEWUSER = 0x1000_0000;
        /// 创建新的 PID namespace；当前先按兼容 no-op 处理。
        const CLONE_NEWPID = 0x2000_0000;
        /// 创建新的 network namespace；当前先按兼容 no-op 处理。
        const CLONE_NEWNET = 0x4000_0000;
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
        let vfork_clone = flags.contains(CloneFlags::CLONE_VFORK);
        let thread_clone = flags.contains(CloneFlags::CLONE_VM) && !vfork_clone;
        if flags.contains(CloneFlags::CLONE_VM)
            && !flags.contains(CloneFlags::CLONE_THREAD)
            && !vfork_clone
        {
            warn!("kernel: sys_clone CLONE_VM without CLONE_THREAD is unsupported");
            return Err(ERRNO::EINVAL);
        }
        if vfork_clone && !flags.contains(CloneFlags::CLONE_VM) {
            warn!("kernel: sys_clone CLONE_VFORK without CLONE_VM is unsupported");
            return Err(ERRNO::EINVAL);
        }
        if vfork_clone && flags.contains(CloneFlags::CLONE_THREAD) {
            warn!("kernel: sys_clone CLONE_VFORK with CLONE_THREAD is unsupported");
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
                trap_cx.set_kernel_sp(new_task.kstack.get_top());
                trap_cx.set_syscall_ret(0);
                if stack != 0 {
                    trap_cx.set_user_sp(stack);
                }
                if let Some(tls) = child_tls {
                    trap_cx.set_tls(tls);
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
            if vfork_clone {
                debug!(
                    "kernel: sys_clone emulate CLONE_VM|CLONE_VFORK as fork-like process clone"
                );
            }
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

/// `setns` 兼容实现。
///
/// 当前内核尚未提供独立 namespace 隔离，但 LTP 的网络 helper 需要
/// `/proc/<pid>/ns/*` 可打开且 `setns()` 可成功返回，才能继续构造本地
/// 双端口拓扑。这里先校验 fd 有效，再按 no-op 成功处理。
pub fn sys_setns(fd: i32, _nstype: i32) -> isize {
    syscall_body!({
        if fd < 0 {
            return Err(ERRNO::EBADF);
        }
        let process = current_process();
        let inner = process.inner_exclusive_access();
        let fd = fd as usize;
        if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
            return Err(ERRNO::EBADF);
        }
        Ok(0)
    })
}

/// `unshare` namespace compatibility shim.
///
/// CosmOS does not isolate namespace state yet. Accept the namespace flags used
/// by LTP setup helpers as no-ops so tests can exercise the target syscall
/// behavior behind their namespace bootstrap.
pub fn sys_unshare(flags: usize) -> isize {
    const CLONE_NEWNS: usize = 0x0002_0000;
    const CLONE_NEWUTS: usize = 0x0400_0000;
    const CLONE_NEWIPC: usize = 0x0800_0000;
    const CLONE_NEWUSER: usize = 0x1000_0000;
    const CLONE_NEWPID: usize = 0x2000_0000;
    const CLONE_NEWNET: usize = 0x4000_0000;
    const SUPPORTED_FLAGS: usize =
        CLONE_NEWNS | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWUSER | CLONE_NEWPID | CLONE_NEWNET;

    syscall_body!({
        if flags & !SUPPORTED_FLAGS != 0 {
            return Err(ERRNO::EINVAL);
        }
        Ok(0)
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
        if envs_vec.is_empty() {
            let inherited_envs = process.inner_exclusive_access().environment.clone();
            if !inherited_envs.is_empty() {
                if should_trace_exec(path.as_str(), args_vec.as_slice()) {
                    debug!(
                        "[execve] pid={} inherited empty envp from current environment PATH={:?}",
                        process.getpid(),
                        path_env_from_envs(inherited_envs.as_slice())
                    );
                }
                envs_vec = inherited_envs;
            }
        }
        if should_trace_exec(path.as_str(), args_vec.as_slice()) {
            debug!(
                "[execve] pid={} path='{}' argv={:?} PATH={:?}",
                process.getpid(),
                path,
                args_vec,
                path_env_from_envs(envs_vec.as_slice())
            );
        }

        let cwd = process.inner_exclusive_access().cwd.clone();
        debug!(" ------------------- Resolve -----------------------");
        let resolved = match resolve_exec_image(cwd.as_str(), path.as_str(), args_vec, 0) {
            Ok(resolved) => resolved,
            Err(errno) => {
                if path.contains("acct02") {
                    debug!(
                        "[execve] resolve failed pid={} cwd='{}' path='{}': {:?}",
                        process.getpid(),
                        cwd,
                        path,
                        errno
                    );
                }
                return Err(errno);
            }
        };
        debug!(" ------------------- End Resolve -----------------------");
        let ResolvedExecImage {
            elf_data,
            argv,
            exec_path,
        } = resolved;
        if exec_path.contains("acct02") || argv.iter().any(|arg| arg.contains("acct02")) {
            debug!(
                "[execve] resolved pid={} exec_path='{}' argv={:?}",
                process.getpid(),
                exec_path,
                argv
            );
        }
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

/// waitpid syscall
///
/// If there is not a child process whose pid is same as given, return -ECHILD.
/// Else if there is a child process but it is still running, return -EAGAIN.
pub fn sys_wait4(pid: isize, exit_status_ptr: *mut i32, options: isize) -> isize {
    trace!("kernel: sys_wait4");
    let process = current_process();
    syscall_body!({
        // 只在低 32 位上校验选项，避免符号扩展把 `__WCLONE`(0x80000000) 误判为非法位。
        // `WUNTRACED`/`WCONTINUED` 当前没有额外的停止/继续状态可上报（本内核不实现
        // 作业控制停止），按 Linux 语义安全地忽略即可——这正是 shell 前台等待
        // `waitpid(pid, &status, WUNTRACED)` 所需要的。
        if (options & 0xffff_ffff) & !WAIT_RECOGNIZED != 0 {
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
                    ExitReason::Signal(signum) => {
                        // 低 7 位为终止信号；若该信号默认动作会转储核心，
                        // 置上 0x80（WCOREDUMP）以满足 `WCOREDUMP(status)`。
                        let mut status = (signum & 0x7f) as i32;
                        let dumps_core = crate::signal::SignalNum::from_number(signum)
                            .map(|sig| sig.dumps_core())
                            .unwrap_or(false);
                        if dumps_core {
                            status |= 0x80;
                        }
                        status
                    }
                };
                inner.child_user_time = inner
                    .child_user_time
                    .saturating_add(child_inner.user_time)
                    .saturating_add(child_inner.child_user_time);
                inner.child_kernel_time = inner
                    .child_kernel_time
                    .saturating_add(child_inner.kernel_time)
                    .saturating_add(child_inner.child_kernel_time);
                drop(child_inner);
                drop(inner);
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

            // If woken by a deliverable user-handled signal, return EINTR so the trap handler can dispatch it.
            {
                let task = current_task().unwrap();
                let inner = process.inner_exclusive_access();
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
        if let Some(flag) = flag {
            for process in &targets {
                debug!(
                    "sys_kill: sender_pid={} target_pid={} target_pgid={} pid_arg={}",
                    sender_pid,
                    process.getpid(),
                    process.getpgid(),
                    pid
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

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SysInfo {
    pub uptime: i64,
    pub loads: [u64; 3],
    pub totalram: u64,
    pub freeram: u64,
    pub sharedram: u64,
    pub bufferram: u64,
    pub totalswap: u64,
    pub freeswap: u64,
    pub procs: u16,
    pub pad: u16,
    pub totalhigh: u64,
    pub freehigh: u64,
    pub mem_unit: u32,
    pub _f: [u8; 0],
}

impl Pod for SysInfo {}

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
        let machine = crate::platform::machine_name().as_bytes();
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

/// sysinfo syscall
pub fn sys_sysinfo(info_ptr: *mut SysInfo) -> isize {
    trace!(
        "kernel:pid[{}] sys_sysinfo",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let stats = frame_allocator_stats();
        let page_bytes = PAGE_SIZE as u64;
        let info = SysInfo {
            uptime: (get_time_ns() / 1_000_000_000).min(i64::MAX as u64) as i64,
            loads: [0; 3],
            totalram: stats.total_pages as u64 * page_bytes,
            freeram: stats.free_pages as u64 * page_bytes,
            sharedram: 0,
            bufferram: 0,
            totalswap: 0,
            freeswap: 0,
            procs: list_pids().len().min(u16::MAX as usize) as u16,
            pad: 0,
            totalhigh: 0,
            freehigh: 0,
            mem_unit: 1,
            _f: [],
        };
        write_pod_to_user(info_ptr, &info)?;
        Ok(0)
    })
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
