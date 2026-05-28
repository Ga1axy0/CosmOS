//! Allocator for pid, task user resource, kernel stack using a simple recycle strategy.

use super::ProcessControlBlock;
use crate::config::{KERNEL_STACK_SIZE, PAGE_SIZE, TRAMPOLINE, TRAP_CONTEXT_BASE, USER_STACK_SIZE};
use crate::mm::{
    defer_release, deferred_kstack_id_count, flush_deferred, online_mask, DeferredUserReclaim,
    MapPermission, PhysPageNum, VirtAddr, Vma, KERNEL_SPACE,
};
use crate::sync::{SpinNoIrqLock};
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use lazy_static::*;

/// Allocator with a simple recycle strategy
pub struct RecycleAllocator {
    current: usize,
    recycled: Vec<usize>,
}

impl RecycleAllocator {
    /// Create a new allocator
    pub fn new() -> Self {
        RecycleAllocator {
            current: 0,
            recycled: Vec::new(),
        }
    }
    /// allocate a new item
    pub fn alloc(&mut self) -> usize {
        if let Some(id) = self.recycled.pop() {
            id
        } else {
            self.current += 1;
            self.current - 1
        }
    }
    /// deallocate an item
    pub fn dealloc(&mut self, id: usize) {
        assert!(id < self.current);
        assert!(
            !self.recycled.iter().any(|i| *i == id),
            "id {} has been deallocated!",
            id
        );
        self.recycled.push(id);
    }
}

lazy_static! {
    /// Glocal allocator for pid
    static ref PID_ALLOCATOR: SpinNoIrqLock<RecycleAllocator> = SpinNoIrqLock::new(RecycleAllocator::new());
    /// Global allocator for kernel stack
    static ref KSTACK_ALLOCATOR: SpinNoIrqLock<RecycleAllocator> = SpinNoIrqLock::new(RecycleAllocator::new());
}

/// deferred kernel stack id 超过该水位时触发一次全局 flush 回收。
const KSTACK_DEFERRED_RECYCLE_WATERMARK: usize = 128;

/// The idle task's pid is 0
pub const IDLE_PID: usize = 0;

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

/// Allocate a kernel stack for a task
pub fn kstack_alloc() -> KernelStack {
    if deferred_kstack_id_count() > KSTACK_DEFERRED_RECYCLE_WATERMARK {
        flush_deferred(online_mask());
    }
    let kstack_id = KSTACK_ALLOCATOR.lock().alloc();
    let (kstack_bottom, kstack_top) = kernel_stack_position(kstack_id);
    KERNEL_SPACE.lock().insert_framed_area(
        kstack_bottom.into(),
        kstack_top.into(),
        MapPermission::R | MapPermission::W,
    );
    KernelStack(kstack_id)
}

impl Drop for KernelStack {
    fn drop(&mut self) {
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
        alloc_user_res: bool,
    ) -> Self {
        let tid = process.inner_exclusive_access().alloc_tid();
        let thread_id_handle = if tid == 0 {
            None
        } else {
            Some(thread_id_alloc())
        };
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
        if alloc_user_res {
            task_user_res.alloc_user_res();
        }
        task_user_res
    }
    /// Allocate user resource for a task
    pub fn alloc_user_res(&self) {
        let process = self.process.upgrade().unwrap();
        let mut process_inner = process.inner_exclusive_access();
        // alloc user stack
        let ustack_bottom = ustack_bottom_from_tid(self.ustack_base, self.tid);
        let ustack_top = ustack_bottom + USER_STACK_SIZE;
        let ustack_vma = Vma::new_user_stack(ustack_bottom.into(), ustack_top.into(), self.tid);
        if self.tid == 0 {
            // Main thread needs eager mapping: kernel writes args/auxv before start.
            process_inner.memory_set.insert_vma_eager(ustack_vma);
        } else {
            process_inner.memory_set.insert_vma(ustack_vma, None);
        }
        // alloc trap_cx
        let trap_cx_bottom = trap_cx_bottom_from_tid(self.tid);
        let trap_cx_top = trap_cx_bottom + PAGE_SIZE;
        process_inner.memory_set.insert_vma(
            Vma::new_trap_context(trap_cx_bottom.into(), trap_cx_top.into(), self.tid),
            None,
        );
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
            debug!(
                "[tlb] task user resource reclaim: tid={}",
                self.tid
            );
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
