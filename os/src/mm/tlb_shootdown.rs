//! TLB shootdown state and deferred recycle helpers.

use super::FrameTracker;
use crate::config::MAX_HARTS;
use crate::hal::hartid;
use crate::hal::traits::AddressSpaceToken;
#[cfg(feature = "cosmos-meminfo")]
use crate::hal::traits::Timer as _;
#[cfg(feature = "cosmos-meminfo")]
use crate::hal::Plat;
use crate::sbi::send_ipi_mask;
use crate::sync::{SpinLock, SpinLockGuard, SpinNoIrqLock};
use alloc::vec::Vec;
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
    /// 刷新当前 hart 上属于一个 ASID 的全部非全局 TLB 翻译。
    ///
    /// 该请求不会失效带全局位的翻译；调用方必须保证被修改的映射没有设置 G 位。
    Asid {
        /// 目标硬件地址空间标识符。
        asid: usize,
    },
    /// 刷新一个 ASID 下的单页非全局翻译。
    Page {
        /// 目标硬件地址空间标识符。
        asid: usize,
        /// 目标页内的任意虚拟地址。
        vaddr: usize,
    },
    /// 刷新一个 ASID 下指定半开区间覆盖的非全局翻译。
    Range {
        /// 目标硬件地址空间标识符。
        asid: usize,
        /// 区间起始虚拟地址（含）。
        start: usize,
        /// 区间结束虚拟地址（不含）。
        end: usize,
    },
    /// 刷新某个地址空间的 TLB。
    ///
    /// 调用方应使用目标地址空间的 active-user hart 掩码决定通知范围。
    /// inactive hart 不参与同步等待；它会在下次返回该地址空间前根据 TLB
    /// generation 执行 ASID-wide fence。
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
    /// 当前请求附带的地址空间 token 或 ASID 参数。
    arg_token: AtomicUsize,
    /// 当前请求附带的起始地址或单页地址参数。
    arg_start: AtomicUsize,
    /// 当前请求附带的结束地址参数。
    arg_end: AtomicUsize,
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
            arg_start: AtomicUsize::new(0),
            arg_end: AtomicUsize::new(0),
        }
    }
}

/// 内核态延迟回收状态。
///
/// 这里记录“哪些内核虚拟地址区间已拆映射但尚未完成 kernel-ASID shootdown”，以及
/// 对应暂缓归还给 frame allocator 的页框。
pub struct DeferredKernelRecycleState {
    /// 记录尚未经过 kernel-ASID flush 的内核虚拟地址区间。
    deferred_va_ranges: Vec<DeferredVaRange>,
    /// 当前 deferred 区间数量。
    deferred_va_range_count: usize,
    /// 记录尚未经过 kernel-ASID flush 的页框。
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
        DeferredBatch {
            ranges,
            frames,
            kstack_ids,
        }
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
}

/// 全局 TLB shootdown 请求状态。
static TLB_SHOOTDOWN_STATE: TlbShootdownState = TlbShootdownState::new();
/// 串行化 shootdown 发起流程的全局锁。
static TLB_SHOOTDOWN_LAUNCH_LOCK: SpinLock<()> = SpinLock::new(());
/// 每个 hart 最后认领并完成的 shootdown 请求序号。
///
/// 主动轮询和异步 IPI handler 可能同时观察到同一请求。认领序号确保每个 hart
/// 对每一代请求只执行一次 flush/ack，也防止旧请求的重复 ack 跨越到下一代请求。
static LAST_HANDLED_SEQ: [AtomicUsize; MAX_HARTS] = [const { AtomicUsize::new(0) }; MAX_HARTS];

const KIND_GLOBAL: usize = 0;
const KIND_ADDRESS_SPACE: usize = 1;
const KIND_ASID: usize = 2;
const KIND_PAGE: usize = 3;
const KIND_RANGE: usize = 4;

#[cfg(feature = "cosmos-meminfo")]
static TLB_SHOOTDOWN_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static TLB_SHOOTDOWN_IPI_TARGETS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static TLB_SHOOTDOWN_ACK_WAITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static TLB_SHOOTDOWN_ACK_WAIT_TICKS: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "cosmos-meminfo")]
#[derive(Clone, Copy, Debug, Default)]
/// Runtime counters for synchronous TLB shootdown traffic.
pub struct TlbShootdownStats {
    /// Number of shootdown requests launched.
    pub calls: usize,
    /// Cumulative number of remote hart targets sent an IPI.
    pub ipi_targets: usize,
    /// Number of requests that had to wait for at least one remote ack.
    pub ack_waits: usize,
    /// Cumulative platform timer ticks spent waiting for remote acks.
    pub ack_wait_ticks: usize,
}

