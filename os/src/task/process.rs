//! Implementation of  [`ProcessControlBlock`]
#![allow(deprecated)]

use super::id::RecycleAllocator;
use super::runqueue::insert_into_pid2process;
use super::{SchedAttr, TaskControlBlock};
use super::{add_task, SignalAction, SignalActions, SignalFlags, SIG_IGN};
use super::{pid_alloc, PidHandle};
use super::WaitQueue;
use crate::config::{CLOCK_FREQ, PAGE_SIZE};
use crate::fs::{mapping_for_inode, new_stdio_files, File, FileDescription};
use crate::mm::{
    register_file_mapping, shootdown, translated_refmut, DeferredUserReclaim, InodeKey,
    MapPermission, MemorySet, PageFaultAccess, ShootdownKind, UserSpaceLayout, VirtAddr, Vma,
    KERNEL_SPACE,
};
use crate::sync::{Condvar, DeadlockDetector, Mutex, Semaphore, SpinNoIrqLock, SpinNoIrqLockGuard};
use crate::syscall::ResourceLimits;
use crate::syscall::errno::ERRNO;
use crate::trap::{trap_handler, TrapContext};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;

/// 每秒对应的纳秒数。
const NSEC_PER_SEC: u64 = 1_000_000_000;

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

/// Process Control Block
pub struct ProcessControlBlock {
    /// immutable
    pub pid: PidHandle,
    /// mutable
    inner: SpinNoIrqLock<ProcessControlBlockInner>,
    pub wait_exit_queue: Arc<WaitQueue>,
}

#[derive(Debug, Clone, Copy)]
pub struct Credentials {
    pub uid: u32,
    pub euid: u32,
    pub gid: u32,
    pub egid: u32,
    pub sid: u32,
}

