//! Implementation of  [`ProcessControlBlock`]

use super::id::RecycleAllocator;
use super::manager::insert_into_pid2process;
use super::TaskControlBlock;
use super::{add_task, SignalActions, SignalFlags};
use super::{pid_alloc, PidHandle};
use crate::fs::{File, Stdin, Stdout};
use crate::mm::{translated_refmut, MapPermission, MemorySet, UserSpaceLayout, VirtAddr, Vma, KERNEL_SPACE};
use crate::sync::{Condvar, DeadlockDetector, Mutex, Semaphore, UPSafeCell};
use crate::trap::{trap_handler, TrapContext};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefMut;

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
    inner: UPSafeCell<ProcessControlBlockInner>,
    pub wait_exit_condvar: Arc<Condvar>,
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
    pub fd_table: Vec<Option<Arc<dyn File + Send + Sync>>>,
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
}

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum CpuAccountingState {
    Inactive,
    User,
    Kernel,
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
            start_stack: layout.start_stack,
        }
    }
}

impl ProcessControlBlockInner {
    #[allow(unused)]
    /// get the address of app's page table
    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }
    /// allocate a new file descriptor
    pub fn alloc_fd(&mut self) -> usize {
        if let Some(fd) = (0..self.fd_table.len()).find(|fd| self.fd_table[*fd].is_none()) {
            fd
        } else {
            self.fd_table.push(None);
            self.fd_table.len() - 1
        }
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
        self.tasks.len()
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
    pub fn inner_exclusive_access(&self) -> RefMut<'_, ProcessControlBlockInner> {
        self.inner.exclusive_access()
    }
    /// new process from elf file
    pub fn new(elf_data: &[u8]) -> Arc<Self> {
        trace!("kernel: ProcessControlBlock::new");
        // memory_set with elf program headers/trampoline/trap context/user stack
        // assert that initproc is always valid elf
        let (memory_set, user_layout, entry_point) = MemorySet::from_elf(elf_data).unwrap();
        let ustack_base = user_layout.ustack_base;
        let vm_layout = ProcessVmLayout::from_user_layout(user_layout);
        // allocate a pid
        let pid_handle = pid_alloc();
        let process = Arc::new(Self {
            pid: pid_handle,
            inner: unsafe {
                UPSafeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    vm_layout,
                    parent: None,
                    children: Vec::new(),
                    exit_reason: ExitReason::Exit(0),
                    fd_table: vec![
                        // 0 -> stdin
                        Some(Arc::new(Stdin)),
                        // 1 -> stdout
                        Some(Arc::new(Stdout)),
                        // 2 -> stderr
                        Some(Arc::new(Stdout)),
                    ],
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
                    user_time: 0,
                    kernel_time: 0,
                    child_user_time: 0,
                    child_kernel_time: 0,
                    accounting_state: CpuAccountingState::Inactive,
                    accounting_timestamp: 0,
                })
            },
            wait_exit_condvar: Arc::new(Condvar::new()),
        });
        // create a main thread, we should allocate ustack and trap_cx here
        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&process),
            ustack_base,
            true,
        ));
        // prepare trap_cx of main thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        let ustack_top = task_inner.res.as_ref().unwrap().ustack_top();
        let kstack_top = task.kstack.get_top();
        drop(task_inner);
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            ustack_top,
            KERNEL_SPACE.exclusive_access().token(),
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
    pub fn exec(self: &Arc<Self>, elf_data: &[u8], args: Vec<String>) -> Result<(), ()> {
        trace!("kernel: exec");
        assert_eq!(self.inner_exclusive_access().thread_count(), 1);
        // memory_set with elf program headers/trampoline/trap context/user stack
        trace!("kernel: exec .. MemorySet::from_elf");
        let (memory_set, user_layout, entry_point) = MemorySet::from_elf(elf_data)?;
        let ustack_base = user_layout.ustack_base;
        let new_token = memory_set.token();
        let vm_layout = ProcessVmLayout::from_user_layout(user_layout);
        // substitute memory_set
        trace!("kernel: exec .. substitute memory_set");
        {
            let mut inner = self.inner_exclusive_access();
            inner.memory_set = memory_set;
            inner.vm_layout = vm_layout;
        }
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
        //   [sp+(argc+4)*8]   4096            | auxv
        //   [sp+(argc+5)*8]   AT_NULL = 0     |
        //   [sp+(argc+6)*8]   0              /
        //   [above, 16-byte aligned]  argument strings
        trace!("kernel: exec .. push arguments on user stack");
        let mut user_sp = task_inner.res.as_mut().unwrap().ustack_top();

        // 1. Push argument strings (from stack top downward)
        let mut arg_ptrs: Vec<usize> = Vec::with_capacity(args.len());
        for arg in args.iter().rev() {
            user_sp -= arg.len() + 1;
            let mut p = user_sp;
            for c in arg.as_bytes() {
                *translated_refmut(new_token, p as *mut u8).unwrap() = *c;
                p += 1;
            }
            *translated_refmut(new_token, p as *mut u8).unwrap() = 0;
            arg_ptrs.push(user_sp);
        }
        arg_ptrs.reverse(); // arg_ptrs[i] now points to args[i]

        // 2. 16-byte align sp (RISC-V calling convention)
        user_sp &= !0xf_usize;

        // 3. Push auxv: [AT_PAGESZ=6, 0x1000, AT_NULL=0, 0]
        //    Iterating in this order pushes 0 first (highest addr), AT_PAGESZ last (lowest),
        //    so the in-memory layout reads: AT_PAGESZ, PAGE_SIZE, AT_NULL, 0.
        for &word in &[0usize, 0usize, crate::config::PAGE_SIZE, 6usize] {
            user_sp -= core::mem::size_of::<usize>();
            *translated_refmut(new_token, user_sp as *mut usize).unwrap() = word;
        }

        // 4. Push envp NULL terminator (empty environment)
        user_sp -= core::mem::size_of::<usize>();
        *translated_refmut(new_token, user_sp as *mut usize).unwrap() = 0;

        // 5. Push argv NULL terminator, then argv pointers (reversed to fill low→high)
        user_sp -= core::mem::size_of::<usize>();
        *translated_refmut(new_token, user_sp as *mut usize).unwrap() = 0; // argv[argc] = NULL
        for &ptr in arg_ptrs.iter().rev() {
            user_sp -= core::mem::size_of::<usize>();
            *translated_refmut(new_token, user_sp as *mut usize).unwrap() = ptr;
        }
        let argv_base = user_sp; // argv_base == sp+8 once argc is pushed below

        // 6. Push argc  —  sp now points here; glibc/musl _start reads argc from *sp
        user_sp -= core::mem::size_of::<usize>();
        *translated_refmut(new_token, user_sp as *mut usize).unwrap() = args.len();

        // initialize trap_cx
        trace!("kernel: exec .. initialize trap_cx");
        let mut trap_cx = TrapContext::app_init_context(
            entry_point,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            task.kstack.get_top(),
            trap_handler as usize,
        );
        // a0/a1 are set for compatibility with non-glibc entry points;
        // glibc _start ignores these and reads argc/argv directly from the stack.
        trap_cx.x[10] = args.len();
        trap_cx.x[11] = argv_base;
        *task_inner.get_trap_cx() = trap_cx;
        Ok(())
    }

    /// Only support processes with a single thread.
    pub fn fork(self: &Arc<Self>) -> Arc<Self> {
        trace!("kernel: fork");
        let mut parent = self.inner_exclusive_access();
        assert_eq!(parent.thread_count(), 1);
        // clone parent's memory_set completely including trampoline/ustacks/trap_cxs
        let memory_set = MemorySet::from_existed_user(&parent.memory_set);
        let vm_layout = parent.vm_layout;
        // alloc a pid
        let pid = pid_alloc();
        // copy fd table
        let mut new_fd_table: Vec<Option<Arc<dyn File + Send + Sync>>> = Vec::new();
        for fd in parent.fd_table.iter() {
            if let Some(file) = fd {
                new_fd_table.push(Some(file.clone()));
            } else {
                new_fd_table.push(None);
            }
        }
        // create child process pcb
        let child = Arc::new(Self {
            pid,
            inner: unsafe {
                UPSafeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    vm_layout,
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    exit_reason: ExitReason::Exit(0),
                    fd_table: new_fd_table,
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
                    cwd: parent.cwd.clone(),
                    user_time: 0,
                    kernel_time: 0,
                    child_user_time: 0,
                    child_kernel_time: 0,
                    accounting_state: CpuAccountingState::Inactive,
                    accounting_timestamp: 0,
                })
            },
            wait_exit_condvar: Arc::new(Condvar::new())
        });
        // add child
        parent.children.push(Arc::clone(&child));
        // create main thread of child process
        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&child),
            parent
                .get_task(0)
                .inner_exclusive_access()
                .res
                .as_ref()
                .unwrap()
                .ustack_base(),
            // here we do not allocate trap_cx or ustack again
            // but mention that we allocate a new kstack here
            false,
        ));
        // attach task to child process
        let mut child_inner = child.inner_exclusive_access();
        child_inner.tasks.push(Some(Arc::clone(&task)));
        drop(child_inner);
        // modify kstack_top in trap_cx of this thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        trap_cx.kernel_sp = task.kstack.get_top();
        drop(task_inner);
        insert_into_pid2process(child.getpid(), Arc::clone(&child));
        // add this thread to scheduler
        add_task(task);
        child
    }

    /// Create a child process directly from elf image.
    pub fn spawn(self: &Arc<Self>, elf_data: &[u8]) -> Result<Arc<Self>, ()> {
        let (memory_set, user_layout, entry_point) = MemorySet::from_elf(elf_data)?;
        let ustack_base = user_layout.ustack_base;
        let vm_layout = ProcessVmLayout::from_user_layout(user_layout);
        let mut parent = self.inner_exclusive_access();
        let pid = pid_alloc();
        let mut new_fd_table: Vec<Option<Arc<dyn File + Send + Sync>>> = Vec::new();
        for fd in parent.fd_table.iter() {
            if let Some(file) = fd {
                new_fd_table.push(Some(file.clone()));
            } else {
                new_fd_table.push(None);
            }
        }
        let child = Arc::new(Self {
            pid,
            inner: unsafe {
                UPSafeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    vm_layout,
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    exit_reason: ExitReason::Exit(0),
                    fd_table: new_fd_table,
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
                    user_time: 0,
                    kernel_time: 0,
                    child_user_time: 0,
                    child_kernel_time: 0,
                    accounting_state: CpuAccountingState::Inactive,
                    accounting_timestamp: 0,
                })
            },
            wait_exit_condvar: Arc::new(Condvar::new())
        });
        parent.children.push(Arc::clone(&child));
        drop(parent);

        let task = Arc::new(TaskControlBlock::new(Arc::clone(&child), ustack_base, true));
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        let ustack_top = task_inner.res.as_ref().unwrap().ustack_top();
        let kstack_top = task.kstack.get_top();
        drop(task_inner);
        *trap_cx = TrapContext::app_init_context(
            entry_point,
            ustack_top,
            KERNEL_SPACE.exclusive_access().token(),
            kstack_top,
            trap_handler as usize,
        );

        let mut child_inner = child.inner_exclusive_access();
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

    /// map an anonymous area with given permission, return true if success
    pub fn mmap(&self, start: VirtAddr, end: VirtAddr, perm: MapPermission) -> bool {
        self.inner
            .exclusive_access()
            .memory_set
            .mmap_anonymous(start, end, perm)
    }
    /// unmap an area. return true if success
    pub fn munmap(&self, start: VirtAddr, end: VirtAddr) -> bool {
        self.inner
            .exclusive_access()
            .memory_set
            .munmap_anonymous(start, end)
    }

    /// 返回当前进程用于 `mmap(NULL, ...)` 的默认起始基址。
    pub fn mmap_base(&self) -> usize {
        self.inner.exclusive_access().vm_layout.mmap_base
    }

    /// 按目标地址调整程序 break，返回调整后的当前 break。
    pub fn set_program_brk(&self, new_brk: usize) -> usize {
        let mut inner = self.inner.exclusive_access();
        let old_brk = inner.vm_layout.brk;
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
        let heap_exists = inner.memory_set.vmas.iter().any(|vma| vma.is_heap());

        if new_brk > old_brk {
            let success = if heap_exists {
                inner.memory_set.append_to(heap_start, new_brk_va)
            } else {
                inner.memory_set.insert_vma(
                    Vma::new_heap(heap_start, new_brk_va, MapPermission::R | MapPermission::W | MapPermission::U),
                    None,
                )
            };
            if !success && old_end_vpn != new_end_vpn {
                return old_brk;
            }
        } else if new_brk == inner.vm_layout.start_brk {
            inner
                .memory_set
                .remove_vma_with_start_vpn(heap_start.floor());
        } else if old_end_vpn != new_end_vpn
            && !inner.memory_set.shrink_to(heap_start, new_brk_va)
        {
            return old_brk;
        }

        inner.vm_layout.brk = new_brk;
        new_brk
    }

    /// Mark this process as running in kernel mode from `now`.
    pub fn resume_in_kernel(&self, now: usize) {
        let mut inner = self.inner.exclusive_access();
        inner.accounting_state = CpuAccountingState::Kernel;
        inner.accounting_timestamp = now;
    }

    /// Account the user-mode slice that ended at `now`, then switch to kernel mode.
    pub fn enter_kernel(&self, now: usize) {
        let mut inner = self.inner.exclusive_access();
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
        let mut inner = self.inner.exclusive_access();
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
    }

    /// Flush the current running slice into the corresponding accumulator.
    pub fn pause_cpu_accounting(&self, now: usize) {
        let mut inner = self.inner.exclusive_access();
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
        let inner = self.inner.exclusive_access();
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

}
