//! TLB shootdown state and deferred recycle helpers.

use super::{kernel_token, FrameTracker};
use crate::arch::riscv::Sv39Paging;
use crate::hal::hartid;
use crate::hal::traits::{AddressSpaceToken, PagingArch};
use crate::sbi::send_ipi_mask;
use crate::sync::{SpinLock, SpinNoIrqLock};
use alloc::vec::Vec;
use core::arch::asm;
use core::hint::spin_loop;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use lazy_static::*;

/// 一个处于 deferred 状态的内核虚拟地址区间。
#[derive(Copy, Clone)]
pub struct DeferredVaRange {
    /// 区间起始地址（含）。
    pub start: usize,
    /// 区间结束地址（不含）。
    pub end: usize,
}

/// 一次 TLB shootdown 请求的刷新语义。
#[derive(Copy, Clone)]
pub enum ShootdownKind {
    /// 刷新当前 hart 上整个地址空间的 TLB。
    Global,
    /// 刷新某个地址空间的 TLB。
    ///
    /// 调用方应使用目标地址空间的 loaded hart 掩码决定通知范围。远端收到 IPI
    /// 时可能已经从用户态切入内核，但仍需要完成本地 flush 并 ack。
    /// TODO：引入 ASID 后，应把这里改成按 ASID 或地址范围精确刷新。
    AddressSpace {
        /// 目标地址空间的架构 token。
        token: AddressSpaceToken,
    },
}

/// 全局 shootdown 请求槽。
///
/// 同一时刻只允许存在一个进行中的请求；发起方通过 `launch_lock` 串行化。
struct TlbShootdownState {
    /// 当前是否存在尚未完成的 shootdown 请求。
    active: AtomicBool,
    /// 请求序号，便于调试和后续扩展。
    seq: AtomicUsize,
    /// 本次请求需要响应的目标 hart 掩码（不含发起方自身）。
    target_mask: AtomicUsize,
    /// 已完成本地 flush 的目标 hart 掩码。
    ack_mask: AtomicUsize,
    /// 已上线 hart 掩码。
    online_hart_mask: AtomicUsize,
    /// 当前请求类型编码。
    kind_bits: AtomicUsize,
    /// 当前请求附带的地址空间 token 参数。
    arg_token: AtomicUsize,
}

impl TlbShootdownState {
    /// 创建一份空的全局 shootdown 状态。
    const fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            seq: AtomicUsize::new(0),
            target_mask: AtomicUsize::new(0),
            ack_mask: AtomicUsize::new(0),
            online_hart_mask: AtomicUsize::new(0),
            kind_bits: AtomicUsize::new(0),
            arg_token: AtomicUsize::new(0),
        }
    }
}

/// 内核态延迟回收状态。
///
/// 这里记录“哪些内核虚拟地址区间已拆映射但尚未完成全局 shootdown”，以及
/// 对应暂缓归还给 frame allocator 的页框。
pub struct DeferredKernelRecycleState {
    /// 记录尚未经过全局 flush 的内核虚拟地址区间。
    deferred_va_ranges: Vec<DeferredVaRange>,
    /// 当前 deferred 区间数量。
    deferred_va_range_count: usize,
    /// 记录尚未经过全局 flush 的页框。
    deferred_frames: Vec<FrameTracker>,
    /// 记录 flush 完成后才能归还的 kernel stack id。
    deferred_kstack_ids: Vec<usize>,
}

impl DeferredKernelRecycleState {
    /// 创建一份空的延迟回收状态。
    pub const fn new() -> Self {
        Self {
            deferred_va_ranges: Vec::new(),
            deferred_va_range_count: 0,
            deferred_frames: Vec::new(),
            deferred_kstack_ids: Vec::new(),
        }
    }

    /// 判断两个区间是否存在重叠。
    fn ranges_overlap(lhs: DeferredVaRange, rhs: DeferredVaRange) -> bool {
        lhs.start < rhs.end && rhs.start < lhs.end
    }