/// Reset TLB shootdown counters after early memory-management setup.
#[cfg(feature = "cosmos-meminfo")]
pub fn reset_tlb_shootdown_stats() {
    TLB_SHOOTDOWN_CALLS.store(0, Ordering::Release);
    TLB_SHOOTDOWN_IPI_TARGETS.store(0, Ordering::Release);
    TLB_SHOOTDOWN_ACK_WAITS.store(0, Ordering::Release);
    TLB_SHOOTDOWN_ACK_WAIT_TICKS.store(0, Ordering::Release);
}

/// Return cumulative TLB shootdown counters.
#[cfg(feature = "cosmos-meminfo")]
pub fn tlb_shootdown_stats() -> TlbShootdownStats {
    TlbShootdownStats {
        calls: TLB_SHOOTDOWN_CALLS.load(Ordering::Acquire),
        ipi_targets: TLB_SHOOTDOWN_IPI_TARGETS.load(Ordering::Acquire),
        ack_waits: TLB_SHOOTDOWN_ACK_WAITS.load(Ordering::Acquire),
        ack_wait_ticks: TLB_SHOOTDOWN_ACK_WAIT_TICKS.load(Ordering::Acquire),
    }
}

/// 记录一个被释放的内核虚拟地址区间及其页框，等待后续 kernel-ASID TLB flush 处理。
pub fn defer_release(
    start: usize,
    end: usize,
    kstack_id: Option<usize>,
    frames: Vec<FrameTracker>,
) {
    let frame_count = frames.len();
    DEFERRED_KERNEL_RECYCLE_STATE.lock().mark_va_range_deferred(
        DeferredVaRange { start, end },
        frames,
        kstack_id,
    );
    debug!(
        "[tlb] defer kernel va range [{:#x}, {:#x}), frames={}",
        start, end, frame_count
    );
}

/// 判断给定内核虚拟地址区间在当前是否仍要求先做 kernel-ASID TLB flush。
pub fn needs_flush(start: usize, end: usize) -> bool {
    DEFERRED_KERNEL_RECYCLE_STATE
        .lock()
        .va_range_requires_flush(DeferredVaRange { start, end })
}

/// 返回当前待 flush 的 deferred 内核虚拟地址区间数量。
pub fn deferred_range_count() -> usize {
    DEFERRED_KERNEL_RECYCLE_STATE.lock().deferred_va_range_count
}

/// 返回当前待 flush 的 deferred 页数统计。
pub fn deferred_frame_count() -> usize {
    DEFERRED_KERNEL_RECYCLE_STATE.lock().deferred_frames.len()
}

/// 返回当前等待 flush 后回收的 kernel stack id 数量。
pub fn deferred_kstack_id_count() -> usize {
    DEFERRED_KERNEL_RECYCLE_STATE
        .lock()
        .deferred_kstack_ids
        .len()
}

/// 判断当前是否存在待后续 kernel-ASID flush 处理的内核态延迟回收状态。
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
    TLB_SHOOTDOWN_STATE.online_hart_mask.load(Ordering::Acquire)
}

/// 尝试在当前 hart 上完成一份尚未响应的 shootdown 请求。
///
/// 该路径不打印日志、不分配内存，也不依赖本地中断开启。因此它既可由 IPI
/// handler 调用，也可在关中断状态等待任意自旋锁时主动轮询。
fn service_pending_shootdown_quiet() -> Option<(usize, ShootdownKind)> {
    if !TLB_SHOOTDOWN_STATE.active.load(Ordering::Acquire) {
        return None;
    }

    let hart_id = hartid();
    if hart_id >= MAX_HARTS {
        return None;
    }
    let self_bit = 1usize << hart_id;
    let seq = TLB_SHOOTDOWN_STATE.seq.load(Ordering::Acquire);
    let target_mask = TLB_SHOOTDOWN_STATE.target_mask.load(Ordering::Acquire);
    if target_mask & self_bit == 0 {
        return None;
    }

    // Do not combine the target mask of one generation with the sequence of
    // another.  A concurrent IPI handler on this hart may have completed the
    // generation observed above while this path was preempted.
    if !TLB_SHOOTDOWN_STATE.active.load(Ordering::Acquire)
        || TLB_SHOOTDOWN_STATE.seq.load(Ordering::Acquire) != seq
    {
        return None;
    }

    let last_handled = &LAST_HANDLED_SEQ[hart_id];
    let mut observed = last_handled.load(Ordering::Acquire);
    loop {
        if observed == seq {
            return None;
        }
        match last_handled.compare_exchange_weak(observed, seq, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => break,
            Err(current) => observed = current,
        }
    }

    // Once this hart has claimed a targeted generation, the launcher cannot
    // retire that generation or publish the next one until our ack is visible.
    // This makes it safe to read the request payload after claiming it.
    let kind = decode_shootdown_kind(
        TLB_SHOOTDOWN_STATE.kind_bits.load(Ordering::Acquire),
        TLB_SHOOTDOWN_STATE.arg_token.load(Ordering::Acquire),
        TLB_SHOOTDOWN_STATE.arg_start.load(Ordering::Acquire),
        TLB_SHOOTDOWN_STATE.arg_end.load(Ordering::Acquire),
    );
    perform_local_tlb_shootdown(kind);
    TLB_SHOOTDOWN_STATE
        .ack_mask
        .fetch_or(self_bit, Ordering::AcqRel);
    Some((seq, kind))
}