impl Credentials {
    pub const fn root() -> Self {
        Self {
            uid: 0,
            euid: 0,
            gid: 0,
            egid: 0,
            sid: 0,
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
    pub pending_signals: SignalFlags,
    /// blocked process signals
    pub signal_mask: SignalFlags,
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
    /// process credentials
    pub cred: Credentials,
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
    /// `ITIMER_REAL`：基于 `CLOCK_REALTIME`（墙钟时间）。
    pub itimer_real: ItimerState,
    /// `ITIMER_VIRTUAL`：基于进程用户态 CPU 时间。
    pub itimer_virtual: ItimerState,
    /// `ITIMER_PROF`：基于进程用户态 + 内核态 CPU 时间。
    pub itimer_prof: ItimerState,
    /// Robust list
    pub robust_list: RobustList,
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
    auxv_extra: &[(usize, usize)],
) -> usize {
    let mut user_sp = stack_top;
    let mut arg_ptrs: Vec<usize> = Vec::with_capacity(args.len());
    // 参数字符串放在高地址区域，argv 表只保存用户态指针。
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

    // glibc 会读取 AT_RANDOM，这里先提供固定的 16 字节随机区。
    // TODO: 后续接入真正的随机源，避免固定 canary。
    user_sp -= 8;
    *translated_refmut(token, user_sp as *mut usize).unwrap() = 0xdeadbeef_cafebabe_usize;
    user_sp -= 8;
    *translated_refmut(token, user_sp as *mut usize).unwrap() = 0x0102030405060708_usize;
    let at_random_ptr = user_sp;

    // 先放入 AT_NULL，再倒序放入额外 auxv 与基础 auxv，保证内存中按正序排列。
    user_sp -= core::mem::size_of::<usize>();
    *translated_refmut(token, user_sp as *mut usize).unwrap() = 0;
    user_sp -= core::mem::size_of::<usize>();
    *translated_refmut(token, user_sp as *mut usize).unwrap() = 0;
    for &(tag, val) in auxv_extra.iter().rev() {
        user_sp -= core::mem::size_of::<usize>();
        *translated_refmut(token, user_sp as *mut usize).unwrap() = val;
        user_sp -= core::mem::size_of::<usize>();
        *translated_refmut(token, user_sp as *mut usize).unwrap() = tag;
    }
    for &word in &[at_random_ptr, 25, crate::config::PAGE_SIZE, 6usize] {
        user_sp -= core::mem::size_of::<usize>();
        *translated_refmut(token, user_sp as *mut usize).unwrap() = word;
    }

    user_sp -= core::mem::size_of::<usize>();
    *translated_refmut(token, user_sp as *mut usize).unwrap() = 0;

    user_sp -= core::mem::size_of::<usize>();
    *translated_refmut(token, user_sp as *mut usize).unwrap() = 0;
    for &ptr in arg_ptrs.iter().rev() {
        user_sp -= core::mem::size_of::<usize>();
        *translated_refmut(token, user_sp as *mut usize).unwrap() = ptr;
    }

    user_sp -= core::mem::size_of::<usize>();
    *translated_refmut(token, user_sp as *mut usize).unwrap() = args.len();
    user_sp
}

/// 便捷封装：从静态字符串参数构造用户初始栈。
fn init_user_stack(token: usize, stack_top: usize, args: &[&str]) -> usize {
    let args: Vec<String> = args.iter().map(|arg| String::from(*arg)).collect();
    init_user_stack_from_strings(token, stack_top, args.as_slice(), &[])
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
        self.tasks.iter().filter(|task| task.is_some()).count()
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
    /// new process from elf file
    pub fn new(elf_data: &[u8]) -> Arc<Self> {
        trace!("kernel: ProcessControlBlock::new");
        // memory_set with elf program headers/trampoline/trap context/user stack
        // assert that initproc is always valid elf
        let (memory_set, user_layout, load_info) = MemorySet::from_elf(elf_data).unwrap();
        let entry_point = load_info.entry_point;
        let ustack_base = user_layout.ustack_base;
        let vm_layout = ProcessVmLayout::from_user_layout(user_layout);
        // allocate a pid
        let pid_handle = pid_alloc();
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
                    pending_signals: SignalFlags::empty(),
                    signal_mask: SignalFlags::empty(),
                    signal_actions: SignalActions::default(),
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                    deadlock_enabled: false,
                    mutex_detector: DeadlockDetector::new(),
                    semaphore_detector: DeadlockDetector::new(),
                    cwd: String::from("/"),
                    cred: Credentials::root(),
                    user_time: 0,
                    kernel_time: 0,
                    child_user_time: 0,
                    child_kernel_time: 0,
                    accounting_state: CpuAccountingState::Inactive,
                    accounting_timestamp: 0,
                    itimer_real: ItimerState::default(),
                    itimer_virtual: ItimerState::default(),
                    itimer_prof: ItimerState::default(),
                    robust_list: RobustList { head: 0, len: 0 },
                }),
            wait_exit_queue: Arc::new(WaitQueue::new()),
        });
        // create a main thread, we should allocate ustack and trap_cx here
        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&process),
            ustack_base,
            true,
            SchedAttr::default(),
        ));
        // prepare trap_cx of main thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        let ustack_top = task_inner.res.as_ref().unwrap().ustack_top();
        let kstack_top = task.kstack.get_top();
        drop(task_inner);
        let user_sp = init_user_stack(process.inner_exclusive_access().get_user_token(), ustack_top, &["initproc"]);
        *trap_cx = TrapContext::app_init_context(
            final_entry,
            user_sp,
            KERNEL_SPACE.lock().token(),
            kstack_top,
            trap_handler as usize,
        );
        // add main thread to the process
        let mut process_inner = process.inner_exclusive_access();
        process_inner.tasks.push(Some(Arc::clone(&task)));
        drop(process_inner);
        insert_into_pid2process(process.getpid(), Arc::clone(&process));
        // add main thread to scheduler
        add_task(task);
        process
    }

    /// Only support processes with a single thread.
    pub fn exec(self: &Arc<Self>, elf_data: &[u8], args: Vec<String>) -> Result<(), ERRNO> {
        trace!("kernel: exec");
        assert_eq!(self.inner_exclusive_access().thread_count(), 1);

        // 首先加载原始程序
        trace!("kernel: exec .. MemorySet::from_elf for application");
        let (mut memory_set, user_layout, app_load_info) = MemorySet::from_elf(elf_data)?;

        // 获取当前进程的cwd，用于打开动态链接器
        let cwd = self.inner_exclusive_access().cwd.clone();

        // 决定最终入口点和auxv
        let (final_entry, auxv_extra) = if let Some(interp_path) = &app_load_info.interp_path {
            debug!("Dynamic linking required, loading interpreter: {}", interp_path);

            // 加载动态链接器到同一地址空间
            // 使用 open_file_at 以支持相对路径（虽然INTERP通常是绝对路径）
            let interp_inode = crate::fs::open_file_at(
                cwd.as_str(),
                interp_path.as_str(),
                crate::fs::OpenFlags::RDONLY
            ).map_err(|_| {
                error!("Failed to open interpreter {}", interp_path);
                ERRNO::ENOENT
            })?;

            if interp_inode.is_dir() {
                error!("Dynamic linker path is a directory: {}", interp_path);
                return Err(ERRNO::EISDIR);
            }

            let interp_data = interp_inode.read_all();

            // 解析动态链接器ELF
            let interp_elf = xmas_elf::ElfFile::new(&interp_data).map_err(|_| ERRNO::ENOEXEC)?;
            let interp_entry = interp_elf.header.pt2.entry_point() as usize;
            let ph_count = interp_elf.header.pt2.ph_count();

            // 动态链接器通常是位置无关的（PIE），需要重定位到不冲突的基地址
            // 使用 INTERP_BASE 作为加载基地址
            let interp_base = crate::config::INTERP_BASE;
            debug!("Loading interpreter at base address: {:#x}", interp_base);
            debug!("Interpreter original entry: {:#x}", interp_entry);

            // 将动态链接器的LOAD段加载到内存，所有地址加上 interp_base
            for i in 0..ph_count {
                let ph = interp_elf.program_header(i).map_err(|_| ERRNO::ELIBBAD)?;
                if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
                    // 原始虚拟地址 + 基地址 = 实际加载地址
                    let start_va: VirtAddr = (interp_base + ph.virtual_addr() as usize).into();
                    let end_va: VirtAddr = (interp_base + (ph.virtual_addr() + ph.mem_size()) as usize).into();
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

                    debug!("mapping interpreter segment: [{:#x}, {:#x}) with flags {:?}",
                        usize::from(start_va), usize::from(end_va), map_perm);

                    let vma = Vma::new_elf(start_va, end_va, map_perm);
                    let page_off = start_va.page_offset();
                    let raw = &interp_data[ph.offset() as usize..(ph.offset() + ph.file_size()) as usize];
                    let padded: Vec<u8>;
                    let seg_data: &[u8] = if page_off != 0 {
                        let mut buf = alloc::vec![0u8; page_off + raw.len()];
                        buf[page_off..].copy_from_slice(raw);
                        padded = buf;
                        &padded
                    } else {
                        raw
                    };
                    if !memory_set.insert_vma(vma, Some(seg_data)) {
                        warn!("Failed to insert interpreter VMA at [{:#x}, {:#x})",
                            usize::from(start_va), usize::from(end_va));
                        return Err(ERRNO::EACCES);
                    }
                }
            }

            // 入口点也需要重定位
            let relocated_entry = interp_base + interp_entry;
            debug!("Interpreter relocated entry: {:#x}", relocated_entry);
            debug!("App PHDR vaddr: {:#x}, phnum: {}", app_load_info.phdr_vaddr, app_load_info.phnum);

            // 构造auxv：告诉动态链接器原程序的信息
            let auxv_extra = vec![
                (3usize, app_load_info.phdr_vaddr),  // AT_PHDR
                (4usize, app_load_info.phent_size),  // AT_PHENT
                (5usize, app_load_info.phnum),       // AT_PHNUM
                (9usize, app_load_info.entry_point), // AT_ENTRY
                (7usize, interp_base),               // AT_BASE - 动态链接器实际加载的基地址
            ];

            (relocated_entry, auxv_extra)
        } else {
            // 静态链接程序
            debug!("Static linking, using application entry directly");
            (app_load_info.entry_point, vec![])
        };

        let ustack_base = user_layout.ustack_base;
        let new_token = memory_set.token();
        let vm_layout = ProcessVmLayout::from_user_layout(user_layout);
        // substitute memory_set
        trace!("kernel: exec .. substitute memory_set");
        let (mut old_memory_set, old_token, old_mask, cloexec_entries) = {
            let mut inner = self.inner_exclusive_access();
            let old_memory_set = core::mem::replace(&mut inner.memory_set, memory_set);
            let old_token = old_memory_set.token();
            let old_mask = old_memory_set.loaded_user_harts();
            inner.vm_layout = vm_layout;
            // POSIX: on exec, reset all user-defined signal handlers to SIG_DFL.
            // SIG_IGN dispositions are preserved across exec.
            for action in inner.signal_actions.table.iter_mut() {
                if action.handler != SIG_IGN {
                    *action = SignalAction::default();
                }
            }
            inner.pending_signals = SignalFlags::empty();
            // 关键点：真正销毁 `FileDescription` 可能触发同步回写和块设备等待，
            // 这里必须先把表项挪出进程自旋锁，再在锁外执行 drop。
            let cloexec_entries = inner.take_cloexec_fds();
            (old_memory_set, old_token, old_mask, cloexec_entries)
        };
        debug!("[mmap] exec teardown old memory_set before installing new user context");
        let old_batch = old_memory_set.recycle_data_pages_deferred();
        DeferredUserReclaim::new(old_token, old_mask, old_batch).flush_then_release();
        drop(cloexec_entries);
        // then we alloc user resource for main thread again
        // since memory_set has been changed
        trace!("kernel: exec .. alloc user resource for main thread again");
        let task = self.inner_exclusive_access().get_task(0);
        let mut task_inner = task.inner_exclusive_access();
        task_inner.res.as_mut().unwrap().ustack_base = ustack_base;
        task_inner.res.as_mut().unwrap().alloc_user_res();
        task_inner.trap_cx_ppn = task_inner.res.as_mut().unwrap().trap_cx_ppn();
        // push arguments on user stack — Linux ELF ABI layout:
        //   [sp+0*8]          argc
        //   [sp+1*8..argc*8]  argv[0..argc-1]
        //   [sp+(argc+1)*8]   NULL           (argv terminator)
        //   [sp+(argc+2)*8]   NULL           (envp terminator, empty env)
        //   [sp+(argc+3)*8]   AT_PAGESZ = 6  \
        //   [sp+(argc+4)*8]   4096            |
        //   [sp+(argc+5)*8]   AT_RANDOM = 25  | auxv
        //   [sp+(argc+6)*8]   &random[0]      |
        //   [sp+(argc+7)*8]   ... (dynamic linking auxv if needed)
        //   [sp+(argc+?)*8]   AT_NULL = 0     |
        //   [sp+(argc+?)*8]   0              /
        //   [above + 16 bytes random data, 16-byte aligned]  argument strings
        trace!("kernel: exec .. push arguments on user stack");
        let user_sp = init_user_stack_from_strings(
            new_token,
            task_inner.res.as_mut().unwrap().ustack_top(),
            args.as_slice(),
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
            entry_point,
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
    ) -> Arc<Self> {
        trace!("kernel: clone_process");
        let mut parent = self.inner_exclusive_access();
        assert_eq!(parent.thread_count(), 1);
        debug!(
            "[cow] clone_process begin: parent_pid={} parent_threads={}",
            self.getpid(),
            parent.thread_count()
        );
        // clone parent's memory_set completely including trampoline/ustacks/trap_cxs
        let (memory_set, parent_tlb_needs_flush) =
            MemorySet::from_existed_user(&mut parent.memory_set);
        let parent_token = parent.memory_set.token();
        let parent_mask = if parent_tlb_needs_flush {
            parent.memory_set.loaded_user_harts()
        } else {
            0
        };
        let vm_layout = parent.vm_layout;
        let cred = parent.cred;
        let parent_signal_mask = parent.signal_mask;
        let parent_signal_actions = parent.signal_actions.clone();
        let parent_cwd = parent.cwd.clone();
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
                    pending_signals: SignalFlags::empty(),
                    signal_mask: parent_signal_mask,
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
                    cred,
                    user_time: 0,
                    kernel_time: 0,
                    child_user_time: 0,
                    child_kernel_time: 0,
                    accounting_state: CpuAccountingState::Inactive,
                    accounting_timestamp: 0,
                    itimer_real: ItimerState::default(),
                    itimer_virtual: ItimerState::default(),
                    itimer_prof: ItimerState::default(),
                    robust_list: RobustList { head: 0, len: 0 },
                }),
            wait_exit_queue: Arc::new(WaitQueue::new())
        });
        // add child
        parent.children.push(Arc::clone(&child));
        let parent_task = parent.get_task(0);
        let parent_task_inner = parent_task.inner_exclusive_access();
        let parent_ustack_base = parent_task_inner.res.as_ref().unwrap().ustack_base();
        let parent_sched_attr = parent_task_inner.sched_attr();
        let parent_affinity_mask = parent_task_inner.cpu_affinity_mask;
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
        child.register_existing_file_mappings();
        // create main thread of child process
        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&child),
            parent_ustack_base,
            // here we do not allocate trap_cx or ustack again
            // but mention that we allocate a new kstack here
            false,
            parent_sched_attr,
        ));
        task.inner_exclusive_access().cpu_affinity_mask = parent_affinity_mask;
        // attach task to child process
        let mut child_inner = child.inner_exclusive_access();
        child_inner.tasks.push(Some(Arc::clone(&task)));
        drop(child_inner);
        // 在发布到调度器前修正子进程 trap context，避免 SMP 下子进程过早运行。
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
            let child_token = child.inner_exclusive_access().memory_set.token();
            // 写入 child_tid 前先触发子地址空间的 COW，避免直接改到父子共享页。
            let _ = child.handle_private_cow_fault(child_tid_ptr);
            if let Some(slot) = translated_refmut(child_token, child_tid_ptr as *mut i32) {
                *slot = child_tid_value;
            } else {
                // TODO：当前 clone_process 尚未实现失败回滚，只能保守记录异常。
                warn!(
                    "kernel: clone_process failed to write child_tid at {:#x}",
                    child_tid_ptr
                );
            }
        }
        debug!(
            "[cow] clone_process complete: parent_pid={} child_pid={}",
            self.getpid(),
            child.getpid()
        );
        insert_into_pid2process(child.getpid(), Arc::clone(&child));
        // add this thread to scheduler
        add_task(task);
        child
    }

    /// Create a child process directly from elf image.
    pub fn spawn(self: &Arc<Self>, elf_data: &[u8]) -> Result<Arc<Self>, ERRNO> {
        let (memory_set, user_layout, load_info) = MemorySet::from_elf(elf_data)?;
        let entry_point = load_info.entry_point;
        let ustack_base = user_layout.ustack_base;
        let vm_layout = ProcessVmLayout::from_user_layout(user_layout);
        let mut parent = self.inner_exclusive_access();
        let cred = parent.cred;
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
                    pending_signals: SignalFlags::empty(),
                    signal_mask: parent.signal_mask,
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
                    cred,
                    user_time: 0,
                    kernel_time: 0,
                    child_user_time: 0,
                    child_kernel_time: 0,
                    accounting_state: CpuAccountingState::Inactive,
                    accounting_timestamp: 0,
                    itimer_real: ItimerState::default(),
                    itimer_virtual: ItimerState::default(),
                    itimer_prof: ItimerState::default(),
                    robust_list: RobustList { head: 0, len: 0 },
                }),
            wait_exit_queue: Arc::new(WaitQueue::new())
        });
        parent.children.push(Arc::clone(&child));
        let parent_task = parent.get_task(0);
        let parent_task_inner = parent_task.inner_exclusive_access();
        let parent_sched_attr = parent_task_inner.sched_attr();
        let parent_affinity_mask = parent_task_inner.cpu_affinity_mask;
        drop(parent_task_inner);
        drop(parent);

        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&child),
            ustack_base,
            true,
            parent_sched_attr,
        ));
        task.inner_exclusive_access().cpu_affinity_mask = parent_affinity_mask;
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

        let mut child_inner = child.inner.lock();
        child_inner.tasks.push(Some(Arc::clone(&task)));
        drop(child_inner);
        insert_into_pid2process(child.getpid(), Arc::clone(&child));
        add_task(task);
        Ok(child)
    }
    /// get pid
    pub fn getpid(&self) -> usize {
        self.pid.0
    }

    pub fn getuid(&self) -> u32 {
        self.inner.lock().cred.uid
    }

    pub fn geteuid(&self) -> u32 {
        self.inner.lock().cred.euid
    }

    pub fn getgid(&self) -> u32 {
        self.inner.lock().cred.gid
    }

    pub fn getegid(&self) -> u32 {
        self.inner.lock().cred.egid
    }

    pub fn setegid(&self, egid: u32) {
        self.inner.lock().cred.egid = egid;
    }

    pub fn getsid(&self) -> u32 {
        self.inner.lock().cred.sid
    }

    /// map an anonymous area with given permission, return true if success
    pub fn mmap(&self, start: VirtAddr, end: VirtAddr, perm: MapPermission) -> bool {
        let len = usize::from(end).saturating_sub(usize::from(start));
        let mut inner = self.inner.lock();
        if inner.ensure_address_space_capacity(len).is_err() {
            return false;
        }
        inner.memory_set.mmap_anonymous(start, end, perm)
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
    ) -> bool {
        let len = usize::from(end).saturating_sub(usize::from(start));
        let mapped = {
            let mut inner = self.inner.lock();
            if inner.ensure_address_space_capacity(len).is_err() {
                false
            } else {
                inner.memory_set.mmap_file(start, end, perm, file.clone(), pgoff, shared)
            }
        };
        if mapped {
            if let Some(inode) = file.backing_inode() {
                register_file_mapping(&inode, self);
            }
        }
        mapped
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
    /// 处理当前进程的私有页写时复制缺页。
    pub fn handle_private_cow_fault(&self, fault_addr: usize) -> bool {
        let Some(reclaim) = ({
            let mut inner = self.inner.lock();
            let token = inner.memory_set.token();
            let mask = inner.memory_set.loaded_user_harts();
            inner
                .memory_set
                .handle_private_cow_fault(VirtAddr::from(fault_addr))
                .map(|batch| DeferredUserReclaim::new(token, mask, batch))
        }) else {
            return false;
        };
        reclaim.flush_then_release();
        true
    }
    /// 处理当前进程的 heap 懒分配缺页。
    pub fn handle_lazy_heap_fault(&self, fault_addr: usize, access: PageFaultAccess) -> bool {
        self.inner
            .lock()
            .memory_set
            .handle_lazy_heap_fault(VirtAddr::from(fault_addr), access)
    }
    /// 处理当前进程的 file-backed 缺页。
    pub fn handle_file_page_fault(&self, fault_addr: usize, access: PageFaultAccess) -> Result<(), ERRNO> {
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
                return Ok(());
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
            return Err(ERRNO::EFAULT);
        };
        let Some(inode) = plan.file.backing_inode() else {
            return Err(ERRNO::EFAULT);
        };
        let Some(mapping) = mapping_for_inode(&inode) else {
            return Err(ERRNO::EFAULT);
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
            return Err(ERRNO::ENXIO);
        };
        debug!(
            "[mmap] page fault lazy load: pid={} vpn={:#x} page_idx={} shared={} path={:?}",
            self.getpid(),
            plan.vpn.0,
            plan.page_idx,
            plan.shared,
            plan.file.path()
        );
        let page = mapping.get_page(plan.page_idx);
        let mut inner = self.inner.lock();
        // TODO：这里目前只靠二次匹配校验 VMA 是否仍然有效；
        // 后续补齐更严格的 `mm_seq` 代际校验与跨 hart TLB shootdown。
        let committed = if plan.shared {
            inner.memory_set.map_shared_file_page(&plan, page)
        } else {
            inner.memory_set.map_private_file_page(&plan, page)
        };
        debug!(
            "[mmap] page fault commit result: pid={} vpn={:#x} shared={} committed={}",
            self.getpid(),
            plan.vpn.0,
            plan.shared,
            committed
        );
        if committed {
            Ok(())
        } else {
            Err(ERRNO::EFAULT)
        }
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
                    return old_brk;
                }
            }

            if new_brk > old_brk {
                let success = inner.memory_set.append_metadata_to(heap_start, new_brk_va)
                    || inner.memory_set.register_vma_metadata(Vma::new_heap(
                        heap_start,
                        new_brk_va,
                        MapPermission::R | MapPermission::W | MapPermission::U,
                    ));
                if !success {
                    return old_brk;
                }
            } else if new_brk == inner.vm_layout.start_brk {
                batch = Some(inner
                    .memory_set
                    .remove_vma_with_start_vpn_user_deferred(heap_start.floor()));
            } else if old_end_vpn != new_end_vpn {
                let Some(shrink_batch) = inner.memory_set.shrink_to_deferred(heap_start, new_brk_va) else {
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
            timer.interval_ns = new_interval_ns;
            timer.deadline_ns = if new_value_ns == 0 {
                0
            } else {
                now_ns.saturating_add(new_value_ns)
            };
        }

        Ok(old)
    }

    /// 在一个时钟 tick 上推进进程级 interval timers，并返回本次应投递的信号集合。
    pub fn consume_expired_itimers(&self, now_raw: usize, now_realtime_ns: u64) -> SignalFlags {
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

        let mut pending = SignalFlags::empty();

        if inner.itimer_real.deadline_ns != 0 && inner.itimer_real.deadline_ns <= now_realtime_ns {
            pending |= SignalFlags::SIGALRM;
            rearm_itimer_after_expire(&mut inner.itimer_real, now_realtime_ns);
        }
        if inner.itimer_virtual.deadline_ns != 0 && inner.itimer_virtual.deadline_ns <= user_ns {
            pending |= SignalFlags::SIGVTALRM;
            rearm_itimer_after_expire(&mut inner.itimer_virtual, user_ns);
        }
        if inner.itimer_prof.deadline_ns != 0 && inner.itimer_prof.deadline_ns <= prof_ns {
            pending |= SignalFlags::SIGPROF;
            rearm_itimer_after_expire(&mut inner.itimer_prof, prof_ns);
        }

        pending
    }

}