    /// 记录一个进入 deferred 状态的内核虚拟地址区间。
    fn mark_va_range_deferred(
        &mut self,
        mut range: DeferredVaRange,
        mut frames: Vec<FrameTracker>,
        kstack_id: Option<usize>,
    ) {
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
        if let Some(kstack_id) = kstack_id {
            self.deferred_kstack_ids.push(kstack_id);
        }
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

    /// 提取并清空当前全部 deferred 状态。
    fn take_all(&mut self) -> DeferredBatch {
        let ranges = self.deferred_va_ranges.drain(..).collect();
        self.deferred_va_range_count = 0;
        let frames = self.deferred_frames.drain(..).collect();
        let kstack_ids = self.deferred_kstack_ids.drain(..).collect();
        DeferredBatch { ranges, frames, kstack_ids }
    }
}

/// 一次 flush 完成后可提交的 deferred 回收批次。
pub struct DeferredBatch {
    /// 本批次被确认安全的虚拟地址区间。
    pub ranges: Vec<DeferredVaRange>,
    /// 本批次可以真正归还 allocator 的页框。
    pub frames: Vec<FrameTracker>,
    /// 本批次可以重新放回 kernel stack allocator 的 id。
    pub kstack_ids: Vec<usize>,
}

impl DeferredBatch {
    /// 判断当前批次是否为空。
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty() && self.frames.is_empty() && self.kstack_ids.is_empty()
    }
}

lazy_static! {
    /// 全局内核态延迟回收状态。
    static ref DEFERRED_KERNEL_RECYCLE_STATE: SpinNoIrqLock<DeferredKernelRecycleState> =
        SpinNoIrqLock::new(DeferredKernelRecycleState::new());
    /// 串行化 shootdown 发起流程的全局锁。
    static ref TLB_SHOOTDOWN_LAUNCH_LOCK: SpinLock<()> = SpinLock::new(());
}

/// 全局 TLB shootdown 请求状态。
static TLB_SHOOTDOWN_STATE: TlbShootdownState = TlbShootdownState::new();

const KIND_GLOBAL: usize = 0;
const KIND_ADDRESS_SPACE: usize = 1;

/// 记录一个被释放的内核虚拟地址区间及其页框，等待后续全局 TLB flush 处理。
pub fn defer_release(
    start: usize,
    end: usize,
    kstack_id: Option<usize>,
    frames: Vec<FrameTracker>,
) {
    let frame_count = frames.len();
    DEFERRED_KERNEL_RECYCLE_STATE
        .lock()
        .mark_va_range_deferred(DeferredVaRange { start, end }, frames, kstack_id);
    debug!(
        "[tlb] defer kernel va range [{:#x}, {:#x}), frames={}",
        start, end, frame_count
    );
}

/// 判断给定内核虚拟地址区间在当前是否仍要求先做全局 TLB flush。
pub fn needs_flush(start: usize, end: usize) -> bool {
    DEFERRED_KERNEL_RECYCLE_STATE
        .lock()
        .va_range_requires_flush(DeferredVaRange { start, end })
}

/// 返回当前待 flush 的 deferred 内核虚拟地址区间数量。
pub fn deferred_range_count() -> usize {
    DEFERRED_KERNEL_RECYCLE_STATE
        .lock()
        .deferred_va_range_count
}

/// 返回当前待 flush 的 deferred 页数统计。
pub fn deferred_frame_count() -> usize {
    DEFERRED_KERNEL_RECYCLE_STATE.lock().deferred_frames.len()
}

/// 返回当前等待 flush 后回收的 kernel stack id 数量。
pub fn deferred_kstack_id_count() -> usize {
    DEFERRED_KERNEL_RECYCLE_STATE.lock().deferred_kstack_ids.len()
}

/// 判断当前是否存在待后续全局 flush 处理的内核态延迟回收状态。
pub fn has_deferred() -> bool {
    let state = DEFERRED_KERNEL_RECYCLE_STATE.lock();
    state.deferred_va_range_count != 0
        || !state.deferred_frames.is_empty()
        || !state.deferred_kstack_ids.is_empty()
}

