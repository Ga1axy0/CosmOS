//! Implementation of  [`ProcessControlBlock`]
#![allow(deprecated)]

use super::id::RecycleAllocator;
use super::{insert_into_tid2task, SchedAttr, TaskControlBlock};
use super::{SigInfo, SignalAction, SignalActions, SignalBit, MAX_SIG, SIG_IGN};
use crate::sched::add_task;
use crate::sched::insert_into_pid2process;
use super::{pid_alloc, PidHandle};
use super::WaitQueue;
use crate::config::{CLOCK_FREQ, PAGE_SIZE};
use crate::fs::{canonicalize, mapping_for_inode, new_stdio_files, open_file_at, File, FileDescription, OpenFlags};
use crate::ipc;
use crate::mm::{
    register_file_mapping, shootdown, translated_refmut, DeferredUserReclaim, InodeKey,
    MapPermission, MemorySet, MmError, PageFaultAccess, PageFaultHandled, ShootdownKind,
    UserSpaceLayout, VirtAddr, Vma, KERNEL_SPACE,
};
use crate::sync::{Condvar, DeadlockDetector, Mutex, Semaphore, SpinNoIrqLock, SpinNoIrqLockGuard};
use crate::syscall::errno::ERRNO;
use crate::syscall::{write_pod_to_process_user, ResourceLimits};
use crate::timer::get_realtime_ns;
use crate::trap::{trap_handler, TrapContext};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

/// 每秒对应的纳秒数。
const NSEC_PER_SEC: u64 = 1_000_000_000;
/// 新进程默认文件创建掩码，贴近常见 Linux 用户态环境。
const DEFAULT_UMASK: u32 = 0o022;

const INIT_CWD: &str = "/root";
const INIT_INTERPRETER_MAX_DEPTH: usize = 4;
const INIT_PROBE_SIZE: usize = 256;
const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const INIT_ENV: &[&str] = &[
    "HOME=/root",
    "INPUTRC=/etc/inputrc",
    "TERM=xterm-256color",
    "PATH=/bin:/sbin:/usr/bin:/usr/sbin:/root",
    "USER=root",
    "LOGNAME=root",
    "SHELL=/bin/bash",
    "PWD=/root",
];

fn mm_error_to_errno(err: MmError) -> ERRNO {
    match err {
        MmError::OutOfMemory => ERRNO::ENOMEM,
        MmError::InvalidRange => ERRNO::EINVAL,
        MmError::Conflict => ERRNO::EACCES,
        MmError::NoMapping => ERRNO::EFAULT,
        MmError::PermissionDenied => ERRNO::EFAULT,
        MmError::BeyondFileEnd => ERRNO::ENXIO,
        MmError::InvalidElf => ERRNO::ENOEXEC,
    }
}

bitflags! {
    /// fd 表项级别的标志位。
    pub struct FdFlags: u32 {
        /// `exec` 成功后自动关闭该 fd。
        const CLOEXEC = 0x1;
    }
}

/// fd 表中的单个表项，区分 fd 自身标志与底层文件对象。
#[derive(Clone)]
pub struct FdEntry {
    /// 当前 fd 引用的打开文件描述。
    pub desc: Arc<FileDescription>,
    /// 当前 fd 的局部标志位。
    pub flags: FdFlags,
}

impl FdEntry {
    /// 基于文件对象创建默认 fd 表项。
    pub fn new(desc: Arc<FileDescription>) -> Self {
        Self {
            desc,
            // TODO: 后续补齐 `fcntl/open(O_CLOEXEC)` 后，应在创建时设置真实 fd 标志位。
            flags: FdFlags::empty(),
        }
    }
}

/// Process termination reason preserved for wait4/waitpid encoding.
#[derive(Debug, Clone, Copy)]
pub enum ExitReason {
    /// Process terminated via `exit(code)`.
    Exit(i32),
    /// Process terminated by a signal number.
    Signal(u32),
}

#[repr(usize)]
#[derive(Debug, Clone, Copy)]
enum Auxv {
    Phdr = 3,     // program headers for program
    Phent = 4,    // size of program header entry
    Phnum = 5,    // number of program header entries
    Pagesz = 6,   // system page size
    Base = 7,     // base address of interpreter
    Entry = 9,    // entry point of program
    Random = 25,  // address of 16 random bytes
}

/// Process Control Block
pub struct ProcessControlBlock {
    /// immutable
    pub pid: PidHandle,
    /// mutable
    inner: SpinNoIrqLock<ProcessControlBlockInner>,
    /// wait queue for wait4/waitpid
    pub wait_exit_queue: Arc<WaitQueue>,
}

#[derive(Debug, Clone, Copy)]
pub struct Credentials {
    pub uid: u32,
    pub euid: u32,
    pub suid: u32,
    pub gid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub sid: u32,
    pub pgid: u32,
}

#[derive(Debug, Clone, Copy, Default)]
/// Lazily created special keyrings attached to a process context.
pub struct ProcessKeyrings {
    /// Backing serial for `KEY_SPEC_THREAD_KEYRING`.
    pub thread: Option<i32>,
    /// Backing serial for `KEY_SPEC_PROCESS_KEYRING`.
    pub process: Option<i32>,
    /// Backing serial for `KEY_SPEC_SESSION_KEYRING`.
    pub session: Option<i32>,
}

impl Credentials {
    pub const fn root() -> Self {
        Self {
            uid: 0,
            euid: 0,
            suid: 0,
            gid: 0,
            egid: 0,
            sgid: 0,
            sid: 0,
            pgid: 0,
        }
    }
}

/// Inner of Process Control Block
pub struct ProcessControlBlockInner {
    /// is zombie?
    pub is_zombie: bool,
    /// memory set(address space)
    pub memory_set: MemorySet,
    /// process virtual memory layout metadata
    pub vm_layout: ProcessVmLayout,
    /// parent process
    pub parent: Option<Weak<ProcessControlBlock>>,
    /// children process
    pub children: Vec<Arc<ProcessControlBlock>>,
    /// exit reason observed by wait4/waitpid
    pub exit_reason: ExitReason,
    /// file descriptor table
    pub fd_table: Vec<Option<FdEntry>>,
    /// per-process resource limits
    pub resource_limits: ResourceLimits,
    /// pending process signals
    pub pending_signals: SignalBit,
    /// Per-signal metadata paired with `pending_signals`.
    pub pending_siginfo: [SigInfo; MAX_SIG + 1],
    /// installed signal actions
    pub signal_actions: SignalActions,
    /// tasks(also known as threads)
    pub tasks: Vec<Option<Arc<TaskControlBlock>>>,
    /// task resource allocator
    pub task_res_allocator: RecycleAllocator,
    /// mutex list
    pub mutex_list: Vec<Option<Arc<dyn Mutex>>>,
    /// semaphore list
    pub semaphore_list: Vec<Option<Arc<Semaphore>>>,
    /// condvar list
    pub condvar_list: Vec<Option<Arc<Condvar>>>,
    /// deadlock_enabled
    pub deadlock_enabled: bool,
    /// deadlock detector for mutex resources
    pub mutex_detector: DeadlockDetector,
    /// deadlock detector for semaphore resources
    pub semaphore_detector: DeadlockDetector,
    /// current working directory (absolute path)
    pub cwd: String,
    /// absolute path of the last executed image (for /proc/<pid>/exe)
    pub exec_path: String,
    /// process environment seen by future `execve` inheritance/fallback
    pub environment: Vec<String>,
    /// process file creation mask (`umask`)
    pub umask: u32,
    /// process credentials
    pub cred: Credentials,
    /// lazily created special keyrings visible through `add_key/keyctl`
    pub keyrings: ProcessKeyrings,
    /// CPU time spent in user mode for this process (raw timer counter units)
    pub user_time: usize,
    /// CPU time spent in kernel mode for this process (raw timer counter units)
    pub kernel_time: usize,
    /// waited-for children's aggregated user time (raw timer counter units)
    pub child_user_time: usize,
    /// waited-for children's aggregated kernel time (raw timer counter units)
    pub child_kernel_time: usize,
    /// Current CPU accounting mode for this process on the single core.
    pub accounting_state: CpuAccountingState,
    /// Timestamp of the last accounting state transition.
    pub accounting_timestamp: usize,
    /// Process birth time on the realtime clock, used for BSD process accounting.
    pub accounting_start_time_ns: u64,
    /// `ITIMER_REAL`：基于 `CLOCK_REALTIME`（墙钟时间）。
    pub itimer_real: ItimerState,
    /// `ITIMER_VIRTUAL`：基于进程用户态 CPU 时间。
    pub itimer_virtual: ItimerState,
    /// `ITIMER_PROF`：基于进程用户态 + 内核态 CPU 时间。
    pub itimer_prof: ItimerState,
    /// Robust list
    pub robust_list: RobustList,
    /// Active SysV shared-memory attachments in this process.
    pub shm_attachments: Vec<ShmAttachment>,
}

