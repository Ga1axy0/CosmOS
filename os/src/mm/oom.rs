//! OOM logging helpers kept out of the allocator hot path.

use super::frame_allocator_stats;
use super::heap_allocator::{KERNEL_HEAP_BYTES, KERNEL_HEAP_USED_BYTES};
#[cfg(feature = "oom_diagnostics")]
use crate::sched::PID2PCB;
#[cfg(feature = "oom_diagnostics")]
use crate::task::ProcessControlBlock;
use crate::{
    fs::PAGE_CACHE_MANAGER,
    task::{current_task, current_trap_cx},
};
#[cfg(feature = "oom_diagnostics")]
use alloc::{sync::Arc, vec::Vec};
use core::{error, sync::atomic::Ordering};

/// Log a WARN-level snapshot of kernel heap state for leak diagnosis.
///
/// Call this at process-lifecycle boundaries (fork, exit, reap) so the
/// delta between successive events reveals whether the heap returns to
/// baseline after a fork→exit→wait cycle.
pub fn warn_heap_state(label: &str, pid: usize) {
    let heap_bytes = KERNEL_HEAP_BYTES.load(Ordering::Acquire);
    let heap_used = KERNEL_HEAP_USED_BYTES.load(Ordering::Acquire);
    let stats = frame_allocator_stats();
    let proc_count = crate::sched::PID2PCB.lock().len();
    warn!(
        "[heap_trace] {} pid={} heap_used={} heap_committed={} heap_internal_free={} frames_free={} frames_allocated={} proc_count={}",
        label,
        pid,
        heap_used,
        heap_bytes,
        heap_bytes.saturating_sub(heap_used),
        stats.free_pages,
        stats.allocated_pages,
        proc_count,
    );
}

/// Lock-free variant of [`warn_heap_state`] that avoids acquiring
/// `PID2PCB`; safe to call while holding a per-process inner lock.
pub fn warn_heap_state_lockfree(label: &str, pid: usize) {
    let heap_bytes = KERNEL_HEAP_BYTES.load(Ordering::Acquire);
    let heap_used = KERNEL_HEAP_USED_BYTES.load(Ordering::Acquire);
    let stats = frame_allocator_stats();
    warn!(
        "[heap_trace] {} pid={} heap_used={} heap_committed={} heap_internal_free={} frames_free={} frames_allocated={}",
        label,
        pid,
        heap_used,
        heap_bytes,
        heap_bytes.saturating_sub(heap_used),
        stats.free_pages,
        stats.allocated_pages,
    );
}

/// Log one allocation failure at a syscall or user-fault boundary.
pub fn log_oom(context: &str, access: Option<&str>, fault_addr: Option<usize>) {
    let stats = frame_allocator_stats();
    let heap_bytes = KERNEL_HEAP_BYTES.load(Ordering::Acquire);
    let heap_used_bytes = KERNEL_HEAP_USED_BYTES.load(Ordering::Acquire);
    let (pid, tid, syscall_nr) = current_task()
        .and_then(|task| {
            let process = task.process.upgrade()?;
            let tid = {
                let inner = task.inner_exclusive_access();
                inner
                    .res
                    .as_ref()
                    .map(|res| res.thread_id)
                    .unwrap_or(process.getpid())
            };
            Some((process.getpid(), tid, current_trap_cx().syscall_nr()))
        })
        .unwrap_or((0, 0, 0));

    error!(
        "[oom] context={} access={} pid={} tid={} syscall={} fault_addr={:#x} free={} allocated={} total={} oom_count={} kernel_heap_bytes={} kernel_heap_used_bytes={}",
        context,
        access.unwrap_or("-"),
        pid,
        tid,
        syscall_nr,
        fault_addr.unwrap_or(0),
        stats.free_pages,
        stats.allocated_pages,
        stats.total_pages,
        stats.oom_count,
        heap_bytes,
        heap_used_bytes,
    );

    let pcm = PAGE_CACHE_MANAGER.lock();
    error!(
        "[oom] Page cache: cached {}, low {}, high {}",
        pcm.cached_pages, pcm.low_watermark, pcm.high_watermark
    );
    log_detailed_oom();
}

#[cfg(feature = "oom_diagnostics")]
fn log_detailed_oom() {
    let processes: Vec<Arc<ProcessControlBlock>> = {
        let map = PID2PCB.lock();
        let mut processes = Vec::new();
        if processes.try_reserve_exact(map.len()).is_err() {
            error!(
                "[oom] detailed process snapshot skipped: unable to reserve {} entries",
                map.len()
            );
            return;
        }
        for process in map.values() {
            processes.push(Arc::clone(process));
        }
        processes
    };

    for process in processes {
        let inner = process.inner_exclusive_access();
        let ppid = inner
            .parent
            .as_ref()
            .and_then(|parent| parent.upgrade())
            .map(|parent| parent.getpid())
            .unwrap_or(0);
        let fd_used = inner
            .fd_table
            .iter()
            .filter(|entry| entry.is_some())
            .count();
        let live_threads = inner.tasks.iter().filter(|task| task.is_some()).count();
        error!(
            "[oom] process pid={} ppid={} zombie={} threads={} fd_used={} fd_slots={} children={} user_vma_bytes={} vmas={} shm={} exec={}",
            process.getpid(),
            ppid,
            inner.is_zombie,
            live_threads,
            fd_used,
            inner.fd_table.len(),
            inner.children.len(),
            inner.memory_set.user_vma_bytes(),
            inner.memory_set.vmas.len(),
            inner.shm_attachments.len(),
            inner.exec_path.as_str(),
        );
        for task in inner.tasks.iter().filter_map(|slot| slot.as_ref()) {
            let task_inner = task.inner_exclusive_access();
            let (inner_tid, thread_id) = task_inner
                .res
                .as_ref()
                .map(|res| (res.tid, res.thread_id))
                .unwrap_or((usize::MAX, usize::MAX));
            error!(
                "[oom] └ thread pid={} tid={} inner_tid={} status={:?} wait={:?} on_cpu={} on_rq={} policy={:?} pending={:#x} blocked={:#x}",
                process.getpid(),
                thread_id,
                inner_tid,
                task_inner.task_status,
                task_inner.wait_reason,
                task_inner.sched.on_cpu,
                task_inner.sched.on_rq,
                task_inner.sched.policy,
                task_inner.pending_signals.bits(),
                task_inner.signal_mask.bits(),
            );
        }
    }
}

#[cfg(not(feature = "oom_diagnostics"))]
fn log_detailed_oom() {}