/// 提取并清空当前全部 deferred 回收状态。
pub fn take_deferred() -> DeferredBatch {
    DEFERRED_KERNEL_RECYCLE_STATE.lock().take_all()
}

/// 仅清空 deferred 状态，不主动触发页框回收。
///
/// TODO：该接口主要用于调试/兜底；正常路径应优先使用
/// `take_deferred()` 在 flush 完成点显式提交回收。
pub fn clear_deferred() {
    let _ = take_deferred();
}

/// 标记当前 hart 已上线，可参与后续 shootdown。
pub fn mark_online(hart_id: usize) {
    let online_mask = TLB_SHOOTDOWN_STATE
        .online_hart_mask
        .fetch_or(1usize << hart_id, Ordering::Release)
        | (1usize << hart_id);
    debug!("[tlb] hart {} online, mask={:#b}", hart_id, online_mask);
}

/// 返回当前已上线 hart 掩码。
pub fn online_mask() -> usize {
    TLB_SHOOTDOWN_STATE
        .online_hart_mask
        .load(Ordering::Acquire)
}

/// 对指定 hart 掩码发起一次同步 TLB shootdown。
///
/// 调用方需要保证自己当前不持有会长时间关中断的锁，否则可能放大等待时间。
pub fn shootdown(hart_mask: usize, kind: ShootdownKind) {
    let _launch_guard = TLB_SHOOTDOWN_LAUNCH_LOCK.lock();
    shootdown_inner(hart_mask, kind);
}

/// 在已持有发起锁的前提下执行一次同步 TLB shootdown。
fn shootdown_inner(hart_mask: usize, kind: ShootdownKind) {
    let self_bit = 1usize << hartid();
    let online_mask = online_mask();
    let target_mask = hart_mask & online_mask & !self_bit;
    let seq = TLB_SHOOTDOWN_STATE.seq.load(Ordering::Acquire) + 1;

    let (kind_bits, arg_token) = encode_shootdown_kind(kind);
    TLB_SHOOTDOWN_STATE
        .kind_bits
        .store(kind_bits, Ordering::Release);
    TLB_SHOOTDOWN_STATE
        .arg_token
        .store(arg_token, Ordering::Release);
    TLB_SHOOTDOWN_STATE
        .target_mask
        .store(target_mask, Ordering::Release);
    TLB_SHOOTDOWN_STATE
        .ack_mask
        .store(0, Ordering::Release);
    TLB_SHOOTDOWN_STATE.seq.fetch_add(1, Ordering::AcqRel);
    TLB_SHOOTDOWN_STATE.active.store(true, Ordering::Release);
    debug!(
        "[tlb] launch shootdown seq={} self={} online={:#b} req={:#b} target={:#b} kind={}",
        seq,
        hartid(),
        online_mask,
        hart_mask,
        target_mask,
        shootdown_kind_name(kind)
    );

    // 先刷新发起方本地 TLB，再通知其他 hart。
    perform_local_tlb_shootdown(kind);
    if target_mask != 0 {
        send_ipi_mask(target_mask);
        trace!(
            "[tlb] seq={} ipi sent to mask={:#b}, waiting ack",
            seq, target_mask
        );
        while TLB_SHOOTDOWN_STATE.ack_mask.load(Ordering::Acquire) != target_mask {
            spin_loop();
        }
        debug!("[tlb] seq={} all remote ack received", seq);
    }

    TLB_SHOOTDOWN_STATE.active.store(false, Ordering::Release);
    TLB_SHOOTDOWN_STATE
        .target_mask
        .store(0, Ordering::Release);
    TLB_SHOOTDOWN_STATE
        .ack_mask
        .store(0, Ordering::Release);
    debug!("[tlb] seq={} shootdown complete", seq);
}

/// 对所有已上线 hart 发起一次全局 TLB shootdown。
pub fn shootdown_global() {
    shootdown(usize::MAX, ShootdownKind::Global);
}