/// 在普通内核路径中主动轮询一次 shootdown 请求。
///
/// 用户 trap 入口调用此函数，使一个已经切换到 kernel page table、但尚未重新
/// 开启中断的 hart 也能及时确认发起方早先拍摄到的 active-user hart 快照。
pub fn poll_pending_shootdown() -> bool {
    service_pending_shootdown_quiet().is_some()
}

/// 获取全局 shootdown 发起锁，并在竞争期间协助完成上一轮请求。
///
/// 不能直接在这里无条件自旋：若当前 hart 是锁持有者正在等待的目标，而本地
/// 中断又处于关闭状态，单纯等待会与远端的 ack 等待形成环形死锁。
fn acquire_launch_lock() -> SpinLockGuard<'static, ()> {
    loop {
        if let Some(guard) = TLB_SHOOTDOWN_LAUNCH_LOCK.try_lock() {
            return guard;
        }
        let _ = service_pending_shootdown_quiet();
        spin_loop();
    }
}

/// 对指定 hart 掩码发起一次同步 TLB shootdown。
///
/// 调用方需要保证自己当前不持有会长时间关中断的锁，否则可能放大等待时间。
pub fn shootdown(hart_mask: usize, kind: ShootdownKind) {
    let _launch_guard = acquire_launch_lock();
    shootdown_inner(hart_mask, kind, true);
}

/// 对指定 hart 掩码发起一次同步 TLB shootdown，但不在发起路径打印日志。
///
/// 这用于 kernel heap grow 等分配器内部路径：普通 `debug!` 日志会格式化并写
/// klog ring buffer，可能再次触发 heap 分配，造成递归 grow 卡死。
pub fn shootdown_quiet(hart_mask: usize, kind: ShootdownKind) {
    let _launch_guard = acquire_launch_lock();
    shootdown_inner(hart_mask, kind, false);
}