/// One SysV shared-memory attachment in a process address space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShmAttachment {
    /// Shared memory identifier.
    pub shmid: usize,
    /// User virtual address returned by `shmat`.
    pub addr: usize,
    /// Mapped byte length.
    pub size: usize,
}

pub struct RobustList {
    pub head: usize,
    pub len: usize,
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum CpuAccountingState {
    Inactive,
    User,
    Kernel,
}

/// 进程级 interval timer 状态。
///
/// `deadline_ns == 0` 表示该 timer 当前未启用。
#[derive(Debug, Clone, Copy, Default)]
pub struct ItimerState {
    /// 周期重装载间隔，单位：纳秒。
    pub interval_ns: u64,
    /// 下一次到期绝对时间，单位：纳秒（所在时钟域）。
    pub deadline_ns: u64,
}

/// 系统范围内当前“已武装”的 interval timer 数量。
///
/// 时钟 tick 在没有任何 timer 武装时（绝大多数负载，例如 hackbench）可据此
/// 完全跳过对全部进程的扫描，对应 Linux 在没有挂起 timer 时不做 per-tick 工作
/// 的做法。该计数是**保守**的：武装时 +1、显式解除/单次到期时 -1，但进程在
/// 仍持有已武装 timer 的情况下退出时不做 -1，因此只会“多算”（偶尔多扫一次，
/// 无害），绝不会“少算”，从而保证 timer 投递不会被漏掉。
static ARMED_ITIMERS: AtomicUsize = AtomicUsize::new(0);

/// 读取当前已武装 interval timer 数量。
pub fn armed_itimers_count() -> usize {
    ARMED_ITIMERS.load(Ordering::Acquire)
}

#[inline]
fn itimer_account_arm() {
    ARMED_ITIMERS.fetch_add(1, Ordering::AcqRel);
}

#[inline]
fn itimer_account_disarm() {
    // saturating，避免任何意外下溢。
    let _ = ARMED_ITIMERS.fetch_update(Ordering::AcqRel, Ordering::Acquire, |v| {
        Some(v.saturating_sub(1))
    });
}

#[inline]
fn raw_counter_to_ns(raw: usize) -> u64 {
    ((raw as u128) * (NSEC_PER_SEC as u128) / (CLOCK_FREQ as u128)) as u64
}

#[inline]
fn itimer_remaining_ns(deadline_ns: u64, now_ns: u64) -> u64 {
    if deadline_ns == 0 || deadline_ns <= now_ns {
        0
    } else {
        deadline_ns - now_ns
    }
}

#[inline]
fn rearm_itimer_after_expire(timer: &mut ItimerState, now_ns: u64) {
    if timer.deadline_ns == 0 || timer.deadline_ns > now_ns {
        return;
    }
    if timer.interval_ns == 0 {
        timer.deadline_ns = 0;
        return;
    }
    let elapsed = now_ns.saturating_sub(timer.deadline_ns);
    let periods = elapsed / timer.interval_ns + 1;
    let advance = timer.interval_ns.saturating_mul(periods);
    timer.deadline_ns = timer.deadline_ns.saturating_add(advance);
}

/// 进程级虚拟内存边界信息，用于协调 heap、mmap 和主线程栈布局。
#[derive(Debug, Clone, Copy)]
pub struct ProcessVmLayout {
    /// 初始程序 break，不允许向下收缩到此地址以下。
    pub start_brk: usize,
    /// 当前程序 break。
    pub brk: usize,
    /// 匿名 mmap 自动选址的默认基址。
    pub mmap_base: usize,
    /// 上一次匿名 mmap 成功后留下的提示地址。
    pub mmap_hint: usize,
    /// 主线程的初始栈顶位置。
    pub start_stack: usize,
}

impl ProcessVmLayout {
    /// 根据装载器返回的用户地址空间布局初始化进程级边界信息。
    pub fn from_user_layout(layout: UserSpaceLayout) -> Self {
        Self {
            start_brk: layout.start_brk,
            brk: layout.start_brk,
            mmap_base: layout.mmap_base,
            mmap_hint: layout.mmap_base,
            start_stack: layout.start_stack,
        }
    }
}

/// 按 Linux ELF ABI 在用户栈上构造 argc/argv/envp/auxv 初始布局。
fn init_user_stack_from_strings(
    token: usize,
    stack_top: usize,
    args: &[String],
    envs: &[String],
    auxv_extra: &[(Auxv, usize)],
) -> usize {
    let mut user_sp = stack_top;

    // Place environment strings at high addresses (reverse order so pointers end up in order).
    let mut env_ptrs: Vec<usize> = Vec::with_capacity(envs.len());
    for env in envs.iter().rev() {
        user_sp -= env.len() + 1;
        let mut p = user_sp;
        for c in env.as_bytes() {
            *translated_refmut(token, p as *mut u8).unwrap() = *c;
            p += 1;
        }
        *translated_refmut(token, p as *mut u8).unwrap() = 0;
        env_ptrs.push(user_sp);
    }
    env_ptrs.reverse();

    // Place argument strings.
    let mut arg_ptrs: Vec<usize> = Vec::with_capacity(args.len());
    for arg in args.iter().rev() {
        user_sp -= arg.len() + 1;
        let mut p = user_sp;
        for c in arg.as_bytes() {
            *translated_refmut(token, p as *mut u8).unwrap() = *c;
            p += 1;
        }
        *translated_refmut(token, p as *mut u8).unwrap() = 0;
        arg_ptrs.push(user_sp);
    }
    arg_ptrs.reverse();

    user_sp &= !0xf_usize;

    // AT_RANDOM: 16 bytes of pseudo-random data for stack canary.
    // TODO: 后续接入真正的随机源，避免固定 canary。
    user_sp -= 8;
    *translated_refmut(token, user_sp as *mut usize).unwrap() = 0xdeadbeef_cafebabe_usize;
    user_sp -= 8;
    *translated_refmut(token, user_sp as *mut usize).unwrap() = 0x0102030405060708_usize;
    let at_random_ptr = user_sp;

    // auxv: AT_NULL terminator, then extra entries (reversed), then base entries.
    user_sp -= core::mem::size_of::<usize>();
    *translated_refmut(token, user_sp as *mut usize).unwrap() = 0;
    user_sp -= core::mem::size_of::<usize>();
    *translated_refmut(token, user_sp as *mut usize).unwrap() = 0;
    for &(tag, val) in auxv_extra.iter().rev() {
        user_sp -= core::mem::size_of::<usize>();
        *translated_refmut(token, user_sp as *mut usize).unwrap() = val;
        user_sp -= core::mem::size_of::<usize>();
        *translated_refmut(token, user_sp as *mut usize).unwrap() = tag as usize;
    }
    for &word in &[at_random_ptr, 25, crate::config::PAGE_SIZE, 6usize] {
        user_sp -= core::mem::size_of::<usize>();
        *translated_refmut(token, user_sp as *mut usize).unwrap() = word;
    }

    // envp array: NULL terminator, then pointers in reverse.
    user_sp -= core::mem::size_of::<usize>();
    *translated_refmut(token, user_sp as *mut usize).unwrap() = 0;
    for &ptr in env_ptrs.iter().rev() {
        user_sp -= core::mem::size_of::<usize>();
        *translated_refmut(token, user_sp as *mut usize).unwrap() = ptr;
    }

    // argv array: NULL terminator, then pointers in reverse.
    user_sp -= core::mem::size_of::<usize>();
    *translated_refmut(token, user_sp as *mut usize).unwrap() = 0;
    for &ptr in arg_ptrs.iter().rev() {
        user_sp -= core::mem::size_of::<usize>();
        *translated_refmut(token, user_sp as *mut usize).unwrap() = ptr;
    }

    // argc
    user_sp -= core::mem::size_of::<usize>();
    *translated_refmut(token, user_sp as *mut usize).unwrap() = args.len();
    user_sp
}

struct ResolvedInitImage {
    elf_data: Vec<u8>,
    argv: Vec<String>,
}

struct ShebangInfo {
    interpreter: String,
    optional_arg: Option<String>,
}

fn is_elf_image(file_data: &[u8]) -> bool {
    file_data.starts_with(ELF_MAGIC)
}

fn parse_shebang_line(file_data: &[u8]) -> Result<Option<ShebangInfo>, ERRNO> {
    if !file_data.starts_with(b"#!") {
        return Ok(None);
    }

    let line_end = file_data
        .iter()
        .position(|&ch| ch == b'\n')
        .unwrap_or(file_data.len());
    let line = core::str::from_utf8(&file_data[2..line_end]).map_err(|_| ERRNO::ENOEXEC)?;
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

fn resolve_init_image(cwd: &str, path: &str, argv: Vec<String>, depth: usize) -> Result<ResolvedInitImage, ERRNO> {
    if depth >= INIT_INTERPRETER_MAX_DEPTH {
        return Err(ERRNO::ELOOP);
    }

    let abs_path = canonicalize(cwd, path);
    let inode = open_file_at(cwd, path, OpenFlags::RDONLY).map_err(|_| ERRNO::ENOENT)?;
    if inode.is_dir() {
        return Err(ERRNO::EISDIR);
    }

    let (first_line, first_line_complete) = inode.read_first_line_limited(INIT_PROBE_SIZE);
    if is_elf_image(&first_line) {
        return Ok(ResolvedInitImage {
            elf_data: inode.read_all(),
            argv,
        });
    }

    if !first_line_complete {
        return Err(ERRNO::ENOEXEC);
    }

    if let Some(shebang) = parse_shebang_line(&first_line)? {
        if !shebang.interpreter.starts_with('/') {
            return Err(ERRNO::ENOEXEC);
        }

        let mut next_argv = Vec::with_capacity(argv.len() + 2);
        next_argv.push(shebang.interpreter.clone());
        if let Some(optional_arg) = shebang.optional_arg {
            next_argv.push(optional_arg);
        }
        next_argv.push(abs_path);
        next_argv.extend(argv.into_iter().skip(1));
        return resolve_init_image(cwd, shebang.interpreter.as_str(), next_argv, depth + 1);
    }

    Err(ERRNO::ENOEXEC)
}

fn init_user_stack(token: usize, stack_top: usize, args: &[&str]) -> usize {
    let args: Vec<String> = args.iter().map(|arg| String::from(*arg)).collect();
    init_user_stack_from_strings(token, stack_top, args.as_slice(), &[], &[])
}

fn load_process_image(
    elf_data: &[u8],
    cwd: &str,
) -> Result<(MemorySet, UserSpaceLayout, usize, Vec<(Auxv, usize)>), ERRNO> {
    let (mut memory_set, user_layout, app_load_info) =
        MemorySet::from_elf(elf_data).map_err(mm_error_to_errno)?;

    let (final_entry, auxv_extra) = if let Some(interp_path) = &app_load_info.interp_path {
        debug!("Dynamic linking required, loading interpreter: {}", interp_path);
        let interp_inode = match open_file_at(cwd, interp_path.as_str(), OpenFlags::RDONLY) {
            Ok(inode) => inode,
            Err(_) => {
                error!("Failed to open interpreter {}", interp_path);
                return Err(ERRNO::ENOENT);
            }
        };

        if interp_inode.is_dir() {
            error!("Dynamic linker path is a directory: {}", interp_path);
            return Err(ERRNO::EISDIR);
        }

        let interp_data = interp_inode.read_all();
        let interp_elf = xmas_elf::ElfFile::new(&interp_data).map_err(|_| ERRNO::ENOEXEC)?;
        let interp_entry = interp_elf.header.pt2.entry_point() as usize;
        let ph_count = interp_elf.header.pt2.ph_count();
        let interp_base = crate::config::INTERP_BASE;
        debug!("Loading interpreter at base address: {:#x}", interp_base);
        debug!("Interpreter original entry: {:#x}", interp_entry);

        for i in 0..ph_count {
            let ph = interp_elf.program_header(i).map_err(|_| ERRNO::ELIBBAD)?;
            if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
                let start_va: VirtAddr = (interp_base + ph.virtual_addr() as usize).into();
                let end_va: VirtAddr =
                    (interp_base + (ph.virtual_addr() + ph.mem_size()) as usize).into();
                let mut map_perm = MapPermission::U;
                let ph_flags = ph.flags();
                if ph_flags.is_read() {
                    map_perm |= MapPermission::R;
                }
                if ph_flags.is_write() {
                    map_perm |= MapPermission::W;
                }
                if ph_flags.is_execute() {
                    map_perm |= MapPermission::X;
                }

                debug!(
                    "mapping interpreter segment: [{:#x}, {:#x}) with flags {:?}",
                    usize::from(start_va),
                    usize::from(end_va),
                    map_perm
                );

                let vma = Vma::new_elf(start_va, end_va, map_perm);
                let page_off = start_va.page_offset();
                let raw = &interp_data[ph.offset() as usize
                    ..(ph.offset() + ph.file_size()) as usize];
                let padded: Vec<u8>;
                let seg_data: &[u8] = if page_off != 0 {
                    let mut buf = alloc::vec![0u8; page_off + raw.len()];
                    buf[page_off..].copy_from_slice(raw);
                    padded = buf;
                    &padded
                } else {
                    raw
                };
                memory_set
                    .insert_vma(vma, Some(seg_data))
                    .map_err(mm_error_to_errno)?;
            }
        }

        let relocated_entry = interp_base + interp_entry;
        debug!("Interpreter relocated entry: {:#x}", relocated_entry);
        debug!(
            "App PHDR vaddr: {:#x}, phnum: {}",
            app_load_info.phdr_vaddr,
            app_load_info.phnum
        );

        let auxv_extra = vec![
            (Auxv::Phdr, app_load_info.phdr_vaddr),
            (Auxv::Phent, app_load_info.phent_size),
            (Auxv::Phnum, app_load_info.phnum),
            (Auxv::Pagesz, crate::config::PAGE_SIZE),
            (Auxv::Base, interp_base),
            (Auxv::Entry, app_load_info.entry_point),
        ];

        (relocated_entry, auxv_extra)
    } else {
        debug!("Static linking, using application entry directly");
        debug!(
            "App PHDR vaddr: {:#x}, phnum: {}",
            app_load_info.phdr_vaddr,
            app_load_info.phnum
        );
        let auxv_extra = vec![
            (Auxv::Phdr, app_load_info.phdr_vaddr),
            (Auxv::Phent, app_load_info.phent_size),
            (Auxv::Phnum, app_load_info.phnum),
            (Auxv::Pagesz, crate::config::PAGE_SIZE),
            (Auxv::Entry, app_load_info.entry_point),
        ];
        (app_load_info.entry_point, auxv_extra)
    };

    Ok((memory_set, user_layout, final_entry, auxv_extra))
}

impl ProcessControlBlockInner {
    #[allow(unused)]
    /// get the address of app's page table
    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }
    fn nofile_limit(&self) -> usize {
        self.resource_limits.nofile.rlim_cur.min(usize::MAX as u64) as usize
    }
    pub fn available_fd_slots(&self) -> usize {
        let limit = self.nofile_limit();
        let occupied = self
            .fd_table
            .iter()
            .take(limit)
            .filter(|entry| entry.is_some())
            .count();
        limit.saturating_sub(occupied)
    }
    pub fn ensure_fd_capacity(&self, needed: usize) -> Result<(), ERRNO> {
        if self.available_fd_slots() < needed {
            Err(ERRNO::EMFILE)
        } else {
            Ok(())
        }
    }
    /// allocate a new file descriptor
    pub fn alloc_fd(&mut self) -> Result<usize, ERRNO> {
        self.alloc_fd_from(0)
    }
    /// allocate a new file descriptor no smaller than `min_fd`
    pub fn alloc_fd_from(&mut self, min_fd: usize) -> Result<usize, ERRNO> {
        let limit = self.nofile_limit();
        if min_fd >= limit {
            return Err(ERRNO::EMFILE);
        }
        if let Some(fd) = (min_fd..self.fd_table.len().min(limit)).find(|fd| self.fd_table[*fd].is_none()) {
            return Ok(fd);
        }
        if self.fd_table.len() < limit {
            if min_fd > self.fd_table.len() {
                self.fd_table.resize(min_fd, None);
            }
            self.fd_table.push(None);
            return Ok(self.fd_table.len() - 1);
        }
        Err(ERRNO::EMFILE)
    }
    pub fn address_space_bytes(&self) -> usize {
        self.memory_set.user_vma_bytes()
    }
    pub fn ensure_address_space_capacity(&self, additional: usize) -> Result<(), ERRNO> {
        let limit = self.resource_limits.address_space.rlim_cur;
        if limit == u64::MAX {
            return Ok(());
        }
        let current = self.address_space_bytes() as u128;
        let required = current.checked_add(additional as u128).ok_or(ERRNO::ENOMEM)?;
        if required > limit as u128 {
            Err(ERRNO::ENOMEM)
        } else {
            Ok(())
        }
    }
    /// 取走指定 fd 表项，供调用方在释放进程锁后再销毁底层文件对象。
    pub fn take_fd(&mut self, fd: usize) -> Option<FdEntry> {
        self.fd_table.get_mut(fd).and_then(Option::take)
    }
    /// 取走所有带 `FD_CLOEXEC` 的 fd 表项，供 `exec` 在锁外统一关闭。
    pub fn take_cloexec_fds(&mut self) -> Vec<FdEntry> {
        let mut removed = Vec::new();
        for entry in self.fd_table.iter_mut() {
            let should_close = entry
                .as_ref()
                .map(|entry| entry.flags.contains(FdFlags::CLOEXEC))
                .unwrap_or(false);
            if should_close {
                if let Some(removed_entry) = entry.take() {
                    removed.push(removed_entry);
                }
            }
        }
        removed
    }
    /// 取走当前进程持有的全部 fd 表项，供退出路径在锁外统一释放。
    pub fn take_all_fds(&mut self) -> Vec<FdEntry> {
        let mut removed = Vec::new();
        for entry in self.fd_table.iter_mut() {
            if let Some(removed_entry) = entry.take() {
                removed.push(removed_entry);
            }
        }
        removed
    }
    /// allocate a new task id
    pub fn alloc_tid(&mut self) -> usize {
        self.task_res_allocator.alloc()
    }
    /// deallocate a task id
    pub fn dealloc_tid(&mut self, tid: usize) {
        self.task_res_allocator.dealloc(tid)
    }
    /// the count of tasks(threads) in this process
    pub fn thread_count(&self) -> usize {
        self.tasks
            .iter()
            .filter_map(|task| task.as_ref())
            .filter(|task| task.inner_exclusive_access().exit_code.is_none())
            .count()
    }
    /// get a task with tid in this process
    pub fn get_task(&self, tid: usize) -> Arc<TaskControlBlock> {
        self.tasks[tid].as_ref().unwrap().clone()
    }