/// 完成一次“刷新后提交 deferred 回收”的同步点。
pub fn flush_deferred(hart_mask: usize) {
    let deferred_ranges = deferred_range_count();
    let deferred_frames = deferred_frame_count();
    let mut batch = take_deferred();
    if batch.is_empty() {
        return;
    }
    debug!(
        "[tlb] flush deferred recycle on mask={:#b}, ranges={}, frames={}",
        hart_mask, deferred_ranges, deferred_frames
    );
    shootdown(hart_mask, ShootdownKind::Global);
    debug!(
        "[tlb] reclaim deferred batch: ranges={}, frames={}",
        batch.ranges.len(),
        batch.frames.len()
    );
    let kstack_ids = core::mem::take(&mut batch.kstack_ids);
    crate::task::recycle_deferred_kstack_ids(kstack_ids);
    // 这里通过显式丢弃批次，让其中的 FrameTracker 在 flush 完成后统一回收。
    drop(batch);
}

/// 处理当前 hart 收到的一次 shootdown IPI。
pub fn handle_ipi() {
    let self_bit = 1usize << hartid();
    if !TLB_SHOOTDOWN_STATE.active.load(Ordering::Acquire) {
        return;
    }
    let target_mask = TLB_SHOOTDOWN_STATE.target_mask.load(Ordering::Acquire);
    if target_mask & self_bit == 0 {
        return;
    }
    let kind = decode_shootdown_kind(
        TLB_SHOOTDOWN_STATE.kind_bits.load(Ordering::Acquire),
        TLB_SHOOTDOWN_STATE.arg_token.load(Ordering::Acquire),
    );
    let seq = TLB_SHOOTDOWN_STATE.seq.load(Ordering::Acquire);
    trace!(
        "[tlb] hart {} handling shootdown seq={} kind={}",
        hartid(),
        seq,
        shootdown_kind_name(kind)
    );
    perform_local_tlb_shootdown(kind);
    // 远端完成本地 flush 后再置 ack bit。
    TLB_SHOOTDOWN_STATE
        .ack_mask
        .fetch_or(self_bit, Ordering::AcqRel);
    trace!("[tlb] hart {} ack shootdown seq={}", hartid(), seq);
}

/// 将枚举语义编码到全局请求槽。
fn encode_shootdown_kind(kind: ShootdownKind) -> (usize, usize) {
    match kind {
        ShootdownKind::Global => (KIND_GLOBAL, 0),
        ShootdownKind::AddressSpace { token } => (KIND_ADDRESS_SPACE, token),
    }
}

/// 从全局请求槽解码出当前请求语义。
fn decode_shootdown_kind(kind_bits: usize, arg_token: usize) -> ShootdownKind {
    match kind_bits {
        KIND_ADDRESS_SPACE => ShootdownKind::AddressSpace { token: arg_token },
        _ => ShootdownKind::Global,
    }
}

/// 在当前 hart 上执行一次本地 TLB flush。
fn perform_local_tlb_shootdown(kind: ShootdownKind) {
    match kind {
        ShootdownKind::Global => local_sfence_vma_all(),
        ShootdownKind::AddressSpace { token: target_token } => {
            // 目标 hart 可能是在用户态收到 IPI 后刚切入内核，因此当前 satp
            // 可能已经是 kernel_token；此时仍然要完成本地 flush 并回 ack。
            // TODO：引入 ASID 后，应避免把 kernel_token 情况退化成全量 flush。
            let current_token = unsafe { Sv39Paging::current_token() };
            if current_token == target_token || current_token == kernel_token() {
                local_sfence_vma_all();
            }
        }
    }
}

/// 返回 shootdown 类型名称，便于调试日志观察。
fn shootdown_kind_name(kind: ShootdownKind) -> &'static str {
    match kind {
        ShootdownKind::Global => "global",
        ShootdownKind::AddressSpace { .. } => "address-space",
    }
}

/// 在当前 hart 上执行一次全量 `sfence.vma`。
fn local_sfence_vma_all() {
    unsafe {
        asm!("sfence.vma");
    }
}