/// 在已持有发起锁的前提下执行一次同步 TLB shootdown。
fn shootdown_inner(hart_mask: usize, kind: ShootdownKind, emit_logs: bool) {
    let self_bit = 1usize << hartid();
    let online_mask = online_mask();
    let target_mask = hart_mask & online_mask & !self_bit;
    let seq = TLB_SHOOTDOWN_STATE.seq.load(Ordering::Acquire) + 1;
    #[cfg(feature = "cosmos-meminfo")]
    {
        TLB_SHOOTDOWN_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(feature = "cosmos-meminfo")]
    {
        TLB_SHOOTDOWN_IPI_TARGETS.fetch_add(target_mask.count_ones() as usize, Ordering::Relaxed);
    }

    let (kind_bits, arg_token, arg_start, arg_end) = encode_shootdown_kind(kind);
    TLB_SHOOTDOWN_STATE
        .kind_bits
        .store(kind_bits, Ordering::Release);
    TLB_SHOOTDOWN_STATE
        .arg_token
        .store(arg_token, Ordering::Release);
    TLB_SHOOTDOWN_STATE
        .arg_start
        .store(arg_start, Ordering::Release);
    TLB_SHOOTDOWN_STATE
        .arg_end
        .store(arg_end, Ordering::Release);
    TLB_SHOOTDOWN_STATE
        .target_mask
        .store(target_mask, Ordering::Release);
    TLB_SHOOTDOWN_STATE.ack_mask.store(0, Ordering::Release);
    TLB_SHOOTDOWN_STATE.seq.fetch_add(1, Ordering::AcqRel);
    TLB_SHOOTDOWN_STATE.active.store(true, Ordering::Release);
    if emit_logs {
        debug!(
            "[tlb] launch shootdown seq={} self={} online={:#b} req={:#b} target={:#b} kind={}",
            seq,
            hartid(),
            online_mask,
            hart_mask,
            target_mask,
            shootdown_kind_name(kind)
        );
    }

    // 先刷新发起方本地 TLB，再通知其他 hart。
    perform_local_tlb_shootdown(kind);
    if target_mask != 0 {
        #[cfg(feature = "cosmos-meminfo")]
        {
            TLB_SHOOTDOWN_ACK_WAITS.fetch_add(1, Ordering::Relaxed);
        }
        #[cfg(feature = "cosmos-meminfo")]
        let ack_wait_start = Plat::read_time();
        send_ipi_mask(target_mask);
        if emit_logs {
            trace!(
                "[tlb] seq={} ipi sent to mask={:#b}, waiting ack",
                seq,
                target_mask
            );
        }
        // A target hart that is spinning in an IRQ-disabling lock (or otherwise
        // unable to take the IPI) cannot ack, and this wait — held under the
        // global launch lock — would then hang every subsequent shootdown
        // system-wide. A healthy shootdown completes in microseconds; sample the
        // spin count and, past a threshold only an abnormal stall would reach,
        // log the still-missing hart mask so the wedge is debuggable instead of a
        // silent lockup. Emitted unconditionally (not gated by `emit_logs`) and
        // independent of the allocator: grow's local-flush path above never
        // recurses into a shootdown, so logging here is safe.
        let mut spins: u64 = 0;
        let stall_threshold: u64 = 1 << 23; // ~8M spins ≈ a few ms on typical QEMU
        while TLB_SHOOTDOWN_STATE.ack_mask.load(Ordering::Acquire) & target_mask != target_mask {
            spin_loop();
            spins = spins.wrapping_add(1);
            if spins >= stall_threshold && spins.is_power_of_two() {
                let missing = target_mask & !TLB_SHOOTDOWN_STATE.ack_mask.load(Ordering::Acquire);
                warn!(
                    "[tlb] seq={} shootdown stall: target={:#b} ack={:#b} missing={:#b} — \
                     a hart in the missing mask likely has interrupts disabled (spinning in a \
                     SpinNoIrqLock) and cannot service the shootdown IPI",
                    seq,
                    target_mask,
                    TLB_SHOOTDOWN_STATE.ack_mask.load(Ordering::Acquire),
                    missing
                );
            }
        }
        #[cfg(feature = "cosmos-meminfo")]
        {
            TLB_SHOOTDOWN_ACK_WAIT_TICKS.fetch_add(
                Plat::read_time().wrapping_sub(ack_wait_start),
                Ordering::Relaxed,
            );
        }
        if emit_logs {
            debug!("[tlb] seq={} all remote ack received", seq);
        }
    }

    TLB_SHOOTDOWN_STATE.active.store(false, Ordering::Release);
    TLB_SHOOTDOWN_STATE.target_mask.store(0, Ordering::Release);
    TLB_SHOOTDOWN_STATE.ack_mask.store(0, Ordering::Release);
    if emit_logs {
        debug!("[tlb] seq={} shootdown complete", seq);
    }
}

/// 对所有已上线 hart 发起一次全局 TLB shootdown。
pub fn shootdown_global() {
    shootdown(usize::MAX, ShootdownKind::Global);
}

/// 对所有已上线 hart 发起一次全局 TLB shootdown，但不打印日志。
pub fn shootdown_global_quiet() {
    shootdown_quiet(usize::MAX, ShootdownKind::Global);
}

/// 对指定 hart 掩码发起一次 ASID 定向 TLB shootdown。
pub fn shootdown_asid(hart_mask: usize, asid: usize) {
    shootdown(hart_mask, ShootdownKind::Asid { asid });
}

/// 对指定 hart 掩码发起一次 ASID 定向 TLB shootdown，但不打印日志。
pub fn shootdown_asid_quiet(hart_mask: usize, asid: usize) {
    shootdown_quiet(hart_mask, ShootdownKind::Asid { asid });
}

/// 对指定 hart 掩码发起一次单页 VA+ASID TLB shootdown。
pub fn shootdown_page(hart_mask: usize, asid: usize, vaddr: usize) {
    shootdown(hart_mask, ShootdownKind::Page { asid, vaddr });
}

/// 对指定 hart 掩码发起一次单页 VA+ASID TLB shootdown，但不打印日志。
pub fn shootdown_page_quiet(hart_mask: usize, asid: usize, vaddr: usize) {
    shootdown_quiet(hart_mask, ShootdownKind::Page { asid, vaddr });
}

