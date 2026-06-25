//! Allocator for pid, task user resource, kernel stack using a simple recycle strategy.

use super::ProcessControlBlock;
use crate::config::{KERNEL_STACK_SIZE, PAGE_SIZE, TRAMPOLINE, TRAP_CONTEXT_BASE, USER_STACK_SIZE};
use crate::mm::{
    defer_release, deferred_frame_count, deferred_kstack_id_count, flush_deferred, online_mask,
    DeferredUserReclaim, MapPermission, MmError, PhysPageNum, VirtAddr, Vma, KERNEL_SPACE,
};
use crate::sync::SpinNoIrqLock;
use crate::timer::get_time_ns;
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use lazy_static::*;

/// Allocator with a simple recycle strategy
pub struct RecycleAllocator {
    current: usize,
    recycled: Vec<usize>,
    recycled_flags: Vec<bool>,
}

impl RecycleAllocator {
    /// Create a new allocator
    pub fn new() -> Self {
        RecycleAllocator {
            current: 0,
            recycled: Vec::new(),
            recycled_flags: Vec::new(),
        }
    }
    /// allocate a new item
    pub fn alloc(&mut self) -> usize {
        if let Some(id) = self.recycled.pop() {
            self.recycled_flags[id] = false;
            id
        } else {
            self.current += 1;
            self.recycled_flags.push(false);
            self.current - 1
        }
    }
    /// deallocate an item
    pub fn dealloc(&mut self, id: usize) {
        assert!(id < self.current);
        debug_assert!(!self.recycled_flags[id], "id {} has been deallocated!", id);
        self.recycled_flags[id] = true;
        self.recycled.push(id);
    }
}

lazy_static! {
    /// Global allocator for pid.
    ///
    /// pid 0 is reserved for the kernel idle/swapper context and is never handed
    /// to a userspace process, so the first process (init) is pid 1 — matching
    /// Linux. This matters because a process's `pgid`/`sid` default to its own
    /// pid, and a `pgid`/`sid` of 0 is invalid in POSIX (it breaks tty job
    /// control, e.g. busybox's `while (tcgetpgrp(fd) != getpgrp()) ...` loop).
    static ref PID_ALLOCATOR: SpinNoIrqLock<RecycleAllocator> = {
        let mut allocator = RecycleAllocator::new();
        let reserved = allocator.alloc(); // burn pid 0
        debug_assert_eq!(reserved, 0);
        SpinNoIrqLock::new(allocator)
    };
    /// Global allocator for kernel stack
    static ref KSTACK_ALLOCATOR: SpinNoIrqLock<RecycleAllocator> = SpinNoIrqLock::new(RecycleAllocator::new());
    /// Cache of fully mapped kernel stacks that can be reused without a global TLB flush.
    static ref KSTACK_CACHE: SpinNoIrqLock<Vec<usize>> = SpinNoIrqLock::new(Vec::new());
}

/// deferred kernel stack id 超过该水位时触发一次全局 flush 回收。
const KSTACK_DEFERRED_RECYCLE_WATERMARK: usize = 64;
/// deferred 物理页超过该水位时触发一次全局 flush 回收。
const DEFERRED_FRAME_RECYCLE_WATERMARK: usize = 16 * 1024 * 1024 / PAGE_SIZE;
/// Keep a small bounded pool of mapped kernel stacks for reuse without letting
/// fork/exit storms permanently withhold large amounts of memory.
const KSTACK_CACHE_LIMIT: usize = 64;
const TASK_USER_RES_TIMING_WARN_THRESHOLD_NS: u64 = 1_000_000;
const KSTACK_ALLOC_TIMING_WARN_THRESHOLD_NS: u64 = 1_000_000;

/// The init process runs as pid 1 (Linux-style); pid 0 is the reserved
/// idle/swapper pid. The kernel shuts down when the process with this pid exits.
pub const IDLE_PID: usize = 1;
/// Linux-compatible reported upper bound for `/proc/sys/kernel/pid_max`.
pub const PID_MAX: usize = 4_194_304;

/// A handle to a pid
pub struct PidHandle(pub usize);

