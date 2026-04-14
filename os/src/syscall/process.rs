use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall::{write_pod_to_user, Pod};
use crate::syscall_body;
use crate::{
    fs::{canonicalize, open_file, open_file_at, File, OpenFlags},
    hart::hartid,
    mm::{translated_ref, translated_refmut, translated_str},
    task::{
        add_signal_to_process, current_process, current_task, current_user_token,
        exit_current_and_run_next, pid2process, ExitReason, SignalAction, SignalFlags,
        WaitReason,
    },
};

use alloc::{string::String, vec::Vec};
/// `execve` 在解析脚本后得到的最终执行目标。
struct ResolvedExecImage {
    /// 最终需要交给 ELF 装载器处理的字节内容。
    elf_data: Vec<u8>,
    /// 按 shebang 规则重写后的参数列表。
    argv: Vec<String>,
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
    if is_elf_image(&first_line) {
        let file_data = inode.read_all();
        return Ok(ResolvedExecImage {
            elf_data: file_data,
            argv,
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

pub fn sys_getsid() -> isize {
    let process = current_process();
    trace!("kernel: sys_getsid pid:{}", process.getpid());
    process.getsid() as isize
}

pub fn sys_setsid() -> isize {
    trace!("kernel: sys_setsid pid:{}", current_process().getpid());
    warn!("kernel: sys_setsid is not fully implemented, just return new sid 1");
    1
}

/// fork child process syscall
pub fn sys_fork() -> isize {
    trace!(
        "kernel:pid[{}] sys_fork",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let current_process = current_process();
    let new_process = current_process.fork();
    new_process.getpid() as isize
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
        let resolved = resolve_exec_image(cwd.as_str(), path.as_str(), args_vec, 0)?;
        let ResolvedExecImage { elf_data, argv } = resolved;
        let argc = argv.len();
        process.exec(elf_data.as_slice(), argv).or_errno(ERRNO::ENOEXEC)?;
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
        add_signal_to_process(&process, flag);
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
        let process = current_process();
        let target_exists = {
            let inner = process.inner_exclusive_access();
            tid < inner.tasks.len() && inner.tasks[tid].is_some()
        };
        if !target_exists {
            return Err(ERRNO::ESRCH);
        }
        let flag = SignalFlags::from_signum(signal).or_errno(ERRNO::EINVAL)?;
        add_signal_to_process(&process, flag);
        Ok(0)
    })
}

/// tgkill syscall
///
/// 当前实现按“进程号 + 进程内 tid 索引”最小语义处理。
pub fn sys_tgkill(tgid: usize, tid: usize, signal: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_tgkill",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let process = pid2process(tgid).or_errno(ERRNO::ESRCH)?;
        let target_exists = {
            let inner = process.inner_exclusive_access();
            tid < inner.tasks.len() && inner.tasks[tid].is_some()
        };
        if !target_exists {
            return Err(ERRNO::ESRCH);
        }
        let flag = SignalFlags::from_signum(signal).or_errno(ERRNO::EINVAL)?;
        add_signal_to_process(&process, flag);
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

/// sigprocmask / rt_sigprocmask 系统调用
///
/// Linux 语义：
///   how == SIG_BLOCK   (0) -> mask |= set
///   how == SIG_UNBLOCK (1) -> mask &= ~set
///   how == SIG_SETMASK (2) -> mask = set
/// 参数含义遵循 rt_sigprocmask: (int how, const sigset_t *set, sigset_t *oset, size_t sigsetsize)
pub fn sys_sigprocmask(how: i32, set: *const u32, oset: *mut u32, sigsetsize: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_sigprocmask",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        // We represent sigset as a u32 bitmask in this kernel; require user-provided
        // buffer to be large enough to hold at least a u32.
        if sigsetsize < core::mem::size_of::<u32>() {
            return Err(ERRNO::EINVAL);
        }

        let process = current_process();
        let mut inner = process.inner_exclusive_access();

        // If user requested old mask, write it out first.
        if !oset.is_null() {
            let old_bits = inner.signal_mask.bits();
            let slot = translated_refmut(token, oset).ok_or(ERRNO::EFAULT)?;
            *slot = old_bits;
        }

        // If user provided a new mask, apply according to `how`.
        if !set.is_null() {
            let new_bits = *translated_ref(token, set).or_errno(ERRNO::EFAULT)?;
            let new_mask = SignalFlags::from_bits(new_bits).or_errno(ERRNO::EINVAL)?;
            match how {
                0 => inner.signal_mask.insert(new_mask), // SIG_BLOCK
                1 => inner.signal_mask.remove(new_mask), // SIG_UNBLOCK
                2 => inner.signal_mask = new_mask,       // SIG_SETMASK
                _ => return Err(ERRNO::EINVAL),
            }
        }

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

pub fn sys_getcpu(cpu_ptr: *mut u32, node_ptr: *mut u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_getcpu",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let token = current_user_token();
        let cpu = hartid() as u32;
        if !cpu_ptr.is_null() {
            translated_refmut(token, cpu_ptr).map(|slot| *slot = cpu).ok_or(ERRNO::EFAULT)?;
        }
        if !node_ptr.is_null() {
            translated_refmut(token, node_ptr).map(|slot| *slot = 0).ok_or(ERRNO::EFAULT)?;
        }
        Ok(0)
    })
}
