//! Deferred TLB recycle state for kernel-space resources.

use super::FrameTracker;
use crate::sync::SpinNoIrqLock;
use alloc::vec::Vec;
use lazy_static::*;

/// 一个处于 deferred 状态的内核虚拟地址区间。
#[derive(Copy, Clone)]
pub struct DeferredVaRange {
    /// 区间起始地址（含）。
    pub start: usize,
    /// 区间结束地址（不含）。
    pub end: usize,
}

/// 内核态延迟回收统计状态。
///
/// 当前阶段只负责记录“哪些内核虚拟地址区间在全局 TLB flush 之前被释放过”，
/// 以及这些释放动作累计对应的页数，供下一步接入真正的 flush 机制使用。
pub struct DeferredKernelRecycleState {
    /// 记录尚未经过全局 flush 的内核虚拟地址区间。
    deferred_va_ranges: Vec<DeferredVaRange>,
    /// 当前 deferred 区间数量。
    deferred_va_range_count: usize,
    /// 记录尚未经过全局 flush 的页框。
    deferred_frames: Vec<FrameTracker>,
}

impl DeferredKernelRecycleState {
    /// 创建一份空的延迟回收统计状态。
    pub const fn new() -> Self {
        Self {
            deferred_va_ranges: Vec::new(),
            deferred_va_range_count: 0,
            deferred_frames: Vec::new(),
        }
    }

    /// 判断两个区间是否存在重叠。
    fn ranges_overlap(lhs: DeferredVaRange, rhs: DeferredVaRange) -> bool {
        lhs.start < rhs.end && rhs.start < lhs.end
    }

    /// 记录一个进入 deferred 状态的内核虚拟地址区间。
    fn mark_va_range_deferred(&mut self, mut range: DeferredVaRange, mut frames: Vec<FrameTracker>) {
        if range.start >= range.end {
            return;
        }
        let mut idx = 0;
        while idx < self.deferred_va_ranges.len() {
            let current = self.deferred_va_ranges[idx];
            if current.end < range.start {
                idx += 1;
                continue;
            }
            if range.end < current.start {
                break;
            }
            if Self::ranges_overlap(current, range)
                || current.end == range.start
                || range.end == current.start
            {
                range.start = range.start.min(current.start);
                range.end = range.end.max(current.end);
                self.deferred_va_ranges.remove(idx);
                self.deferred_va_range_count = self.deferred_va_range_count.saturating_sub(1);
                continue;
            }
            idx += 1;
        }
        self.deferred_va_ranges.insert(idx, range);
        self.deferred_va_range_count += 1;
        self.deferred_frames.append(&mut frames);
    }

    /// 判断给定虚拟地址区间是否仍然处于 deferred 状态。
    fn va_range_requires_flush(&self, range: DeferredVaRange) -> bool {
        if range.start >= range.end {
            return false;
        }
        self.deferred_va_ranges
            .iter()
            .copied()
            .any(|current| Self::ranges_overlap(current, range))
    }
}

lazy_static! {
    /// 全局内核态延迟回收统计状态。
    static ref DEFERRED_KERNEL_RECYCLE_STATE: SpinNoIrqLock<DeferredKernelRecycleState> =
        SpinNoIrqLock::new(DeferredKernelRecycleState::new());
}

/// 记录一个被释放的内核虚拟地址区间及其页框，等待后续全局 TLB flush 处理。
pub fn note_deferred_kernel_va_release(
    start: usize,
    end: usize,
    frames: Vec<FrameTracker>,
) {
    DEFERRED_KERNEL_RECYCLE_STATE
        .lock()
        .mark_va_range_deferred(DeferredVaRange { start, end }, frames);
}

/// 判断给定内核虚拟地址区间在当前是否仍要求先做全局 TLB flush。
pub fn kernel_va_range_requires_flush(start: usize, end: usize) -> bool {
    DEFERRED_KERNEL_RECYCLE_STATE
        .lock()
        .va_range_requires_flush(DeferredVaRange { start, end })
}

/// 返回当前待 flush 的 deferred 内核虚拟地址区间数量。
pub fn deferred_kernel_va_range_count() -> usize {
    DEFERRED_KERNEL_RECYCLE_STATE
        .lock()
        .deferred_va_range_count
}

/// 返回当前待 flush 的 deferred 页数统计。
pub fn deferred_kernel_frame_count() -> usize {
    DEFERRED_KERNEL_RECYCLE_STATE.lock().deferred_frames.len()
}

/// 判断当前是否存在待后续全局 flush 处理的内核态延迟回收状态。
pub fn has_deferred_kernel_recycle_work() -> bool {
    let state = DEFERRED_KERNEL_RECYCLE_STATE.lock();
    state.deferred_va_range_count != 0 || !state.deferred_frames.is_empty()
}

/// 清空当前所有 deferred 统计状态。
///
/// TODO：接入真正的 global TLB flush 后，应在 flush 完成的同步点统一调用此接口，
/// 再把延迟回收的页框批量并回 allocator。
pub fn clear_deferred_kernel_recycle_state() {
    let mut state = DEFERRED_KERNEL_RECYCLE_STATE.lock();
    state.deferred_va_ranges.clear();
    state.deferred_va_range_count = 0;
    state.deferred_frames.clear();
}