/// Allocate a pid for a process
pub fn pid_alloc() -> PidHandle {
    PidHandle(PID_ALLOCATOR.lock().alloc())
}

impl Drop for PidHandle {
    fn drop(&mut self) {
        // trace!("drop pid {}", self.0);
        PID_ALLOCATOR.lock().dealloc(self.0);
    }
}

/// A handle to a Linux-visible thread id that shares the global pid/tid namespace.
pub struct ThreadIdHandle(pub usize);

/// Allocate a globally unique Linux-visible thread id.
pub fn thread_id_alloc() -> ThreadIdHandle {
    ThreadIdHandle(PID_ALLOCATOR.lock().alloc())
}

impl Drop for ThreadIdHandle {
    fn drop(&mut self) {
        PID_ALLOCATOR.lock().dealloc(self.0);
    }
}

/// Return (bottom, top) of a kernel stack in kernel space.
pub fn kernel_stack_position(kstack_id: usize) -> (usize, usize) {
    let top = TRAMPOLINE - kstack_id * (KERNEL_STACK_SIZE + PAGE_SIZE);
    let bottom = top - KERNEL_STACK_SIZE;
    (bottom, top)
}

/// Kernel stack for a task
pub struct KernelStack(pub usize);

pub(crate) fn cached_kstack_count() -> usize {
    KSTACK_CACHE.lock().len()
}

pub(crate) fn reclaim_cached_kstacks(target_cached: usize) -> usize {
    let mut kstack_ids = Vec::new();
    {
        let mut cache = KSTACK_CACHE.lock();
        while cache.len() > target_cached {
            if let Some(kstack_id) = cache.pop() {
                kstack_ids.push(kstack_id);
            }
        }
    }
    let reclaimed = kstack_ids.len();
    for kstack_id in kstack_ids {
        let (kernel_stack_bottom, _) = kernel_stack_position(kstack_id);
        let kernel_stack_bottom_va: VirtAddr = kernel_stack_bottom.into();
        let deferred_frames = KERNEL_SPACE
            .lock()
            .remove_vma_with_start_vpn_deferred(kernel_stack_bottom_va.into());
        defer_release(
            kernel_stack_bottom,
            kernel_stack_bottom + KERNEL_STACK_SIZE,
            Some(kstack_id),
            deferred_frames,
        );
    }
    if reclaimed != 0 {
        flush_deferred(online_mask());
    }
    reclaimed
}

fn try_take_cached_kstack() -> Option<usize> {
    KSTACK_CACHE.lock().pop()
}

fn try_cache_kstack(kstack_id: usize) -> bool {
    let mut cache = KSTACK_CACHE.lock();
    if cache.len() >= KSTACK_CACHE_LIMIT {
        return false;
    }
    cache.push(kstack_id);
    true
}