/// 对指定 hart 掩码发起一次范围 VA+ASID TLB shootdown。
pub fn shootdown_range(hart_mask: usize, asid: usize, start: usize, end: usize) {
    if start < end {
        shootdown(hart_mask, ShootdownKind::Range { asid, start, end });
    }
}

/// 对指定 hart 掩码发起一次范围 VA+ASID TLB shootdown，但不打印日志。
pub fn shootdown_range_quiet(hart_mask: usize, asid: usize, start: usize, end: usize) {
    if start < end {
        shootdown_quiet(hart_mask, ShootdownKind::Range { asid, start, end });
    }
}

/// 完成一次“刷新 kernel ASID 后提交 deferred 回收”的同步点。
///
/// 当前 deferred 状态只承载不带 G 位的动态内核栈映射。
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
    shootdown_asid(hart_mask, super::asid::KERNEL_ASID);
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
    if let Some((seq, kind)) = service_pending_shootdown_quiet() {
        trace!(
            "[tlb] hart {} flushed and acked shootdown seq={} kind={}",
            hartid(),
            seq,
            shootdown_kind_name(kind)
        );
    }
}

/// 将枚举语义编码到全局请求槽。
fn encode_shootdown_kind(kind: ShootdownKind) -> (usize, usize, usize, usize) {
    match kind {
        ShootdownKind::Global => (KIND_GLOBAL, 0, 0, 0),
        ShootdownKind::AddressSpace { token } => (KIND_ADDRESS_SPACE, token, 0, 0),
        ShootdownKind::Asid { asid } => (KIND_ASID, asid, 0, 0),
        ShootdownKind::Page { asid, vaddr } => (KIND_PAGE, asid, vaddr, 0),
        ShootdownKind::Range { asid, start, end } => (KIND_RANGE, asid, start, end),
    }
}

/// 从全局请求槽解码出当前请求语义。
fn decode_shootdown_kind(
    kind_bits: usize,
    arg_token: usize,
    arg_start: usize,
    arg_end: usize,
) -> ShootdownKind {
    match kind_bits {
        KIND_ADDRESS_SPACE => ShootdownKind::AddressSpace { token: arg_token },
        KIND_ASID => ShootdownKind::Asid { asid: arg_token },
        KIND_PAGE => ShootdownKind::Page {
            asid: arg_token,
            vaddr: arg_start,
        },
        KIND_RANGE => ShootdownKind::Range {
            asid: arg_token,
            start: arg_start,
            end: arg_end,
        },
        _ => ShootdownKind::Global,
    }
}

/// 在当前 hart 上执行一次本地 TLB flush。
fn perform_local_tlb_shootdown(kind: ShootdownKind) {
    match kind {
        ShootdownKind::Global => local_sfence_vma_all(),
        ShootdownKind::Asid { asid } => unsafe { crate::hal::flush_tlb_asid(asid) },
        ShootdownKind::Page { asid, vaddr } => local_sfence_vma_page_asid(vaddr, asid),
        ShootdownKind::Range { asid, start, end } => local_sfence_vma_range_asid(start, end, asid),
        ShootdownKind::AddressSpace { token } => {
            // The ack path may run from SpinNoIrqLock::lock while another
            // hart is synchronously waiting for this ack.  It must therefore
            // never acquire KERNEL_SPACE or any other lock.  The ASID is
            // encoded directly in the shootdown token, so invalidation remains
            // precise even after this hart has switched to another satp.
            let asid = crate::hal::address_space_id_from_token(token);
            unsafe { crate::hal::flush_tlb_asid(asid) };
        }
    }
}

/// 返回 shootdown 类型名称，便于调试日志观察。
fn shootdown_kind_name(kind: ShootdownKind) -> &'static str {
    match kind {
        ShootdownKind::Global => "global",
        ShootdownKind::Asid { .. } => "asid",
        ShootdownKind::Page { .. } => "page",
        ShootdownKind::Range { .. } => "range",
        ShootdownKind::AddressSpace { .. } => "address-space",
    }
}

/// 在当前 hart 上执行一次全量 `sfence.vma`。
fn local_sfence_vma_all() {
    unsafe { crate::hal::flush_tlb() }
}

/// 在当前 hart 上刷新一个 VA+ASID 翻译。
fn local_sfence_vma_page_asid(vaddr: usize, asid: usize) {
    unsafe { crate::hal::flush_tlb_page_asid(vaddr, asid) }
}

/// 在当前 hart 上刷新一个 VA 范围；大范围自动退化为 ASID-wide fence。
pub(super) fn local_sfence_vma_range_asid(start: usize, end: usize, asid: usize) {
    unsafe { crate::hal::flush_tlb_range_asid(start, end, asid) }
}