    pub fn is_zombie(&self) -> bool {
        self.is_zombie
    }
}

impl ProcessControlBlock {
    /// inner_exclusive_access
    pub fn inner_exclusive_access(&self) -> SpinNoIrqLockGuard<'_, ProcessControlBlockInner> {
        self.inner.lock()
    }
    /// Construct a task owned by this process without publishing it to the scheduler.
    pub fn create_task(
        self: &Arc<Self>,
        ustack_base: usize,
        alloc_user_res: bool,
        sched_attr: SchedAttr,
    ) -> Result<Arc<TaskControlBlock>, MmError> {
        Ok(Arc::new(TaskControlBlock::new(
            Arc::clone(self),
            ustack_base,
            alloc_user_res,
            sched_attr,
        )?))
    }

    /// Attach a created task to this process's task table without scheduling it.
    pub fn attach_task(self: &Arc<Self>, task: Arc<TaskControlBlock>) {
        let task_inner = task.inner_exclusive_access();
        let res = task_inner.res.as_ref().unwrap();
        let tid = res.tid;
        let thread_id = res.thread_id();
        drop(task_inner);
        let mut inner = self.inner_exclusive_access();
        while inner.tasks.len() <= tid {
            inner.tasks.push(None);
        }
        inner.tasks[tid] = Some(Arc::clone(&task));
        drop(inner);
        insert_into_tid2task(thread_id, &task);
    }

    /// new process from elf file
    pub fn new(exec_path: String) -> Arc<Self> {
        trace!("kernel: ProcessControlBlock::new");
        let init_envs: Vec<String> = INIT_ENV.iter().map(|entry| String::from(*entry)).collect();
        let init_argv = vec![String::from(exec_path.as_str())];
        let resolved = resolve_init_image("/", exec_path.as_str(), init_argv, 0)
            .expect("failed to resolve init image");
        let (memory_set, user_layout, entry_point, auxv_extra) =
            load_process_image(resolved.elf_data.as_slice(), "/").expect("failed to build init process address space");
        let ustack_base = user_layout.ustack_base;
        let vm_layout = ProcessVmLayout::from_user_layout(user_layout);
        // allocate a pid
        let pid_handle = pid_alloc();
        let mut cred = Credentials::root();
        // init 是自身会话与进程组的首领（sid == pgid == pid），与 Linux 一致；
        // 这些值会被 fork 继承，避免出现非法的 pgid/sid 0。
        cred.sid = pid_handle.0 as u32;
        cred.pgid = pid_handle.0 as u32;
        let process = Arc::new(Self {
            pid: pid_handle,
            inner: SpinNoIrqLock::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    vm_layout,
                    parent: None,
                    children: Vec::new(),
                    exit_reason: ExitReason::Exit(0),
                    fd_table: new_stdio_files(),
                    resource_limits: ResourceLimits::default(),
                    pending_signals: SignalBit::empty(),
                    pending_siginfo: [SigInfo::default(); MAX_SIG + 1],
                    signal_actions: SignalActions::default(),
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                    deadlock_enabled: false,
                    mutex_detector: DeadlockDetector::new(),
                    semaphore_detector: DeadlockDetector::new(),
                    cwd: String::from(INIT_CWD),
                    exec_path,
                    environment: init_envs.clone(),
                    umask: DEFAULT_UMASK,
                    cred,
                    keyrings: ProcessKeyrings::default(),
                    user_time: 0,
                    kernel_time: 0,
                    child_user_time: 0,
                    child_kernel_time: 0,
                    accounting_state: CpuAccountingState::Inactive,
                    accounting_timestamp: 0,
                    accounting_start_time_ns: get_realtime_ns(),
                    itimer_real: ItimerState::default(),
                    itimer_virtual: ItimerState::default(),
                    itimer_prof: ItimerState::default(),
                    robust_list: RobustList { head: 0, len: 0 },
                    shm_attachments: Vec::new(),
                }),
            wait_exit_queue: Arc::new(WaitQueue::new()),
        });
        // create a main thread, we should allocate ustack and trap_cx here
        let task = process
            .create_task(ustack_base, true, SchedAttr::default())
            .expect("failed to allocate init task");
        // prepare trap_cx of main thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        let ustack_top = task_inner.res.as_ref().unwrap().ustack_top();
        let kstack_top = task.kstack.get_top();
        drop(task_inner);
        let user_sp = init_user_stack_from_strings(
            process.inner_exclusive_access().get_user_token(),
            ustack_top,
            resolved.argv.as_slice(),
            init_envs.as_slice(),
            auxv_extra.as_slice(),
        );
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.lock().token(),
            kstack_top,
            trap_handler as usize,
        );
        // add main thread to the process
        process.attach_task(Arc::clone(&task));
        insert_into_pid2process(process.getpid(), Arc::clone(&process));
        // publish main thread to scheduler only after the process/task state is fully initialized
        add_task(task);
        process
    }

    /// Only support processes with a single thread.
    pub fn exec(
        self: &Arc<Self>,
        elf_data: &[u8],
        args: Vec<String>,
        envs: Vec<String>,
        exec_path: String,
    ) -> Result<(), ERRNO> {
        trace!("kernel: exec");
        assert_eq!(self.inner_exclusive_access().thread_count(), 1);

        trace!("kernel: exec .. load process image");
        let cwd = self.inner_exclusive_access().cwd.clone();
        let (memory_set, user_layout, final_entry, auxv_extra) =
            load_process_image(elf_data, cwd.as_str())?;

        let ustack_base = user_layout.ustack_base;
        let new_token = memory_set.token();
        let vm_layout = ProcessVmLayout::from_user_layout(user_layout);
        // substitute memory_set
        trace!("kernel: exec .. substitute memory_set");
        let (mut old_memory_set, old_token, old_mask, cloexec_entries, old_shm_attachments) = {
            let mut inner = self.inner_exclusive_access();
            let old_memory_set = core::mem::replace(&mut inner.memory_set, memory_set);
            let old_token = old_memory_set.token();
            let old_mask = old_memory_set.loaded_user_harts();
            inner.vm_layout = vm_layout;
            inner.exec_path = exec_path;
            inner.environment = envs.clone();
            // POSIX: on exec, reset all user-defined signal handlers to SIG_DFL.
            // SIG_IGN dispositions are preserved across exec.
            for action in inner.signal_actions.table.iter_mut() {
                if action.handler != SIG_IGN {
                    *action = SignalAction::default();
                }
            }
            inner.pending_signals = SignalBit::empty();
            inner.pending_siginfo = [SigInfo::default(); MAX_SIG + 1];
            // 关键点：真正销毁 `FileDescription` 可能触发同步回写和块设备等待，
            // 这里必须先把表项挪出进程自旋锁，再在锁外执行 drop。
            let cloexec_entries = inner.take_cloexec_fds();
            let old_shm_attachments = core::mem::take(&mut inner.shm_attachments);
            (old_memory_set, old_token, old_mask, cloexec_entries, old_shm_attachments)
        };
        debug!("[mmap] exec teardown old memory_set before installing new user context");
        let old_batch = old_memory_set.recycle_data_pages_deferred();
        DeferredUserReclaim::new(old_token, old_mask, old_batch).flush_then_release();
        drop(cloexec_entries);
        for attachment in old_shm_attachments {
            ipc::detach_segment(attachment.shmid);
        }
        // then we alloc user resource for main thread again
        // since memory_set has been changed
        trace!("kernel: exec .. alloc user resource for main thread again");
        let task = self.inner_exclusive_access().get_task(0);
        let mut task_inner = task.inner_exclusive_access();
        task_inner.res.as_mut().unwrap().ustack_base = ustack_base;
        task_inner
            .res
            .as_mut()
            .unwrap()
            .alloc_user_res()
            .map_err(mm_error_to_errno)?;
        task_inner.trap_cx_ppn = task_inner.res.as_mut().unwrap().trap_cx_ppn();
        task_inner.pending_signals = SignalBit::empty();
        task_inner.pending_siginfo = [SigInfo::default(); MAX_SIG + 1];
        // push arguments on user stack — Linux ELF ABI layout:
        //   [sp]  argc
        //         argv[0..argc-1], NULL
        //         envp[0..envc-1], NULL
        //         auxv pairs..., AT_NULL
        //         random bytes, argument/environment strings
        trace!("kernel: exec .. push arguments on user stack");
        let user_sp = init_user_stack_from_strings(
            new_token,
            task_inner.res.as_mut().unwrap().ustack_top(),
            args.as_slice(),
            envs.as_slice(),
            auxv_extra.as_slice(),
        );

        // initialize trap_cx
        trace!("kernel: exec .. initialize trap_cx with entry={:#x}", final_entry);
        let mut trap_cx = TrapContext::app_init_context(
            final_entry,
            user_sp,
            KERNEL_SPACE.lock().token(),
            task.kstack.get_top(),
            trap_handler as usize,
        );
        // RISC-V glibc _start treats a0 as rtld_fini and reads argc/argv from the stack.
        trap_cx.x[10] = 0;
        trap_cx.x[11] = 0;
        debug!(
            "kernel: exec trap init entry={:#x} sp={:#x} a0={:#x} a1={:#x}",
            final_entry,
            user_sp,
            trap_cx.x[10],
            trap_cx.x[11]
        );
        *task_inner.get_trap_cx() = trap_cx;
        Ok(())
    }
    /// 按 Linux `clone` 的进程分支创建子进程。
    ///
    /// 当前只支持 fork-like 语义；`child_stack` 非 0 时作为子进程返回用户态的 `sp`。
    pub fn clone_process(
        self: &Arc<Self>,
        child_stack: usize,
        child_tls: Option<usize>,
        child_set_tid: Option<usize>,
    ) -> Result<Arc<Self>, ERRNO> {
        trace!("kernel: clone_process");
        let mut parent = self.inner_exclusive_access();
        // assert_eq!(parent.thread_count(), 1);
        if parent.thread_count() != 1 {
            warn!(
                "clone_process with multiple threads is not fully supported: parent_pid={} thread_count={}",
                self.getpid(),
                parent.thread_count()
            );
            return Err(ERRNO::EINVAL)
        }
        debug!(
            "[cow] clone_process begin: parent_pid={} parent_threads={}",
            self.getpid(),
            parent.thread_count()
        );
        // clone parent's memory_set completely including trampoline/ustacks/trap_cxs
        let (memory_set, parent_tlb_needs_flush) =
            MemorySet::from_existed_user(&mut parent.memory_set).map_err(mm_error_to_errno)?;
        let parent_token = parent.memory_set.token();
        let parent_mask = if parent_tlb_needs_flush {
            parent.memory_set.loaded_user_harts()
        } else {
            0
        };
        let vm_layout = parent.vm_layout;
        let cred = parent.cred;
        let parent_signal_actions = parent.signal_actions.clone();
        let parent_cwd = parent.cwd.clone();
        let parent_exec_path = parent.exec_path.clone();
        let parent_umask = parent.umask;
        let parent_keyrings = parent.keyrings;
        let parent_shm_attachments = parent.shm_attachments.clone();
        // alloc a pid
        let pid = pid_alloc();
        // copy fd table
        let mut new_fd_table: Vec<Option<FdEntry>> = Vec::new();
        for fd in parent.fd_table.iter() {
            if let Some(entry) = fd {
                new_fd_table.push(Some(entry.clone()));
            } else {
                new_fd_table.push(None);
            }
        }
        // create child process pcb
        let child = Arc::new(Self {
            pid,
            inner: SpinNoIrqLock::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    vm_layout,
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    exit_reason: ExitReason::Exit(0),
                    fd_table: new_fd_table,
                    resource_limits: parent.resource_limits,
                    pending_signals: SignalBit::empty(),
                    pending_siginfo: [SigInfo::default(); MAX_SIG + 1],
                    signal_actions: parent_signal_actions,
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                    deadlock_enabled: false,
                    mutex_detector: DeadlockDetector::new(),
                    semaphore_detector: DeadlockDetector::new(),
                    cwd: parent_cwd,
                    exec_path: parent_exec_path,
                    environment: parent.environment.clone(),
                    umask: parent_umask,
                    cred,
                    keyrings: parent_keyrings,
                    user_time: 0,
                    kernel_time: 0,
                    child_user_time: 0,
                    child_kernel_time: 0,
                    accounting_state: CpuAccountingState::Inactive,
                    accounting_timestamp: 0,
                    accounting_start_time_ns: get_realtime_ns(),
                    itimer_real: ItimerState::default(),
                    itimer_virtual: ItimerState::default(),
                    itimer_prof: ItimerState::default(),
                    robust_list: RobustList { head: 0, len: 0 },
                    shm_attachments: parent_shm_attachments.clone(),
                }),
            wait_exit_queue: Arc::new(WaitQueue::new())
        });
        // add child
        parent.children.push(Arc::clone(&child));
        let parent_task = parent.get_task(0);
        let parent_task_inner = parent_task.inner_exclusive_access();
        let parent_ustack_base = parent_task_inner.res.as_ref().unwrap().ustack_base();
        let parent_sched_attr = parent_task_inner.sched_attr();
        let parent_affinity_mask = parent_task_inner.sched.cpu_affinity_mask;
        let parent_signal_mask = parent_task_inner.signal_mask;
        drop(parent_task_inner);
        drop(parent);
        if parent_mask != 0 {
            debug!(
                "[tlb] fork shootdown parent mm: parent_pid={} token={:#x} mask={:#b}",
                self.getpid(),
                parent_token,
                parent_mask
            );
            shootdown(parent_mask, ShootdownKind::AddressSpace { satp: parent_token });
        }
        debug!(
            "[cow] clone_process created child process: parent_pid={} child_pid={}",
            self.getpid(),
            child.getpid()
        );
        // create main thread of child process
        let task = child.create_task(
            parent_ustack_base,
            // here we do not allocate trap_cx or ustack again
            // but mention that we allocate a new kstack here
            false,
            parent_sched_attr,
        ).map_err(|err| {
            self.inner_exclusive_access()
                .children
                .retain(|candidate| !Arc::ptr_eq(candidate, &child));
            mm_error_to_errno(err)
        })?;
        {
            let mut task_inner = task.inner_exclusive_access();
            task_inner.sched.cpu_affinity_mask = parent_affinity_mask;
            task_inner.signal_mask = parent_signal_mask;
        }
        // attach task to child process before publishing it
        child.attach_task(Arc::clone(&task));
        // Finalize the child's trap context before publishing it to the scheduler.
        // Otherwise, on SMP the child may run on another hart before `sys_fork`
        // patches the inherited return register, breaking fork semantics.
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        trap_cx.kernel_sp = task.kstack.get_top();
        trap_cx.x[10] = 0;
        if child_stack != 0 {
            // Linux clone ABI 要求子进程从指定用户栈继续执行。
            trap_cx.x[2] = child_stack;
        }
        if let Some(tls) = child_tls {
            // RISC-V 用户态 TLS 指针使用 tp，也就是 x4。
            trap_cx.x[4] = tls;
        }
        drop(task_inner);
        if let Some(child_tid_ptr) = child_set_tid {
            let child_tid_value = child.getpid() as i32;
            if let Err(err) =
                write_pod_to_process_user(&child, child_tid_ptr as *mut i32, &child_tid_value)
            {
                self.inner_exclusive_access()
                    .children
                    .retain(|candidate| !Arc::ptr_eq(candidate, &child));
                return Err(err);
            }
        }
        for attachment in parent_shm_attachments {
            ipc::retain_attached_segment(attachment.shmid);
        }
        child.register_existing_file_mappings();
        debug!(
            "[cow] clone_process complete: parent_pid={} child_pid={}",
            self.getpid(),
            child.getpid()
        );
        insert_into_pid2process(child.getpid(), Arc::clone(&child));
        add_task(task);
        Ok(child)
    }

    /// Create a child process directly from elf image.
    pub fn spawn(self: &Arc<Self>, elf_data: &[u8], exec_path: String) -> Result<Arc<Self>, ERRNO> {
        let (memory_set, user_layout, load_info) =
            MemorySet::from_elf(elf_data).map_err(mm_error_to_errno)?;
        let entry_point = load_info.entry_point;
        let ustack_base = user_layout.ustack_base;
        let vm_layout = ProcessVmLayout::from_user_layout(user_layout);
        let mut parent = self.inner_exclusive_access();
        let cred = parent.cred;
        let parent_keyrings = parent.keyrings;
        let pid = pid_alloc();
        let mut new_fd_table: Vec<Option<FdEntry>> = Vec::new();
        for fd in parent.fd_table.iter() {
            if let Some(entry) = fd {
                new_fd_table.push(Some(entry.clone()));
            } else {
                new_fd_table.push(None);
            }
        }
        let child = Arc::new(Self {
            pid,
            inner: SpinNoIrqLock::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    vm_layout,
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    exit_reason: ExitReason::Exit(0),
                    fd_table: new_fd_table,
                    resource_limits: parent.resource_limits,
                    pending_signals: SignalBit::empty(),
                    pending_siginfo: [SigInfo::default(); MAX_SIG + 1],
                    signal_actions: parent.signal_actions.clone(),
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                    deadlock_enabled: false,
                    mutex_detector: DeadlockDetector::new(),
                    semaphore_detector: DeadlockDetector::new(),
                    cwd: parent.cwd.clone(), // 同fork，继承自父进程
                    exec_path,
                    environment: parent.environment.clone(),
                    umask: parent.umask,
                    cred,
                    keyrings: parent_keyrings,
                    user_time: 0,
                    kernel_time: 0,
                    child_user_time: 0,
                    child_kernel_time: 0,
                    accounting_state: CpuAccountingState::Inactive,
                    accounting_timestamp: 0,
                    accounting_start_time_ns: get_realtime_ns(),
                    itimer_real: ItimerState::default(),
                    itimer_virtual: ItimerState::default(),
                    itimer_prof: ItimerState::default(),
                    robust_list: RobustList { head: 0, len: 0 },
                    shm_attachments: Vec::new(),
                }),
            wait_exit_queue: Arc::new(WaitQueue::new())
        });
        parent.children.push(Arc::clone(&child));
        let parent_task = parent.get_task(0);
        let parent_task_inner = parent_task.inner_exclusive_access();
        let parent_sched_attr = parent_task_inner.sched_attr();
        let parent_affinity_mask = parent_task_inner.sched.cpu_affinity_mask;
        let parent_signal_mask = parent_task_inner.signal_mask;
        drop(parent_task_inner);
        drop(parent);

        let task = child
            .create_task(ustack_base, true, parent_sched_attr)
            .map_err(|err| {
                self.inner_exclusive_access()
                    .children
                    .retain(|candidate| !Arc::ptr_eq(candidate, &child));
                mm_error_to_errno(err)
            })?;
        {
            let mut task_inner = task.inner_exclusive_access();
            task_inner.sched.cpu_affinity_mask = parent_affinity_mask;
            task_inner.signal_mask = parent_signal_mask;
        }
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        let ustack_top = task_inner.res.as_ref().unwrap().ustack_top();
        let kstack_top = task.kstack.get_top();
        drop(task_inner);
        let user_sp = init_user_stack(child.inner_exclusive_access().get_user_token(), ustack_top, &["spawn"]);
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.lock().token(),
            kstack_top,
            trap_handler as usize,
        );

        child.attach_task(Arc::clone(&task));
        insert_into_pid2process(child.getpid(), Arc::clone(&child));
        add_task(task);
        Ok(child)
    }
    /// get pid
    pub fn getpid(&self) -> usize {
        self.pid.0
    }
    /// Get absolute path of the last executed image.
    pub fn exec_path(&self) -> String {
        self.inner.lock().exec_path.clone()
    }
    pub fn getuid(&self) -> u32 {
        self.inner.lock().cred.uid
    }
    /// get euid
    pub fn geteuid(&self) -> u32 {
        self.inner.lock().cred.euid
    }
    /// get suid
    pub fn getsuid(&self) -> u32 {
        self.inner.lock().cred.suid
    }
    /// get gid
    pub fn getgid(&self) -> u32 {
        self.inner.lock().cred.gid
    }
    /// get egid
    pub fn getegid(&self) -> u32 {
        self.inner.lock().cred.egid
    }

    pub fn setuid_cred(&self, uid: u32) {
        let mut inner = self.inner.lock();
        inner.cred.uid = uid;
        inner.cred.euid = uid;
        inner.cred.suid = uid;
    }

    pub fn setegid(&self, egid: u32) {
        self.inner.lock().cred.egid = egid;
    }

    pub fn getsid(&self) -> u32 {
        self.inner.lock().cred.sid
    }

    pub fn setsid(&self, sid: u32) {
        self.inner.lock().cred.sid = sid;
    }

    pub fn getpgid(&self) -> u32 {
        self.inner.lock().cred.pgid
    }

    pub fn setpgid(&self, pgid: u32) {
        self.inner.lock().cred.pgid = pgid;
    }

    pub fn umask(&self) -> u32 {
        self.inner.lock().umask
    }

    pub fn set_umask(&self, umask: u32) {
        self.inner.lock().umask = umask & 0o777;
    }

    /// map an anonymous area with given permission, return true if success
    pub fn mmap(&self, start: VirtAddr, end: VirtAddr, perm: MapPermission) -> Result<(), ERRNO> {
        let len = usize::from(end).saturating_sub(usize::from(start));
        let mut inner = self.inner.lock();
        inner.ensure_address_space_capacity(len)?;
        inner.memory_set.mmap_anonymous(start, end, perm).map_err(mm_error_to_errno)
    }
    /// 登记一个 file-backed 映射区域，后续由缺页路径按需接入 page cache。
    pub fn mmap_file(
        self: &Arc<Self>,
        start: VirtAddr,
        end: VirtAddr,
        perm: MapPermission,
        file: Arc<FileDescription>,
        pgoff: usize,
        shared: bool,
    ) -> Result<(), ERRNO> {
        let len = usize::from(end).saturating_sub(usize::from(start));
        {
            let mut inner = self.inner.lock();
            inner.ensure_address_space_capacity(len)?;
            inner
                .memory_set
                .mmap_file(start, end, perm, file.clone(), pgoff, shared)
                .map_err(mm_error_to_errno)?;
        }
        if let Some(inode) = file.backing_inode() {
            register_file_mapping(&inode, self);
        }
        Ok(())
    }

    /// 失效当前进程中因 truncate 越过 EOF 的 file-backed 用户映射。
    pub fn invalidate_file_mappings_after_truncate(&self, inode: InodeKey, new_size: usize) {
        let reclaim = {
            let mut inner = self.inner.lock();
            let token = inner.memory_set.token();
            let mask = inner.memory_set.loaded_user_harts();
            let batch = inner
                .memory_set
                .invalidate_file_mappings_after_truncate_deferred(inode, new_size);
            DeferredUserReclaim::new(token, mask, batch)
        };
        reclaim.flush_then_release();
    }

    /// 登记当前地址空间中已有的 file-backed VMA，供 fork 继承映射后补齐反向映射。
    fn register_existing_file_mappings(self: &Arc<Self>) {
        let files = {
            let inner = self.inner.lock();
            inner
                .memory_set
                .vmas
                .values()
                .filter_map(|area| area.file.as_ref())
                .filter_map(|file| file.file.backing_inode())
                .collect::<Vec<_>>()
        };
        for inode in files {
            register_file_mapping(&inode, self);
        }
    }
    /// unmap an area. return true if success
    pub fn munmap(&self, start: VirtAddr, end: VirtAddr) -> bool {
        let Some(reclaim) = ({
            let mut inner = self.inner.lock();
            let token = inner.memory_set.token();
            let mask = inner.memory_set.loaded_user_harts();
            inner
                .memory_set
                .munmap_deferred(start, end)
                .map(|batch| DeferredUserReclaim::new(token, mask, batch))
        }) else {
            return false;
        };
        reclaim.flush_then_release();
        true
    }

    /// Record a successful SysV shared-memory attachment.
    pub fn add_shm_attachment(&self, attachment: ShmAttachment) {
        self.inner.lock().shm_attachments.push(attachment);
    }

    /// Remove the attachment whose mapping starts at `addr`.
    pub fn remove_shm_attachment_by_addr(&self, addr: usize) -> Option<ShmAttachment> {
        let mut inner = self.inner.lock();
        let idx = inner.shm_attachments.iter().position(|entry| entry.addr == addr)?;
        Some(inner.shm_attachments.remove(idx))
    }

    /// Drain all SysV shared-memory attachments during exec/exit teardown.
    pub fn take_all_shm_attachments(&self) -> Vec<ShmAttachment> {
        let mut inner = self.inner.lock();
        core::mem::take(&mut inner.shm_attachments)
    }

    /// 对当前进程地址空间中的指定范围执行 `msync`。
    pub fn msync(&self, start: VirtAddr, end: VirtAddr) -> Result<(), ERRNO> {
        let inner = self.inner.lock();
        inner.memory_set.msync_range(start, end)
    }

    /// 处理当前进程的私有页写时复制缺页。
    pub fn handle_private_cow_fault(
        &self,
        fault_addr: usize,
    ) -> Result<PageFaultHandled, MmError> {
        let (handled, reclaim) = {
            let mut inner = self.inner.lock();
            let token = inner.memory_set.token();
            let mask = inner.memory_set.loaded_user_harts();
            let (handled, batch) = inner
                .memory_set
                .handle_private_cow_fault(VirtAddr::from(fault_addr))?;
            (
                handled,
                batch.map(|batch| DeferredUserReclaim::new(token, mask, batch)),
            )
        };
        if let Some(reclaim) = reclaim {
            reclaim.flush_then_release();
        }
        Ok(handled)
    }
    /// 处理当前进程的用户匿名/heap/user stack 懒分配缺页。
    pub fn handle_lazy_user_fault(
        &self,
        fault_addr: usize,
        access: PageFaultAccess,
    ) -> Result<PageFaultHandled, MmError> {
        self.inner
            .lock()
            .memory_set
            .handle_lazy_user_fault(VirtAddr::from(fault_addr), access)
    }
    /// 处理当前进程的 file-backed 缺页。
    pub fn handle_file_page_fault(
        &self,
        fault_addr: usize,
        access: PageFaultAccess,
    ) -> Result<PageFaultHandled, MmError> {
        debug!(
            "[mmap] page fault enter: pid={} addr={:#x} access={:?}",
            self.getpid(),
            fault_addr,
            access
        );
        if access == PageFaultAccess::Write {
            let notified = {
                let mut inner = self.inner.lock();
                inner
                    .memory_set
                    .handle_shared_write_fault(VirtAddr::from(fault_addr))
            };
            if notified {
                debug!(
                    "[mmap] page fault resolved by shared write-notify: pid={} addr={:#x}",
                    self.getpid(),
                    fault_addr
                );
                return Ok(PageFaultHandled::Handled);
            }
        }
        let plan = {
            let inner = self.inner.lock();
            inner
                .memory_set
                .prepare_file_page_fault(VirtAddr::from(fault_addr), access)
        };
        let Some(plan) = plan else {
            debug!(
                "[mmap] page fault miss: pid={} addr={:#x} access={:?}",
                self.getpid(),
                fault_addr,
                access
            );
            return Ok(PageFaultHandled::NotHandled);
        };
        let Some(inode) = plan.file.backing_inode() else {
            return Ok(PageFaultHandled::NotHandled);
        };
        let Some(mapping) = mapping_for_inode(&inode) else {
            return Ok(PageFaultHandled::NotHandled);
        };
        let page_start = plan.page_idx as usize * PAGE_SIZE;
        let file_size = mapping.size();
        if page_start >= file_size {
            debug!(
                "[mmap] file-backed fault beyond EOF: pid={} vpn={:#x} page_idx={} page_start={:#x} file_size={:#x}",
                self.getpid(),
                plan.vpn.0,
                plan.page_idx,
                page_start,
                file_size
            );
            return Err(MmError::BeyondFileEnd);
        };
        debug!(
            "[mmap] page fault lazy load: pid={} vpn={:#x} page_idx={} shared={} path={:?}",
            self.getpid(),
            plan.vpn.0,
            plan.page_idx,
            plan.shared,
            plan.file.path()
        );
        let page = mapping.try_get_page(plan.page_idx)?;
        let mut inner = self.inner.lock();
        // TODO：这里目前只靠二次匹配校验 VMA 是否仍然有效；
        // 后续补齐更严格的 `mm_seq` 代际校验与跨 hart TLB shootdown。
        let committed = if plan.shared {
            inner.memory_set.map_shared_file_page(&plan, page)
        } else {
            inner.memory_set.map_private_file_page(&plan, page)
        }?;
        debug!(
            "[mmap] page fault commit result: pid={} vpn={:#x} shared={} committed={}",
            self.getpid(),
            plan.vpn.0,
            plan.shared,
            committed == PageFaultHandled::Handled
        );
        Ok(committed)
    }

    /// change permissions of a mapped range. return true if success
    pub fn mprotect(&self, start: VirtAddr, end: VirtAddr, perm: MapPermission) -> bool {
        let (ok, token, mask) = {
            let mut inner = self.inner.lock();
            let ok = inner.memory_set.mprotect_range(start, end, perm);
            let token = inner.memory_set.token();
            // 锁内只快照目标 hart，锁外再等待 ack。远端用户态 IPI 进入
            // trap_handler 前会调用 enter_kernel()，持进程锁等待会造成死锁。
            //
            // 这个快照依赖 trap 入口本地 sfence.vma：快照后才返回用户态的 hart
            // 已经经过本地 flush，不需要包含在本次远端 shootdown 里。
            let mask = if ok {
                inner.memory_set.loaded_user_harts()
            } else {
                0
            };
            (ok, token, mask)
        };
        if ok && mask != 0 {
            debug!(
                "[tlb] mprotect shootdown: pid={} token={:#x} mask={:#b}",
                self.getpid(),
                token,
                mask
            );
            shootdown(mask, ShootdownKind::AddressSpace { satp: token });
        }
        ok
    }

    /// 返回当前进程用于 `mmap(NULL, ...)` 的默认起始基址。
    pub fn mmap_base(&self) -> usize {
        self.inner.lock().vm_layout.mmap_base
    }

    /// 按目标地址调整程序 break，返回调整后的当前 break。
    pub fn set_program_brk(&self, new_brk: usize) -> usize {
        let (result_brk, reclaim) = {
            let mut inner = self.inner.lock();
            let old_brk = inner.vm_layout.brk;
            debug!("brk: old addr = {:#x}, new addr = {:#x}", old_brk, new_brk);
            // TODO： 特殊情况，如果输入 new_brk 为 0，应该返回当前 brk 而不进行调整，用于兼容测试
            if new_brk == 0 {
                return old_brk;
            }
            if new_brk < inner.vm_layout.start_brk
                || new_brk >= inner.vm_layout.mmap_base
                || new_brk >= inner.vm_layout.start_stack
            {
                warn!(
                    "set_program_brk: FAILED at range check: start_brk={:#x}, mmap_base={:#x}, start_stack={:#x}, new_brk={:#x}",
                    inner.vm_layout.start_brk,
                    inner.vm_layout.mmap_base,
                    inner.vm_layout.start_stack,
                    new_brk
                );
                return old_brk;
            }
            if new_brk == old_brk {
                return old_brk;
            }

            let heap_start = VirtAddr::from(inner.vm_layout.start_brk);
            let new_brk_va = VirtAddr::from(new_brk);
            let old_brk_va = VirtAddr::from(old_brk);
            let old_end_vpn = old_brk_va.ceil();
            let new_end_vpn = new_brk_va.ceil();
            let mut batch = None;
            if new_end_vpn > old_end_vpn {
                let additional = (new_end_vpn.0 - old_end_vpn.0) * PAGE_SIZE;
                if inner.ensure_address_space_capacity(additional).is_err() {
                    warn!("set_program_brk: FAILED at ensure address space capacity");
                    return old_brk;
                }
            }

            if new_brk > old_brk {
                let success = inner.memory_set.append_metadata_to(heap_start, new_brk_va)
                    || inner
                        .memory_set
                        .register_vma_metadata(Vma::new_heap(
                            heap_start,
                            new_brk_va,
                            MapPermission::R | MapPermission::W | MapPermission::U,
                        ))
                        .is_ok();
                if !success {
                    warn!("set_program_brk: FAILED at append metadata to heap ({:#x} -> {:#x})", old_brk, new_brk);
                    return old_brk;
                }
            } else if new_brk == inner.vm_layout.start_brk {
                batch = Some(inner
                    .memory_set
                    .remove_vma_with_start_vpn_user_deferred(heap_start.floor()));
            } else if old_end_vpn != new_end_vpn {
                let Some(shrink_batch) = inner.memory_set.shrink_to_deferred(heap_start, new_brk_va) else {
                    warn!("set_program_brk: FAILED at shrink to deferred");
                    return old_brk;
                };
                batch = Some(shrink_batch);
            }

            inner.vm_layout.brk = new_brk;
            let reclaim = batch.map(|batch| {
                // 锁内快照仍在用户态运行该 mm 的 hart，锁外等待 shootdown ack。
                DeferredUserReclaim::new(
                    inner.memory_set.token(),
                    inner.memory_set.loaded_user_harts(),
                    batch,
                )
            });
            (new_brk, reclaim)
        };
        if let Some(reclaim) = reclaim {
            reclaim.flush_then_release();
        }
        result_brk
    }

    /// Mark this process as running in kernel mode from `now`.
    pub fn resume_in_kernel(&self, now: usize) {
        let mut inner = self.inner.lock();
        inner.accounting_state = CpuAccountingState::Kernel;
        inner.accounting_timestamp = now;
    }

    /// Account the user-mode slice that ended at `now`, then switch to kernel mode.
    pub fn enter_kernel(&self, now: usize) {
        let mut inner = self.inner.lock();
        // trap 入口已经切到内核页表并做过本地 sfence.vma，此 hart 不再持有该用户 mm。
        inner.memory_set.mark_user_unloaded(crate::hart::hartid());
        match inner.accounting_state {
            CpuAccountingState::User => {
                inner.user_time = inner
                    .user_time
                    .saturating_add(now.saturating_sub(inner.accounting_timestamp));
            }
            CpuAccountingState::Kernel | CpuAccountingState::Inactive => {}
        }
        inner.accounting_state = CpuAccountingState::Kernel;
        inner.accounting_timestamp = now;
    }

    /// Account the kernel-mode slice that ended at `now`, then switch to user mode.
    pub fn enter_user(&self, now: usize) {
        let mut inner = self.inner.lock();
        match inner.accounting_state {
            CpuAccountingState::Kernel => {
                inner.kernel_time = inner
                    .kernel_time
                    .saturating_add(now.saturating_sub(inner.accounting_timestamp));
            }
            CpuAccountingState::User | CpuAccountingState::Inactive => {}
        }
        inner.accounting_state = CpuAccountingState::User;
        inner.accounting_timestamp = now;
        // 即将跳回用户态，后续其他 hart 修改该 mm 时需要把当前 hart 作为 shootdown 目标。
        inner.memory_set.mark_user_loaded(crate::hart::hartid());
    }

    /// Flush the current running slice into the corresponding accumulator.
    pub fn pause_cpu_accounting(&self, now: usize) {
        let mut inner = self.inner.lock();
        match inner.accounting_state {
            CpuAccountingState::User => {
                inner.user_time = inner
                    .user_time
                    .saturating_add(now.saturating_sub(inner.accounting_timestamp));
            }
            CpuAccountingState::Kernel => {
                inner.kernel_time = inner
                    .kernel_time
                    .saturating_add(now.saturating_sub(inner.accounting_timestamp));
            }
            CpuAccountingState::Inactive => {}
        }
        inner.accounting_state = CpuAccountingState::Inactive;
        inner.accounting_timestamp = now;
    }

    /// Snapshot process times as raw counters: (utime, stime, cutime, cstime).
    pub fn times_snapshot(&self, now: usize) -> (usize, usize, usize, usize) {
        let inner = self.inner.lock();
        let active_delta = now.saturating_sub(inner.accounting_timestamp);
        let (user_time, kernel_time) = match inner.accounting_state {
            CpuAccountingState::User => (
                inner.user_time.saturating_add(active_delta),
                inner.kernel_time,
            ),
            CpuAccountingState::Kernel => (
                inner.user_time,
                inner.kernel_time.saturating_add(active_delta),
            ),
            CpuAccountingState::Inactive => (inner.user_time, inner.kernel_time),
        };
        (
            user_time,
            kernel_time,
            inner.child_user_time,
            inner.child_kernel_time,
        )
    }

    /// 按指定 itimer 类型读取“剩余时间 + 周期间隔”。
    ///
    /// 返回值均为纳秒。
    pub fn get_itimer_state(
        &self,
        which: i32,
        now_raw: usize,
        now_realtime_ns: u64,
    ) -> Result<(u64, u64), ERRNO> {
        let inner = self.inner.lock();
        let active_delta = now_raw.saturating_sub(inner.accounting_timestamp);
        let (user_raw, kernel_raw) = match inner.accounting_state {
            CpuAccountingState::User => (
                inner.user_time.saturating_add(active_delta),
                inner.kernel_time,
            ),
            CpuAccountingState::Kernel => (
                inner.user_time,
                inner.kernel_time.saturating_add(active_delta),
            ),
            CpuAccountingState::Inactive => (inner.user_time, inner.kernel_time),
        };
        let user_ns = raw_counter_to_ns(user_raw);
        let kernel_ns = raw_counter_to_ns(kernel_raw);
        let (timer, now_ns) = match which {
            0 => (&inner.itimer_real, now_realtime_ns),
            1 => (&inner.itimer_virtual, user_ns),
            2 => (&inner.itimer_prof, user_ns.saturating_add(kernel_ns)),
            _ => return Err(ERRNO::EINVAL),
        };
        Ok((
            itimer_remaining_ns(timer.deadline_ns, now_ns),
            timer.interval_ns,
        ))
    }

    /// 按指定 itimer 类型设置 timer。
    ///
    /// - `new_value = None`：仅查询旧值（等价 `setitimer(value=NULL, ovalue=...)` 的内核侧状态读取）。
    /// - `new_value = Some((value_ns, interval_ns))`：应用新值，并返回旧值。
    ///
    /// 返回旧值 `(old_value_ns, old_interval_ns)`，单位均为纳秒。
    pub fn set_itimer_state(
        &self,
        which: i32,
        now_raw: usize,
        now_realtime_ns: u64,
        new_value: Option<(u64, u64)>,
    ) -> Result<(u64, u64), ERRNO> {
        let mut inner = self.inner.lock();
        let active_delta = now_raw.saturating_sub(inner.accounting_timestamp);
        let (user_raw, kernel_raw) = match inner.accounting_state {
            CpuAccountingState::User => (
                inner.user_time.saturating_add(active_delta),
                inner.kernel_time,
            ),
            CpuAccountingState::Kernel => (
                inner.user_time,
                inner.kernel_time.saturating_add(active_delta),
            ),
            CpuAccountingState::Inactive => (inner.user_time, inner.kernel_time),
        };
        let user_ns = raw_counter_to_ns(user_raw);
        let kernel_ns = raw_counter_to_ns(kernel_raw);
        let (timer, now_ns) = match which {
            0 => (&mut inner.itimer_real, now_realtime_ns),
            1 => (&mut inner.itimer_virtual, user_ns),
            2 => (&mut inner.itimer_prof, user_ns.saturating_add(kernel_ns)),
            _ => return Err(ERRNO::EINVAL),
        };

        let old = (
            itimer_remaining_ns(timer.deadline_ns, now_ns),
            timer.interval_ns,
        );

        if let Some((new_value_ns, new_interval_ns)) = new_value {
            let was_armed = timer.deadline_ns != 0;
            timer.interval_ns = new_interval_ns;
            timer.deadline_ns = if new_value_ns == 0 {
                0
            } else {
                now_ns.saturating_add(new_value_ns)
            };
            let now_armed = timer.deadline_ns != 0;
            if !was_armed && now_armed {
                itimer_account_arm();
            } else if was_armed && !now_armed {
                itimer_account_disarm();
            }
        }

        Ok(old)
    }

    /// 在一个时钟 tick 上推进进程级 interval timers，并返回本次应投递的信号集合。
    pub fn consume_expired_itimers(&self, now_raw: usize, now_realtime_ns: u64) -> SignalBit {
        let mut inner = self.inner.lock();
        let active_delta = now_raw.saturating_sub(inner.accounting_timestamp);
        let (user_raw, kernel_raw) = match inner.accounting_state {
            CpuAccountingState::User => (
                inner.user_time.saturating_add(active_delta),
                inner.kernel_time,
            ),
            CpuAccountingState::Kernel => (
                inner.user_time,
                inner.kernel_time.saturating_add(active_delta),
            ),
            CpuAccountingState::Inactive => (inner.user_time, inner.kernel_time),
        };
        let user_ns = raw_counter_to_ns(user_raw);
        let prof_ns = user_ns.saturating_add(raw_counter_to_ns(kernel_raw));

        let mut pending = SignalBit::empty();

        if inner.itimer_real.deadline_ns != 0 && inner.itimer_real.deadline_ns <= now_realtime_ns {
            pending |= SignalBit::SIGALRM;
            rearm_itimer_after_expire(&mut inner.itimer_real, now_realtime_ns);
            if inner.itimer_real.deadline_ns == 0 {
                itimer_account_disarm();
            }
        }
        if inner.itimer_virtual.deadline_ns != 0 && inner.itimer_virtual.deadline_ns <= user_ns {
            pending |= SignalBit::SIGVTALRM;
            rearm_itimer_after_expire(&mut inner.itimer_virtual, user_ns);
            if inner.itimer_virtual.deadline_ns == 0 {
                itimer_account_disarm();
            }
        }
        if inner.itimer_prof.deadline_ns != 0 && inner.itimer_prof.deadline_ns <= prof_ns {
            pending |= SignalBit::SIGPROF;
            rearm_itimer_after_expire(&mut inner.itimer_prof, prof_ns);
            if inner.itimer_prof.deadline_ns == 0 {
                itimer_account_disarm();
            }
        }

        pending
    }

}