/// Allocate a kernel stack for a task
pub fn kstack_alloc() -> Result<KernelStack, MmError> {
    let total_start_ns = get_time_ns();
    let deferred_kstack_before = deferred_kstack_id_count();
    let deferred_frames_before = deferred_frame_count();
    let flush_triggered = deferred_kstack_before > KSTACK_DEFERRED_RECYCLE_WATERMARK
        || deferred_frames_before > DEFERRED_FRAME_RECYCLE_WATERMARK;
    let flush_start_ns = get_time_ns();
    if flush_triggered {
        flush_deferred(online_mask());
    }
    let flush_ns = get_time_ns() - flush_start_ns;
    let cached_before = cached_kstack_count();
    let cache_take_start_ns = get_time_ns();
    if let Some(kstack_id) = try_take_cached_kstack() {
        let cache_take_ns = get_time_ns() - cache_take_start_ns;
        let total_ns = get_time_ns() - total_start_ns;
        if total_ns >= KSTACK_ALLOC_TIMING_WARN_THRESHOLD_NS {
            debug!(
                "[clone-timing] kstack_alloc kstack_id={} total_ns={} cache_hit=true cache_take_ns={} cached_before={} cached_after={} flush_triggered={} flush_ns={} alloc_id_ns=0 map_ns=0 deferred_kstacks_before={} deferred_frames_before={} stack_pages={}",
                kstack_id,
                total_ns,
                cache_take_ns,
                cached_before,
                cached_kstack_count(),
                flush_triggered,
                flush_ns,
                deferred_kstack_before,
                deferred_frames_before,
                KERNEL_STACK_SIZE / PAGE_SIZE,
            );
        }
        return Ok(KernelStack(kstack_id));
    }
    let cache_take_ns = get_time_ns() - cache_take_start_ns;
    let alloc_id_start_ns = get_time_ns();
    let kstack_id = KSTACK_ALLOCATOR.lock().alloc();
    let alloc_id_ns = get_time_ns() - alloc_id_start_ns;
    let (kstack_bottom, kstack_top) = kernel_stack_position(kstack_id);
    let map_start_ns = get_time_ns();
    KERNEL_SPACE.lock().insert_framed_area(
        kstack_bottom.into(),
        kstack_top.into(),
        MapPermission::R | MapPermission::W,
    )?;
    let map_ns = get_time_ns() - map_start_ns;
    let total_ns = get_time_ns() - total_start_ns;
    if total_ns >= KSTACK_ALLOC_TIMING_WARN_THRESHOLD_NS {
        debug!(
            "[clone-timing] kstack_alloc kstack_id={} total_ns={} cache_hit=false cache_take_ns={} cached_before={} cached_after={} flush_triggered={} flush_ns={} alloc_id_ns={} map_ns={} deferred_kstacks_before={} deferred_frames_before={} stack_pages={}",
            kstack_id,
            total_ns,
            cache_take_ns,
            cached_before,
            cached_kstack_count(),
            flush_triggered,
            flush_ns,
            alloc_id_ns,
            map_ns,
            deferred_kstack_before,
            deferred_frames_before,
            KERNEL_STACK_SIZE / PAGE_SIZE,
        );
    }
    Ok(KernelStack(kstack_id))
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        if try_cache_kstack(self.0) {
            debug!(
                "[tlb] kstack cached for reuse: id={}, cached={}",
                self.0,
                cached_kstack_count()
            );
            return;
        }
        let (kernel_stack_bottom, _) = kernel_stack_position(self.0);
        let kernel_stack_bottom_va: VirtAddr = kernel_stack_bottom.into();
        let deferred_frames = KERNEL_SPACE
            .lock()
            .remove_vma_with_start_vpn_deferred(kernel_stack_bottom_va.into());
        debug!(
            "[tlb] kstack drop enters deferred state: id={}, bottom={:#x}, frames={}",
            self.0,
            kernel_stack_bottom,
            deferred_frames.len()
        );
        // 这里先把拆下来的页框挂到 deferred 容器里；真正的 global TLB flush
        // 与批量并回 frame allocator 的同步点在下一步接入。
        defer_release(
            kernel_stack_bottom,
            kernel_stack_bottom + KERNEL_STACK_SIZE,
            Some(self.0),
            deferred_frames,
        );
    }
}

/// 将完成 TLB flush 的 kernel stack id 重新放回分配器。
pub(crate) fn recycle_deferred_kstack_ids(mut kstack_ids: Vec<usize>) {
    if kstack_ids.is_empty() {
        return;
    }
    let mut allocator = KSTACK_ALLOCATOR.lock();
    for id in kstack_ids.drain(..) {
        allocator.dealloc(id);
    }
}

impl KernelStack {
    /// Push a variable of type T into the top of the KernelStack and return its raw pointer
    #[allow(unused)]
    pub fn push_on_top<T>(&self, value: T) -> *mut T
    where
        T: Sized,
    {
        let kernel_stack_top = self.get_top();
        let ptr_mut = (kernel_stack_top - core::mem::size_of::<T>()) as *mut T;
        unsafe {
            *ptr_mut = value;
        }
        ptr_mut
    }
    /// return the top of the kernel stack
    pub fn get_top(&self) -> usize {
        let (_, kernel_stack_top) = kernel_stack_position(self.0);
        kernel_stack_top
    }
}

/// User Resource for a task
pub struct TaskUserRes {
    /// task id
    pub tid: usize,
    /// Linux-visible thread id used by gettid/tgkill.
    pub thread_id: usize,
    /// Handle that owns the globally unique thread id for non-leader threads.
    pub thread_id_handle: Option<ThreadIdHandle>,
    /// user stack base
    pub ustack_base: usize,
    /// process belongs to
    pub process: Weak<ProcessControlBlock>,
}

/// Which per-task user mappings the kernel should allocate for a new task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskUserResAlloc {
    /// Reuse mappings already present in the address space.
    None,
    /// Allocate both the kernel-managed user stack and trap context.
    Full,
    /// Allocate only the trap context; the user stack is supplied by userspace.
    TrapOnly,
}

/// Return the bottom addr (low addr) of the trap context for a task
fn trap_cx_bottom_from_tid(tid: usize) -> usize {
    TRAP_CONTEXT_BASE - tid * PAGE_SIZE
}
/// Return the bottom addr (high addr) of the user stack for a task
fn ustack_bottom_from_tid(ustack_base: usize, tid: usize) -> usize {
    ustack_base + tid * (PAGE_SIZE + USER_STACK_SIZE)
}

impl TaskUserRes {
    /// Create a new TaskUserRes (Task User Resource)
    pub fn new(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        alloc_user_res: TaskUserResAlloc,
    ) -> Result<Self, MmError> {
        let total_start_ns = get_time_ns();
        let alloc_tid_start_ns = get_time_ns();
        let tid = process.inner_exclusive_access().alloc_tid();
        let alloc_tid_ns = get_time_ns() - alloc_tid_start_ns;
        let alloc_thread_id_start_ns = get_time_ns();
        let thread_id_handle = if tid == 0 {
            None
        } else {
            Some(thread_id_alloc())
        };
        let alloc_thread_id_ns = get_time_ns() - alloc_thread_id_start_ns;
        let thread_id = thread_id_handle
            .as_ref()
            .map(|handle| handle.0)
            .unwrap_or_else(|| process.getpid());
        let task_user_res = Self {
            tid,
            thread_id,
            thread_id_handle,
            ustack_base,
            process: Arc::downgrade(&process),
        };
        let alloc_user_res_start_ns = get_time_ns();
        match alloc_user_res {
            TaskUserResAlloc::None => {}
            TaskUserResAlloc::Full => task_user_res.alloc_user_res()?,
            TaskUserResAlloc::TrapOnly => task_user_res.alloc_trap_cx()?,
        }
        let alloc_user_res_ns = get_time_ns() - alloc_user_res_start_ns;
        let total_ns = get_time_ns() - total_start_ns;
        if total_ns >= TASK_USER_RES_TIMING_WARN_THRESHOLD_NS {
            debug!(
                "[clone-timing] task_user_res_new pid={} tid={} thread_id={} alloc_user_res={:?} total_ns={} alloc_tid_ns={} alloc_thread_id_ns={} alloc_user_res_ns={}",
                process.getpid(),
                tid,
                thread_id,
                alloc_user_res,
                total_ns,
                alloc_tid_ns,
                alloc_thread_id_ns,
                alloc_user_res_ns,
            );
        }
        Ok(task_user_res)
    }
    /// Allocate user resource for a task
    pub fn alloc_user_res(&self) -> Result<(), MmError> {
        let process = self.process.upgrade().unwrap();
        let mut process_inner = process.inner_exclusive_access();
        // alloc user stack
        let ustack_bottom = ustack_bottom_from_tid(self.ustack_base, self.tid);
        let ustack_top = ustack_bottom + USER_STACK_SIZE;
        let ustack_vma = Vma::new_user_stack(ustack_bottom.into(), ustack_top.into(), self.tid);
        if self.tid == 0 {
            // Main thread needs eager mapping: kernel writes args/auxv before start.
            process_inner.memory_set.insert_vma_eager(ustack_vma)?;
        } else {
            process_inner.memory_set.insert_vma(ustack_vma, None)?;
        }
        // alloc trap_cx
        let trap_cx_bottom = trap_cx_bottom_from_tid(self.tid);
        let trap_cx_top = trap_cx_bottom + PAGE_SIZE;
        process_inner.memory_set.insert_vma(
            Vma::new_trap_context(trap_cx_bottom.into(), trap_cx_top.into(), self.tid),
            None,
        )?;
        Ok(())
    }

    /// Allocate only the trap context mapping for a Linux `CLONE_VM` thread.
    pub fn alloc_trap_cx(&self) -> Result<(), MmError> {
        let process = self.process.upgrade().unwrap();
        let mut process_inner = process.inner_exclusive_access();
        let trap_cx_bottom = trap_cx_bottom_from_tid(self.tid);
        let trap_cx_top = trap_cx_bottom + PAGE_SIZE;
        process_inner.memory_set.insert_vma(
            Vma::new_trap_context(trap_cx_bottom.into(), trap_cx_top.into(), self.tid),
            None,
        )?;
        Ok(())
    }
    /// Deallocate user resource for a task
    fn dealloc_user_res(&self) {
        // dealloc tid
        let process = self.process.upgrade().unwrap();
        let reclaim = {
            let mut process_inner = process.inner_exclusive_access();
            let token = process_inner.memory_set.token();
            let mask = process_inner.memory_set.loaded_user_harts();
            // 用户栈可能在 fork 后与子进程共享 COW 页，不能使用 kernel stack
            // 专用的独占 frame deferred helper。
            let ustack_bottom_va: VirtAddr =
                ustack_bottom_from_tid(self.ustack_base, self.tid).into();
            let mut release_batch = process_inner
                .memory_set
                .remove_vma_with_start_vpn_user_deferred(ustack_bottom_va.into());
            let trap_cx_bottom_va: VirtAddr = trap_cx_bottom_from_tid(self.tid).into();
            let mut trap_cx_batch = process_inner
                .memory_set
                .remove_vma_with_start_vpn_user_deferred(trap_cx_bottom_va.into());
            release_batch.append(&mut trap_cx_batch);
            DeferredUserReclaim::new(token, mask, release_batch)
        };
        if !reclaim.is_empty() {
            debug!("[tlb] task user resource reclaim: tid={}", self.tid);
        }
        reclaim.flush_then_release();
    }

    #[allow(unused)]
    /// alloc task id
    pub fn alloc_tid(&mut self) {
        self.tid = self
            .process
            .upgrade()
            .unwrap()
            .inner_exclusive_access()
            .alloc_tid();
    }
    /// dealloc task id
    pub fn dealloc_tid(&self) {
        let process = self.process.upgrade().unwrap();
        let mut process_inner = process.inner_exclusive_access();
        process_inner.dealloc_tid(self.tid);
    }
    /// The bottom usr vaddr (low addr) of the trap context for a task with tid
    pub fn trap_cx_user_va(&self) -> usize {
        trap_cx_bottom_from_tid(self.tid)
    }
    /// The physical page number(ppn) of the trap context for a task with tid
    pub fn trap_cx_ppn(&self) -> PhysPageNum {
        let process = self.process.upgrade().unwrap();
        let process_inner = process.inner_exclusive_access();
        let trap_cx_bottom_va: VirtAddr = trap_cx_bottom_from_tid(self.tid).into();
        process_inner
            .memory_set
            .translate(trap_cx_bottom_va.into())
            .unwrap()
            .ppn()
    }
    /// the bottom addr (low addr) of the user stack for a task
    pub fn ustack_base(&self) -> usize {
        self.ustack_base
    }

    /// Linux-visible thread id for this task.
    pub fn thread_id(&self) -> usize {
        self.thread_id
    }

    /// the top addr (high addr) of the user stack for a task
    pub fn ustack_top(&self) -> usize {
        ustack_bottom_from_tid(self.ustack_base, self.tid) + USER_STACK_SIZE
    }
}

impl Drop for TaskUserRes {
    fn drop(&mut self) {
        self.dealloc_tid();
        self.dealloc_user_res();
    }
}
