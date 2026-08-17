//! Address Space [`MemorySet`] management of Process

use super::elf_loader::{ElfLoadInfo, ElfLoader};
use super::{
    frame_alloc_with_reclaim, shootdown, FrameTracker, MmError, PageFaultHandled, ShootdownKind,
};
use super::{AddressSpaceRoot, PTEFlags, PageTable, PageTableEntry};
use super::{PhysAddr, PhysPageNum, VirtAddr, VirtPageNum, USER_SPACE_END};
use super::{StepByOne, VPNRange};
use crate::bootinfo;
#[cfg(target_arch = "loongarch64")]
use crate::config::{KERNEL_HEAP_BASE, MAX_KERNEL_HEAP_SIZE};
use crate::config::{
    MAX_HARTS, PAGE_SIZE, TRAMPOLINE, USER_MMAP_BASE, USER_STACK_BASE, USER_STACK_SIZE,
    USER_VDSO_BASE,
};
use crate::fs::{
    mark_cached_page_dirty, record_fault_around_commit, release_mapped_page, retain_mapped_page,
    sync_inode_range, CachePage, FileDescription, OSInode,
};
use crate::hal::traits::{AddressSpaceToken, TrapMachine};
use crate::hal::ArchTrapMachine;
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::ProcessControlBlock;
use crate::timer::get_time_ns;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt::Write;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use fs::Inode;
use lazy_static::*;

extern "C" {
    fn stext();
    fn etext();
    fn srodata();
    fn erodata();
    fn sdata();
    fn edata();
    fn sbss_with_stack();
    fn ebss();
    fn skernel();
    fn ekernel();
    fn strampoline();
}

const FORK_MEMORYSET_TIMING_WARN_THRESHOLD_NS: u64 = 5_000_000;

/// Counters for the anonymous-page zero-page/COW experiment.
///
/// The zero-page counters are intentionally kept here before the shared-zero
/// page implementation lands, so `/proc/cosmos_meminfo` has a stable baseline
/// interface.  They remain zero until the corresponding mapping and
/// materialization paths call the record helpers below.
#[cfg(feature = "cosmos-meminfo")]
static ANON_ZERO_PAGE_MAP_HITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static ANON_ZERO_PAGE_WRITE_MATERIALIZATIONS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static ANON_PRIVATE_FIRST_FAULTS_READ: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static ANON_PRIVATE_FIRST_FAULTS_WRITE: AtomicUsize = AtomicUsize::new(0);

/// Runtime counters for private anonymous-page first faults and zero-page use.
#[cfg(feature = "cosmos-meminfo")]
#[derive(Clone, Copy, Debug, Default)]
pub struct AnonymousPageStats {
    /// Read or instruction-faults satisfied by the shared zero page.
    pub zero_page_map_hits: usize,
    /// Writes that materialized a private page from the shared zero page.
    pub zero_page_write_materializations: usize,
    /// Private anonymous first faults caused by a read or instruction fetch.
    pub private_first_faults_read: usize,
    /// Private anonymous first faults caused by a write.
    pub private_first_faults_write: usize,
}

/// Return cumulative anonymous-page instrumentation counters.
#[cfg(feature = "cosmos-meminfo")]
pub fn anonymous_page_stats() -> AnonymousPageStats {
    AnonymousPageStats {
        zero_page_map_hits: ANON_ZERO_PAGE_MAP_HITS.load(Ordering::Acquire),
        zero_page_write_materializations: ANON_ZERO_PAGE_WRITE_MATERIALIZATIONS
            .load(Ordering::Acquire),
        private_first_faults_read: ANON_PRIVATE_FIRST_FAULTS_READ.load(Ordering::Acquire),
        private_first_faults_write: ANON_PRIVATE_FIRST_FAULTS_WRITE.load(Ordering::Acquire),
    }
}

/// Reset anonymous-page instrumentation after the memory subsystem is ready.
#[cfg(feature = "cosmos-meminfo")]
pub fn reset_anonymous_page_stats() {
    ANON_ZERO_PAGE_MAP_HITS.store(0, Ordering::Release);
    ANON_ZERO_PAGE_WRITE_MATERIALIZATIONS.store(0, Ordering::Release);
    ANON_PRIVATE_FIRST_FAULTS_READ.store(0, Ordering::Release);
    ANON_PRIVATE_FIRST_FAULTS_WRITE.store(0, Ordering::Release);
}

/// Record one shared-zero-page mapping hit.
///
/// This is exposed for the eventual zero-page fault path; keeping the counter
/// update in one place prevents the `/proc` ABI from changing when that path is
/// enabled.
#[cfg(feature = "cosmos-meminfo")]
#[allow(dead_code)]
pub fn record_anonymous_zero_page_map_hit() {
    ANON_ZERO_PAGE_MAP_HITS.fetch_add(1, Ordering::Relaxed);
}

/// Record one write fault that materialized a private page from the zero page.
#[cfg(feature = "cosmos-meminfo")]
#[allow(dead_code)]
pub fn record_anonymous_zero_page_write_materialization() {
    ANON_ZERO_PAGE_WRITE_MATERIALIZATIONS.fetch_add(1, Ordering::Relaxed);
}

#[inline]
#[cfg(feature = "cosmos-meminfo")]
fn record_private_anonymous_first_fault(access: PageFaultAccess) {
    match access {
        PageFaultAccess::Write => {
            ANON_PRIVATE_FIRST_FAULTS_WRITE.fetch_add(1, Ordering::Relaxed);
        }
        PageFaultAccess::Read | PageFaultAccess::Exec => {
            ANON_PRIVATE_FIRST_FAULTS_READ.fetch_add(1, Ordering::Relaxed);
        }
    }
}

lazy_static! {
    /// The kernel's initial memory mapping(kernel address space)
    pub static ref KERNEL_SPACE: Arc<SpinNoIrqLock<MemorySet>> =
        Arc::new(SpinNoIrqLock::new(MemorySet::new_kernel()));
    /// file-backed mmap 的反向映射注册表，用于 truncate 时找到需要失效的用户页表。
    static ref FILE_MAPPING_REGISTRY: SpinNoIrqLock<Vec<FileMappingEntry>> =
        SpinNoIrqLock::new(Vec::new());
}

/// the kernel token
pub fn kernel_token() -> AddressSpaceToken {
    KERNEL_SPACE.lock().token()
}
/// 用于稳定标识一个底层 inode。
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct InodeKey {
    /// 文件系统编号。
    fs_id: u64,
    /// inode 编号。
    ino: u64,
}

impl InodeKey {
    /// 构造某个文件系统范围内的最小 key。
    pub const fn fs_range_start(fs_id: u64) -> Self {
        Self { fs_id, ino: 0 }
    }

    /// 构造某个文件系统范围内的最大 key。
    pub const fn fs_range_end(fs_id: u64) -> Self {
        Self {
            fs_id,
            ino: u64::MAX,
        }
    }

    /// 从 inode 中提取稳定 key。
    pub fn from_inode(inode: &Arc<Inode>) -> Self {
        Self {
            fs_id: inode.fs_id(),
            ino: inode.ino(),
        }
    }

    /// 返回文件系统编号。
    pub fn fs_id(&self) -> u64 {
        self.fs_id
    }
}

/// 一条 file-backed mmap 反向映射记录。
struct FileMappingEntry {
    /// 被映射的 inode。
    inode: InodeKey,
    /// 曾经映射过该 inode 的进程。
    process: Weak<ProcessControlBlock>,
}

/// 登记当前进程曾建立过某个 inode 的 file-backed mmap。
pub fn register_file_mapping(inode: &Arc<Inode>, process: &Arc<ProcessControlBlock>) {
    let inode = InodeKey::from_inode(inode);
    let mut registry = FILE_MAPPING_REGISTRY.lock();
    let process_ptr = Arc::as_ptr(process);
    if registry
        .iter()
        .any(|entry| entry.inode == inode && entry.process.as_ptr() == process_ptr)
    {
        return;
    }
    registry.push(FileMappingEntry {
        inode,
        process: Arc::downgrade(process),
    });
    debug!(
        "[mmap] register file mapping: fs_id={} ino={} pid={}",
        inode.fs_id,
        inode.ino,
        process.getpid()
    );
}

/// 在进程退出/被回收后清除该进程注册的所有文件映射条目，
/// 避免 `Weak<ProcessControlBlock>` 阻止 `ArcInner` 块释放。
pub fn unregister_file_mappings_for_process(process: &ProcessControlBlock) {
    let process_ptr = process as *const ProcessControlBlock;
    let mut registry = FILE_MAPPING_REGISTRY.lock();
    registry.retain(|entry| {
        let keep = entry.process.as_ptr() != process_ptr;
        if !keep {
            debug!(
                "[mmap] unregister file mapping: fs_id={} ino={} pid={}",
                entry.inode.fs_id,
                entry.inode.ino,
                process.getpid()
            );
        }
        keep
    });
}

/// 在 truncate 缩小时失效所有映射了该 inode 的用户页表项。
pub fn invalidate_inode_mappings_after_truncate(inode: &Arc<Inode>, new_size: usize) {
    let inode = InodeKey::from_inode(inode);
    let processes = {
        let mut registry = FILE_MAPPING_REGISTRY.lock();
        let mut processes = Vec::new();
        registry.retain(|entry| {
            let Some(process) = entry.process.upgrade() else {
                return false;
            };
            if entry.inode == inode {
                processes.push(process);
            }
            true
        });
        processes
    };
    for process in processes {
        process.invalidate_file_mappings_after_truncate(inode, new_size);
    }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    value.checked_add(align - 1).map(|v| v & !(align - 1))
}

fn align_up_to_page(value: usize) -> usize {
    align_up(value, PAGE_SIZE).unwrap_or(usize::MAX)
}

fn align_down_to_page(value: usize) -> usize {
    value & !(PAGE_SIZE - 1)
}

#[cfg(target_arch = "loongarch64")]
#[inline]
fn overlaps_kernel_heap_range(start: usize, end: usize) -> bool {
    start < KERNEL_HEAP_BASE.saturating_add(MAX_KERNEL_HEAP_SIZE) && end > KERNEL_HEAP_BASE
}

fn map_kernel_ram_fragment(memory_set: &mut MemorySet, start: usize, end: usize) {
    if start >= end {
        return;
    }
    memory_set
        .insert_vma(
            Vma::new(
                crate::platform::direct_map_phys_to_virt(start).into(),
                crate::platform::direct_map_phys_to_virt(end).into(),
                MapType::Direct,
                MapPermission::R | MapPermission::W,
                VmaKind::Kernel,
            ),
            None,
        )
        .expect("failed to map physical memory window");
}

fn format_hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (idx, byte) in bytes.iter().enumerate() {
        if idx > 0 {
            if idx % 16 == 0 {
                out.push_str(" | ");
            } else {
                out.push(' ');
            }
        }
        let _ = write!(&mut out, "{:02x}", byte);
    }
    out
}

/// address space
pub struct MemorySet {
    /// page table
    pub page_table: PageTable,
    /// virtual memory areas, keyed by start VPN.
    pub vmas: BTreeMap<VirtPageNum, Vma>,
    /// Hardware address-space ID encoded into this memory set's token.
    asid: usize,
    /// Harts currently eligible to execute userspace from this address space.
    ///
    /// Trap entry now retains this page-table root, but the bit is still
    /// cleared once the hart has left user mode: kernel user-memory helpers
    /// walk the process table explicitly. Inactive harts may retain stale user
    /// entries; `tlb_generation` forces an ASID-wide fence before user return.
    active_user_harts: AtomicUsize,
    /// Incremented after every page-table edit visible to an existing task.
    tlb_generation: AtomicUsize,
    /// Last generation synchronized locally by each hart.
    seen_tlb_generation: [AtomicUsize; MAX_HARTS],
    /// Whether this `MemorySet` is a temporary shared-MM vfork view.
    ///
    /// Such a view borrows the parent's page-table root and must transfer its
    /// VMA/page-table-descendant ownership back before being dropped.
    shared_vfork_view: bool,
}

/// A snapshot of shared file ranges that may be flushed after releasing the
/// process address-space lock.
pub(crate) struct FileMappingSyncPlan {
    ranges: Vec<FileMappingSyncRange>,
}

struct FileMappingSyncRange {
    inode: Arc<Inode>,
    file_offset: usize,
    byte_len: usize,
}

impl FileMappingSyncPlan {
    /// Execute the potentially blocking page-cache writeback outside PCB locks.
    pub(crate) fn execute(self) -> Result<(), ERRNO> {
        for range in self.ranges {
            sync_inode_range(&range.inode, range.file_offset, range.byte_len)?;
        }
        Ok(())
    }
}

/// 用户地址空间初始化后需要交给进程管理层保存的关键边界信息。
pub struct UserSpaceLayout {
    /// 程序数据段末尾对齐后的初始 break。
    pub start_brk: usize,
    /// 供 `mmap(NULL, ...)` 选择地址时使用的默认基址。
    pub mmap_base: usize,
    /// 主线程用户栈所在区域的底部地址。
    pub ustack_base: usize,
    /// 主线程初始栈顶地址。
    pub start_stack: usize,
}

/// 用户页表 shootdown 完成后才能释放的旧页对象集合。
pub(crate) struct UserReleaseBatch {
    pages: Vec<DeferredUserPage>,
}

/// Ownership extracted from a shared-MM vfork view when it execs or exits.
pub(crate) struct SharedMemorySetState {
    pub(crate) vmas: Vec<Vma>,
    pub(crate) page_table_frames: Vec<FrameTracker>,
}

/// 用户页表中已经摘除、但仍需等 TLB shootdown 后才能释放的页对象。
enum DeferredUserPage {
    /// 私有匿名页或 COW 私有页。
    Private(Arc<PrivatePage>),
    /// 直接映射的 page cache 页。
    DirectCache(Arc<SpinNoIrqLock<CachePage>>),
}

impl UserReleaseBatch {
    /// 创建一个空的用户页延迟释放批次。
    pub(crate) fn new() -> Self {
        Self { pages: Vec::new() }
    }

    /// 判断当前批次是否为空。
    pub(crate) fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// 暂存一张私有页，等待远端 TLB flush 完成后再释放引用。
    fn push_private(&mut self, page: Arc<PrivatePage>) {
        self.pages.push(DeferredUserPage::Private(page));
    }

    /// 暂存一张 page cache 映射页，等待远端 TLB flush 完成后再减少映射计数。
    fn push_direct_cache(&mut self, page: Arc<SpinNoIrqLock<CachePage>>) {
        self.pages.push(DeferredUserPage::DirectCache(page));
    }

    /// 合并另一个延迟释放批次。
    pub(crate) fn append(&mut self, other: &mut Self) {
        self.pages.append(&mut other.pages);
    }
}

impl Drop for UserReleaseBatch {
    fn drop(&mut self) {
        for page in self.pages.drain(..) {
            match page {
                DeferredUserPage::Private(_page) => {}
                DeferredUserPage::DirectCache(page) => release_mapped_page(&page),
            }
        }
    }
}

/// 用户页表修改后需要在锁外完成的 TLB shootdown 与延迟释放动作。
pub struct DeferredUserReclaim {
    /// 被修改的用户地址空间 token。
    token: usize,
    /// 需要接收 shootdown 的 hart 掩码。
    mask: usize,
    /// 精确刷新语义。
    flush: DeferredUserFlush,
    /// shootdown 完成后才能释放的旧页对象。
    batch: UserReleaseBatch,
}

#[derive(Copy, Clone)]
enum DeferredUserFlush {
    AddressSpace,
    Page { vaddr: usize },
    Range { start: usize, end: usize },
}

impl DeferredUserReclaim {
    /// 基于锁内快照创建一次用户页表延迟回收动作。
    pub(crate) fn new(token: usize, mask: usize, batch: UserReleaseBatch) -> Self {
        Self {
            token,
            mask,
            flush: DeferredUserFlush::AddressSpace,
            batch,
        }
    }

    /// 创建一次单页 VA+ASID 延迟回收动作。
    pub(crate) fn new_page(
        token: usize,
        mask: usize,
        vaddr: usize,
        batch: UserReleaseBatch,
    ) -> Self {
        Self {
            token,
            mask,
            flush: DeferredUserFlush::Page { vaddr },
            batch,
        }
    }

    /// 创建一次范围 VA+ASID 延迟回收动作。
    pub(crate) fn new_range(
        token: usize,
        mask: usize,
        start: usize,
        end: usize,
        batch: UserReleaseBatch,
    ) -> Self {
        Self {
            token,
            mask,
            flush: DeferredUserFlush::Range { start, end },
            batch,
        }
    }

    /// 判断本次回收是否实际持有旧页对象。
    pub fn is_empty(&self) -> bool {
        self.batch.is_empty()
    }

    /// 在目标 hart 完成 TLB shootdown 后释放旧页对象。
    pub fn flush_then_release(self) {
        // Permission-only edits (for example an exclusive COW page becoming
        // writable) can leave the release batch empty while still requiring
        // remote harts to discard a restrictive translation.  The mask is the
        // authoritative indication that this address space may still be loaded
        // elsewhere; page ownership is only relevant to the deferred release.
        if self.mask != 0 {
            debug!(
                "[tlb] deferred user reclaim shootdown: token={:#x} mask={:#b}",
                self.token, self.mask
            );
            match self.flush {
                DeferredUserFlush::AddressSpace => {
                    shootdown(self.mask, ShootdownKind::AddressSpace { token: self.token });
                }
                DeferredUserFlush::Page { vaddr } => shootdown(
                    self.mask,
                    ShootdownKind::Page {
                        asid: crate::hal::address_space_id_from_token(self.token),
                        vaddr,
                    },
                ),
                DeferredUserFlush::Range { start, end } => shootdown(
                    self.mask,
                    ShootdownKind::Range {
                        asid: crate::hal::address_space_id_from_token(self.token),
                        start,
                        end,
                    },
                ),
            }
        }
        // self 在函数返回时析构，batch 的 Drop 会真正释放旧页引用。
    }
}

impl MemorySet {
    /// Flush every local non-global translation tagged with this memory set's ASID.
    #[inline]
    fn flush_local_tlb_asid(&self) {
        unsafe { crate::hal::flush_tlb_asid(self.asid) };
    }

    /// Flush one local non-global page translation tagged with this ASID.
    #[inline]
    fn flush_local_tlb_page_asid(&self, vaddr: usize) {
        unsafe { crate::hal::flush_tlb_page_asid(vaddr, self.asid) };
    }

    /// Flush one local virtual page number tagged with this ASID.
    #[inline]
    fn flush_local_tlb_vpn_asid(&self, vpn: VirtPageNum) {
        self.flush_local_tlb_page_asid(VirtAddr::from(vpn).0);
    }

    /// Flush a bounded local VA range, falling back to ASID-wide for large ranges.
    #[inline]
    fn flush_local_tlb_range_asid(&self, start: usize, end: usize) {
        super::tlb_shootdown::local_sfence_vma_range_asid(start, end, self.asid);
    }

    fn map_perm_to_pte_flags(map_perm: MapPermission) -> PTEFlags {
        let mut flags = PTEFlags::empty();
        if map_perm.contains(MapPermission::R) {
            flags.insert(PTEFlags::R);
        }
        if map_perm.contains(MapPermission::W) {
            flags.insert(PTEFlags::W);
        }
        if map_perm.contains(MapPermission::X) {
            flags.insert(PTEFlags::X);
        }
        if map_perm.contains(MapPermission::U) {
            flags.insert(PTEFlags::U);
        }
        crate::hal::normalize_leaf_pte_flags(flags)
    }

    /// Return whether a resident leaf PTE permits one user-mode access.
    fn pte_allows_user_access(pte: PageTableEntry, access: PageFaultAccess) -> bool {
        pte.is_user()
            && match access {
                PageFaultAccess::Read => pte.readable(),
                PageFaultAccess::Write => pte.writable(),
                PageFaultAccess::Exec => pte.executable(),
            }
    }

    /// 完成一次会返回延迟回收 batch 的本地页表修改。
    fn finish_deferred_page_table_edit(&self) {
        // 本地 hart 可能刚刚使用过被拆除的翻译，必须先清掉本地 TLB；
        // 远端 hart 的同步由调用方构造 `DeferredUserReclaim` 后在锁外完成。
        self.flush_local_tlb_asid();
    }

    /// Initialize a bare address space directly at `output`.
    ///
    /// Keeping the destination explicit avoids relying on the large-aggregate
    /// return buffer while the LoongArch board boot path is being diagnosed.
    #[inline(never)]
    unsafe fn init_bare_at(output: *mut Self, asid: usize) -> Result<(), MmError> {
        PageTable::init_new_at(core::ptr::addr_of_mut!((*output).page_table))?;
        let vmas = BTreeMap::new();
        // Give every hart an initial value different from the first live
        // generation.  Besides expressing "never synchronized" directly,
        // distinct sentinels avoid an early-boot bulk memset for this atomic
        // array on LoongArch.
        let seen_tlb_generation = core::array::from_fn(|hart| AtomicUsize::new(usize::MAX - hart));

        core::ptr::addr_of_mut!((*output).vmas).write(vmas);
        core::ptr::addr_of_mut!((*output).asid).write(asid);
        core::ptr::addr_of_mut!((*output).active_user_harts).write(AtomicUsize::new(0));
        core::ptr::addr_of_mut!((*output).tlb_generation).write(AtomicUsize::new(1));
        core::ptr::addr_of_mut!((*output).seen_tlb_generation).write(seen_tlb_generation);
        core::ptr::addr_of_mut!((*output).shared_vfork_view).write(false);
        Ok(())
    }

    fn new_bare_with_asid(asid: usize) -> Result<Self, MmError> {
        let mut output = MaybeUninit::<Self>::uninit();
        unsafe {
            Self::init_bare_at(output.as_mut_ptr(), asid)?;
            Ok(output.assume_init())
        }
    }
    /// Create a new empty user `MemorySet` with a boot-unique ASID when
    /// supported by the current architecture.
    pub fn new_bare() -> Result<Self, MmError> {
        // Allocate the page-table root first so an OOM failure does not burn a
        // non-recycled ASID from this boot's finite namespace.
        let mut memory_set = Self::new_bare_with_asid(super::asid::KERNEL_ASID)?;
        memory_set.asid = super::asid::allocate_user_asid();
        #[cfg(target_arch = "riscv64")]
        memory_set
            .page_table
            .share_kernel_half_from(&KERNEL_SPACE.lock().page_table);
        #[cfg(target_arch = "loongarch64")]
        memory_set
            .page_table
            .share_kernel_heap_from(&KERNEL_SPACE.lock().page_table);
        Ok(memory_set)
    }
    /// Get he page table token
    pub fn token(&self) -> AddressSpaceToken {
        crate::hal::with_address_space_id(self.page_table.token(), self.asid)
    }
    /// Pin the root frame while a hart may keep this address space installed.
    pub fn address_space_root(&self) -> AddressSpaceRoot {
        self.page_table.address_space_root(self.token())
    }
    /// Mark one hart active immediately before returning to userspace.
    ///
    /// A hart that missed page-table shootdowns while inactive synchronizes the
    /// whole ASID once here. Ordinary traps whose generation did not change do
    /// not execute a fence.
    pub fn mark_user_active(&self, hart_id: usize) {
        let generation = self.tlb_generation.load(Ordering::Acquire);
        if hart_id >= MAX_HARTS {
            self.flush_local_tlb_asid();
            return;
        }
        if self.seen_tlb_generation[hart_id].load(Ordering::Acquire) != generation {
            self.flush_local_tlb_asid();
            self.seen_tlb_generation[hart_id].store(generation, Ordering::Release);
        }
        let bit = 1usize << hart_id;
        if self.active_user_harts.load(Ordering::Acquire) & bit != 0 {
            return;
        }
        let mask = self.active_user_harts.fetch_or(bit, Ordering::AcqRel) | bit;
        trace!(
            "[tlb] user ASID active on hart {} token={:#x} asid={} generation={} mask={:#b}",
            hart_id,
            self.token(),
            self.asid,
            generation,
            mask
        );
    }
    /// Mark one hart inactive after it has left user mode.
    pub fn mark_user_inactive(&self, hart_id: usize) {
        if hart_id >= MAX_HARTS {
            return;
        }
        // The process root remains loaded in the shared-page-table design, but
        // ordinary kernel code does not dereference user VAs directly. Do not
        // advance `seen_tlb_generation` here. A page-table editor
        // publishes the new generation under process-inner, then launches the
        // remote shootdown after dropping that lock.  This hart can acquire
        // process-inner in between those two steps; retaining the old seen
        // value forces `mark_user_active()` to fence before any such stale
        // translation can be used again.
        let bit = 1usize << hart_id;
        self.active_user_harts.fetch_and(!bit, Ordering::AcqRel);
    }
    /// Return harts currently executing userspace with this address space.
    pub fn active_user_harts(&self) -> usize {
        self.active_user_harts.load(Ordering::Acquire)
    }
    /// Publish one locally synchronized page-table generation and snapshot the
    /// harts that must be synchronously shot down.
    ///
    /// Page-table edit helpers already flush the current hart before calling
    /// this method, so its seen generation can advance without another fence.
    /// Inactive harts are omitted from the synchronous mask and will observe the
    /// new generation in `mark_user_active()` before their next user return.
    fn advance_tlb_generation(&self) -> usize {
        let generation = self
            .tlb_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let hart_id = crate::hal::hartid();
        if hart_id < MAX_HARTS {
            self.seen_tlb_generation[hart_id].store(generation, Ordering::Release);
        }
        self.active_user_harts()
    }
    /// Record a page-table edit after the caller has already synchronized the
    /// current hart's TLB.
    pub fn record_local_tlb_change(&self) -> usize {
        self.advance_tlb_generation()
    }
    /// Synchronize the current hart for this ASID, then publish a new page-table
    /// generation and return the active remote target mask.
    pub fn record_tlb_change_with_local_fence(&self) -> usize {
        self.flush_local_tlb_asid();
        self.advance_tlb_generation()
    }
    /// 对当前正在用户态执行该地址空间的 hart 发起同步 TLB shootdown。
    pub fn shootdown_active_user_harts(&self) {
        let mask = self.active_user_harts();
        self.shootdown_user_harts(mask);
    }
    /// 对指定 hart 掩码发起该地址空间的同步 TLB shootdown。
    ///
    /// 这个接口用于调用方已经在锁内快照出目标 mask，随后释放锁再执行同步等待
    /// 的场景。
    ///
    /// snapshot 只覆盖当前 active harts；inactive harts 会在下一次返回该 mm
    /// 前根据 TLB generation 执行一次本地 ASID-wide fence。
    pub fn shootdown_user_harts(&self, mask: usize) {
        if mask == 0 {
            return;
        }
        debug!(
            "[tlb] shootdown user mm token={:#x} active_mask={:#b}",
            self.token(),
            mask
        );
        shootdown(
            mask,
            ShootdownKind::AddressSpace {
                token: self.token(),
            },
        );
    }
    /// Assume that no conflicts.
    pub fn insert_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) -> Result<(), MmError> {
        self.insert_vma(
            Vma::new(
                start_va,
                end_va,
                MapType::Framed,
                permission,
                VmaKind::Anonymous,
            ),
            None,
        )
    }
    /// 映射最小用户态 vDSO 页，用于 signal handler 返回时进入 rt_sigreturn。
    pub fn map_user_vdso(&mut self) -> Result<(), MmError> {
        let start_va = VirtAddr::from(USER_VDSO_BASE);
        let end_va = VirtAddr::from(USER_VDSO_BASE + PAGE_SIZE);
        let vma = Vma::new_vdso(start_va, end_va);
        self.insert_vma(vma, Some(ArchTrapMachine::rt_sigreturn_trampoline()))
    }
    /// 根据起始虚拟页号删除一段用户区域，并延迟释放拆下的旧页对象。
    pub(crate) fn remove_vma_with_start_vpn_user_deferred(
        &mut self,
        start_vpn: VirtPageNum,
    ) -> UserReleaseBatch {
        let Some(mut area) = self.vmas.remove(&start_vpn) else {
            return UserReleaseBatch::new();
        };
        let mut batch = UserReleaseBatch::new();
        area.teardown_user_deferred(&mut self.page_table, &mut batch);
        self.finish_deferred_page_table_edit();
        batch
    }
    /// 根据起始虚拟页号删除一段 framed 区域，并返回其中拆下的页框。
    ///
    /// 当前只为 kernel stack 与线程用户资源这类独占 framed VMA 的 deferred 回收准备。
    /// TODO：若后续要让更多内核态映射复用这条路径，需要补齐 direct cache page
    /// 与共享私有页的语义约束。
    pub fn remove_vma_with_start_vpn_deferred(
        &mut self,
        start_vpn: VirtPageNum,
    ) -> Vec<FrameTracker> {
        let Some(mut area) = self.vmas.remove(&start_vpn) else {
            return Vec::new();
        };
        let frames = area.teardown_deferred(&mut self.page_table);
        self.finish_deferred_page_table_edit();
        frames
    }
    /// 根据起始虚拟页号删除一段已经登记的区域。
    pub fn remove_vma_with_start_vpn(&mut self, start_vpn: VirtPageNum) {
        if let Some(mut area) = self.vmas.remove(&start_vpn) {
            let _ = area.teardown_deferred(&mut self.page_table);
            self.finish_deferred_page_table_edit();
        }
    }
    /// 判断给定区间是否与当前地址空间中的任意区域重叠。
    pub fn overlaps_vma_range(&self, start_vpn: VirtPageNum, end_vpn: VirtPageNum) -> bool {
        if start_vpn >= end_vpn {
            return true;
        }
        if let Some((_, prev)) = self.vmas.range(..=start_vpn).next_back() {
            if prev.end_vpn() > start_vpn {
                return true;
            }
        }
        if let Some((next_start, _)) = self.vmas.range(start_vpn..).next() {
            if *next_start < end_vpn {
                return true;
            }
        }
        false
    }
    /// 按起始虚拟页号查找一段区域。
    pub fn find_vma(&self, start_vpn: VirtPageNum) -> Option<&Vma> {
        self.vmas.get(&start_vpn)
    }
    /// 按任意落点虚拟页查找所属区域。
    pub fn find_vma_containing(&self, vpn: VirtPageNum) -> Option<&Vma> {
        self.vmas
            .range(..=vpn)
            .next_back()
            .and_then(|(_, vma)| vma.contains_vpn(vpn).then_some(vma))
    }
    /// 按起始虚拟页号查找一段可变区域，供扩缩容等操作复用。
    pub fn find_vma_mut(&mut self, start_vpn: VirtPageNum) -> Option<&mut Vma> {
        self.vmas.get_mut(&start_vpn)
    }
    /// 按任意落点虚拟页查找可变区域。
    pub fn find_vma_containing_mut(&mut self, vpn: VirtPageNum) -> Option<&mut Vma> {
        let start_vpn = self
            .vmas
            .range(..=vpn)
            .next_back()
            .and_then(|(_, vma)| vma.contains_vpn(vpn).then_some(vma.start_vpn()))?;
        self.vmas.get_mut(&start_vpn)
    }
    fn insert_vma_unchecked(&mut self, vma: Vma) {
        self.vmas.insert(vma.start_vpn(), vma);
    }
    fn rebuild_vmas_from_vec(&mut self, areas: Vec<Vma>) {
        self.vmas.clear();
        for area in areas {
            self.insert_vma_unchecked(area);
        }
    }
    /// 将一段区域登记到地址空间并立即建立页表映射；若与现有区域冲突则失败。
    pub fn insert_vma(&mut self, mut vma: Vma, data: Option<&[u8]>) -> Result<(), MmError> {
        if vma.is_user_accessible() && VirtAddr::from(vma.end_vpn()).0 > USER_SPACE_END {
            return Err(MmError::PermissionDenied);
        }
        #[cfg(target_arch = "loongarch64")]
        if vma.is_user_accessible()
            && overlaps_kernel_heap_range(
                usize::from(VirtAddr::from(vma.start_vpn())),
                usize::from(VirtAddr::from(vma.end_vpn())),
            )
        {
            return Err(MmError::PermissionDenied);
        }
        if self.overlaps_vma_range(vma.start_vpn(), vma.end_vpn()) {
            return Err(MmError::Conflict);
        }
        if vma.should_eager_map() {
            vma.map(&mut self.page_table)?;
        }
        if let Some(data) = data {
            vma.copy_data(&mut self.page_table, data);
        }
        self.insert_vma_unchecked(vma);
        Ok(())
    }
    /// Like `insert_vma` but always eagerly maps the pages regardless of `should_eager_map`.
    pub fn insert_vma_eager(&mut self, mut vma: Vma) -> Result<(), MmError> {
        #[cfg(target_arch = "loongarch64")]
        if vma.is_user_accessible()
            && overlaps_kernel_heap_range(
                usize::from(VirtAddr::from(vma.start_vpn())),
                usize::from(VirtAddr::from(vma.end_vpn())),
            )
        {
            return Err(MmError::PermissionDenied);
        }
        if self.overlaps_vma_range(vma.start_vpn(), vma.end_vpn()) {
            return Err(MmError::Conflict);
        }
        vma.map(&mut self.page_table)?;
        self.insert_vma_unchecked(vma);
        Ok(())
    }
    /// 仅登记一段 VMA 元数据，不立即建立页表映射。
    pub fn register_vma_metadata(&mut self, vma: Vma) -> Result<(), MmError> {
        if self.overlaps_vma_range(vma.start_vpn(), vma.end_vpn()) {
            return Err(MmError::Conflict);
        }
        self.insert_vma_unchecked(vma);
        Ok(())
    }
    /// 把一张已有私有页接入指定虚拟页，供 `fork` 共享与后续 COW 使用。
    pub fn map_existing_private_page(
        &mut self,
        vpn: VirtPageNum,
        page: Arc<PrivatePage>,
        flags: PTEFlags,
    ) -> Result<(), MmError> {
        if self.page_table.translate(vpn).is_some() {
            return Err(MmError::Conflict);
        }
        let Some(area) = self.find_vma_containing_mut(vpn) else {
            return Err(MmError::NoMapping);
        };
        area.data_frames.insert(vpn, Arc::clone(&page));
        self.page_table.map(vpn, page.ppn(), flags)?;
        // debug!(
        //     "[cow] install shared private page: vpn={:#x} ppn={:#x} writable={} cow={}",
        //     vpn.0,
        //     page.ppn().0,
        //     flags.contains(PTEFlags::W),
        //     page.is_cow()
        // );
        Ok(())
    }

    /// Install a consecutive run of already resident private pages in one
    /// page-table batch.  The VMA metadata is updated only after all leaf PTE
    /// writes have succeeded.
    pub(crate) fn map_existing_private_pages_batch(
        &mut self,
        pages: &[(VirtPageNum, Arc<PrivatePage>, PTEFlags)],
    ) -> Result<(), MmError> {
        let Some((start_vpn, _, _)) = pages.first() else {
            return Ok(());
        };
        for (index, (vpn, _, _)) in pages.iter().enumerate() {
            if vpn.0 != start_vpn.0 + index {
                return Err(MmError::InvalidRange);
            }
        }
        let end_vpn = VirtPageNum(start_vpn.0 + pages.len());
        let area_start = self
            .find_vma_containing(*start_vpn)
            .filter(|area| end_vpn <= area.end_vpn())
            .map(Vma::start_vpn)
            .ok_or(MmError::NoMapping)?;
        let entries: Vec<_> = pages
            .iter()
            .map(|(_, page, flags)| (page.ppn(), *flags))
            .collect();
        self.page_table
            .map_preallocated_range(*start_vpn, entries.as_slice())?;
        let area = self.vmas.get_mut(&area_start).ok_or(MmError::NoMapping)?;
        for (vpn, page, _) in pages {
            area.data_frames.insert(*vpn, Arc::clone(page));
        }
        Ok(())
    }
    /// 把一张已有的 page cache 页直接接入指定虚拟页，供 `fork` 继承只读文件私有映射。
    pub fn map_existing_direct_cache_page(
        &mut self,
        vpn: VirtPageNum,
        page: Arc<SpinNoIrqLock<CachePage>>,
        flags: PTEFlags,
    ) -> Result<(), MmError> {
        if self.page_table.translate(vpn).is_some() {
            return Err(MmError::Conflict);
        }
        let Some(area) = self.find_vma_containing_mut(vpn) else {
            return Err(MmError::NoMapping);
        };
        let ppn = retain_mapped_page(&page);
        area.direct_cache_pages.insert(vpn, Arc::clone(&page));
        self.page_table.map(vpn, ppn, flags)?;
        // debug!(
        //     "[cow] install inherited direct cache page: vpn={:#x} ppn={:#x} writable={}",
        //     vpn.0,
        //     page.lock().ppn().0,
        //     flags.contains(PTEFlags::W)
        // );
        Ok(())
    }
    /// 在完成分裂、删除或追加后整理可合并的相邻区域。
    pub fn merge_adjacent_vmas(&mut self) {
        let old_vmas = core::mem::take(&mut self.vmas);
        let mut merged: Vec<Vma> = Vec::new();
        for area in old_vmas.into_values() {
            if let Some(last) = merged.last_mut() {
                if last.can_merge_with(&area) {
                    last.absorb(area);
                    continue;
                }
            }
            merged.push(area);
        }
        self.rebuild_vmas_from_vec(merged);
    }

    /// Merge at most the direct neighbours around `key`.
    fn merge_vma_around(&mut self, key: VirtPageNum) {
        let Some(mut area) = self.vmas.remove(&key) else {
            return;
        };
        if let Some(left_key) = self
            .vmas
            .range(..area.start_vpn())
            .next_back()
            .map(|(key, _)| *key)
        {
            let can_merge = self
                .vmas
                .get(&left_key)
                .is_some_and(|left| left.can_merge_with(&area));
            if can_merge {
                let Some(mut merged) = self.vmas.remove(&left_key) else {
                    return;
                };
                merged.absorb(area);
                area = merged;
            }
        }
        if let Some(right_key) = self
            .vmas
            .range(area.end_vpn()..)
            .next()
            .map(|(key, _)| *key)
        {
            let can_merge = self
                .vmas
                .get(&right_key)
                .is_some_and(|right| area.can_merge_with(right));
            if can_merge {
                let Some(right) = self.vmas.remove(&right_key) else {
                    return;
                };
                area.absorb(right);
            }
        }
        self.insert_vma_unchecked(area);
    }
    /// Find a free user mmap range using a hint first, then wrap to the base.
    pub fn find_free_mmap_area(&self, hint: usize, base: usize, len: usize) -> Option<usize> {
        let upper = USER_SPACE_END;
        let start = align_up(hint.max(base), PAGE_SIZE)?;
        self.find_free_area_in_range(start, upper, len).or_else(|| {
            if start > base {
                self.find_free_area_in_range(base, start, len)
            } else {
                None
            }
        })
    }
    fn find_free_area_in_range(&self, start: usize, upper: usize, len: usize) -> Option<usize> {
        if len == 0 || start >= upper || len > upper.checked_sub(start)? {
            return None;
        }
        let mut candidate = align_up(start, PAGE_SIZE)?;
        loop {
            let candidate_end = candidate.checked_add(len)?;
            if candidate_end > upper {
                return None;
            }
            #[cfg(target_arch = "loongarch64")]
            if overlaps_kernel_heap_range(candidate, candidate_end) {
                candidate = align_up(
                    KERNEL_HEAP_BASE.saturating_add(MAX_KERNEL_HEAP_SIZE),
                    PAGE_SIZE,
                )?;
                continue;
            }
            let candidate_vpn = VirtAddr::from(candidate).floor();
            if let Some((_, prev)) = self.vmas.range(..=candidate_vpn).next_back() {
                let prev_end = VirtAddr::from(prev.end_vpn()).0;
                if prev_end > candidate {
                    candidate = align_up(prev_end, PAGE_SIZE)?;
                    continue;
                }
            }
            if let Some((_, next)) = self.vmas.range(candidate_vpn..).next() {
                let next_start = VirtAddr::from(next.start_vpn()).0;
                if candidate_end <= next_start {
                    return Some(candidate);
                }
                candidate = align_up(VirtAddr::from(next.end_vpn()).0, PAGE_SIZE)?;
            } else {
                return Some(candidate);
            }
        }
    }
    /// 当前进程用户态 VMA 占用的总字节数。
    pub fn user_vma_bytes(&self) -> usize {
        self.vmas
            .values()
            .filter(|vma| vma.is_user_accessible())
            .map(|vma| vma.byte_len())
            .sum()
    }
    /// Mention that trampoline is not collected by areas.
    fn map_trampoline(&mut self) -> Result<(), MmError> {
        let trampoline_pa = crate::platform::direct_map_virt_to_phys(strampoline as usize);

        self.page_table.map(
            VirtAddr::from(TRAMPOLINE).into(),
            PhysAddr::from(trampoline_pa).into(),
            PTEFlags::R | PTEFlags::X,
        )?;
        Ok(())
    }
    /// Without kernel stacks.
    pub fn new_kernel() -> Self {
        let mut output = MaybeUninit::<Self>::uninit();
        unsafe { Self::init_bare_at(output.as_mut_ptr(), super::asid::KERNEL_ASID) }
            .expect("failed to allocate boot-time kernel root page table");
        let mut memory_set = unsafe { output.assume_init() };
        // map trampoline
        memory_set
            .map_trampoline()
            .expect("failed to map boot-time kernel trampoline");
        // On LoongArch, kernel sections, physical memory and task kernel stacks
        // are covered by DMW windows. Only the user-trap trampoline needs an
        // explicit page-table mapping.
        #[cfg(not(target_arch = "loongarch64"))]
        {
            // map kernel sections
            info!(".text [{:#x}, {:#x})", stext as usize, etext as usize);
            info!(".rodata [{:#x}, {:#x})", srodata as usize, erodata as usize);
            info!(".data [{:#x}, {:#x})", sdata as usize, edata as usize);
            info!(
                ".bss [{:#x}, {:#x})",
                sbss_with_stack as usize, ebss as usize
            );
            info!("mapping .text section");
            memory_set
                .insert_vma(
                    Vma::new(
                        (stext as usize).into(),
                        (etext as usize).into(),
                        MapType::Direct,
                        MapPermission::R | MapPermission::X,
                        VmaKind::Kernel,
                    ),
                    None,
                )
                .expect("failed to map kernel text");
            info!("mapping .rodata section");
            memory_set
                .insert_vma(
                    Vma::new(
                        (srodata as usize).into(),
                        (erodata as usize).into(),
                        MapType::Direct,
                        MapPermission::R,
                        VmaKind::Kernel,
                    ),
                    None,
                )
                .expect("failed to map kernel rodata");
            info!("mapping .data section");
            memory_set
                .insert_vma(
                    Vma::new(
                        (sdata as usize).into(),
                        (edata as usize).into(),
                        MapType::Direct,
                        MapPermission::R | MapPermission::W,
                        VmaKind::Kernel,
                    ),
                    None,
                )
                .expect("failed to map kernel data");
            info!("mapping .bss section");
            memory_set
                .insert_vma(
                    Vma::new(
                        (sbss_with_stack as usize).into(),
                        (ebss as usize).into(),
                        MapType::Direct,
                        MapPermission::R | MapPermission::W,
                        VmaKind::Kernel,
                    ),
                    None,
                )
                .expect("failed to map kernel bss");
            info!("mapping physical memory");
            let kernel_start = crate::platform::direct_map_virt_to_phys(skernel as usize);
            let kernel_end = crate::platform::direct_map_virt_to_phys(ekernel as usize);
            bootinfo::for_each_usable_memory_region(|region| {
                let start = align_up_to_page(region.start);
                let end = align_down_to_page(region.end);
                map_kernel_ram_fragment(&mut memory_set, start, kernel_start.min(end));
                map_kernel_ram_fragment(&mut memory_set, kernel_end.max(start), end);
            });
            info!("mapping memory-mapped registers");
            for region in bootinfo::get().mmio_regions() {
                let start = crate::platform::mmio_phys_to_virt(region.start);
                memory_set
                    .insert_vma(
                        Vma::new(
                            start.into(),
                            (start + region.end - region.start).into(),
                            MapType::Direct,
                            MapPermission::R | MapPermission::W,
                            VmaKind::Kernel,
                        ),
                        None,
                    )
                    .expect("failed to map mmio window");
            }
        } // end #[cfg(not(loongarch64))]
        #[cfg(target_arch = "riscv64")]
        memory_set.page_table.mark_kernel_half_global();
        memory_set
    }
    /// Load an ELF file and construct the initial user address space.
    pub fn from_elf_file(
        file: Arc<OSInode>,
    ) -> Result<(Self, UserSpaceLayout, ElfLoadInfo), MmError> {
        let mut memory_set = Self::new_bare()?;
        #[cfg(target_arch = "loongarch64")]
        memory_set.map_trampoline()?;
        memory_set.map_user_vdso()?;
        let loaded = ElfLoader::new(&mut memory_set).load_file(&file)?;
        let layout = UserSpaceLayout {
            start_brk: loaded.image_end,
            mmap_base: USER_MMAP_BASE,
            ustack_base: USER_STACK_BASE,
            start_stack: USER_STACK_BASE + USER_STACK_SIZE,
        };
        Ok((memory_set, layout, loaded.info))
    }

    /// Load one ELF file into an existing address space. The optional forced
    /// load bias is used for the dynamic linker; None selects the normal
    /// ET_EXEC/PIE policy used by the main executable.
    pub fn load_elf_file_at(
        &mut self,
        file: &Arc<OSInode>,
        forced_load_bias: Option<usize>,
    ) -> Result<(ElfLoadInfo, usize), MmError> {
        let loaded = ElfLoader::new(self).load_file_at(file, forced_load_bias)?;
        Ok((loaded.info, loaded.image_end))
    }

    /// Include ELF segments and trampoline, and compute initial process VM layout.
    /// Returns (MemorySet, UserSpaceLayout, ElfLoadInfo).
    pub fn from_elf(elf_data: &[u8]) -> Result<(Self, UserSpaceLayout, ElfLoadInfo), MmError> {
        let mut memory_set = Self::new_bare()?;
        #[cfg(target_arch = "loongarch64")]
        memory_set.map_trampoline()?;
        memory_set.map_user_vdso()?;
        let loaded = ElfLoader::new(&mut memory_set).load_bytes(elf_data)?;
        let layout = UserSpaceLayout {
            start_brk: loaded.image_end,
            mmap_base: USER_MMAP_BASE,
            ustack_base: USER_STACK_BASE,
            start_stack: USER_STACK_BASE + USER_STACK_SIZE,
        };
        Ok((memory_set, layout, loaded.info))
    }
    /// Create a new address space by copy code&data from a exited process's address space.
    pub fn from_existed_user(user_space: &mut Self) -> Result<(Self, bool), MmError> {
        let clone_start_ns = get_time_ns();
        let mut memory_set = Self::new_bare()?;
        #[cfg(target_arch = "loongarch64")]
        memory_set.map_trampoline()?;
        let mut parent_tlb_needs_flush = false;
        let mut shared_anon_vmas = 0usize;
        let mut shared_private_vmas = 0usize;
        let mut copied_private_pages = 0usize;
        let mut shared_private_pages = 0usize;
        let mut shared_anon_pages = 0usize;
        let mut inherited_direct_cache_pages = 0usize;
        debug!(
            "[cow] fork clone address space: parent_vmas={}",
            user_space.vmas.len()
        );
        // copy data sections/trap_context/user_stack
        let parent_vma_starts: Vec<_> = user_space.vmas.keys().copied().collect();
        for area_start in parent_vma_starts {
            let Some(area) = user_space.vmas.get(&area_start) else {
                continue;
            };
            let share_private_pages = area.supports_private_page_sharing();
            let new_area = area.clone_metadata();
            if area.shared_anon {
                shared_anon_vmas += 1;
            } else if share_private_pages {
                shared_private_vmas += 1;
            }
            // debug!(
            //     "[cow] fork inspect VMA: start={:#x} end={:#x} kind={:?} share_private_pages={} private_pages={} direct_cache_pages={}",
            //     area.start_vpn().0,
            //     area.end_vpn().0,
            //     area.kind,
            //     share_private_pages,
            //     area.data_frames.len(),
            //     area.direct_cache_pages.len()
            // );
            if area.shared_anon {
                memory_set.register_vma_metadata(new_area)?;
            } else if share_private_pages {
                memory_set.register_vma_metadata(new_area)?;
            } else {
                memory_set.insert_vma(new_area, None)?;
            }
            // 对于可共享的私有页，`fork` 时父子先共用同一张只读页，写时再复制。
            // 对于 trap context 之类内核内部页，仍然保持直接复制，避免把内核写路径卷入 COW。
            let private_pages: Vec<_> = area
                .data_frames
                .iter()
                .map(|(&vpn, page)| (vpn, Arc::clone(page)))
                .collect();
            let map_perm = area.map_perm;
            let file_shared = area.file.as_ref().map(|file| file.shared).unwrap_or(false);
            let direct_cache_pages: Vec<_> = area
                .direct_cache_pages
                .iter()
                .map(|(&vpn, page)| (vpn, Arc::clone(page)))
                .collect();
            let inherit_direct_cache_pages = area.file.is_some();
            if area.shared_anon || share_private_pages {
                // First prepare the child entries and parent write-protection,
                // then install adjacent entries in leaf-table-sized batches.
                let mut shared_pages = Vec::with_capacity(private_pages.len());
                let mut parent_updates = Vec::new();
                for (vpn, page) in private_pages {
                    let mut child_flags = user_space.translate(vpn).unwrap().flags();
                    if area.shared_anon {
                        shared_anon_pages += 1;
                        child_flags.remove(PTEFlags::D);
                    } else {
                        shared_private_pages += 1;
                        if area.is_shared_anonymous() {
                            shared_pages.push((vpn, page, child_flags));
                            continue;
                        }
                        child_flags.remove(PTEFlags::D);
                        if map_perm.contains(MapPermission::W) {
                            // 将父子双方都降为只读，后续写入通过缺页走 COW。
                            page.set_cow(true);
                            child_flags.remove(PTEFlags::W);
                            parent_updates.push((vpn, child_flags));
                            parent_tlb_needs_flush = true;
                        }
                    }
                    shared_pages.push((vpn, page, child_flags));
                }

                let mut run_start = 0usize;
                while run_start < shared_pages.len() {
                    let mut run_end = run_start + 1;
                    while run_end < shared_pages.len()
                        && shared_pages[run_end].0.0
                            == shared_pages[run_end - 1].0.0 + 1
                    {
                        run_end += 1;
                    }
                    memory_set
                        .map_existing_private_pages_batch(&shared_pages[run_start..run_end])?;
                    run_start = run_end;
                }

                // Parent PTEs normally have identical flags across a run, so
                // collapse the write-protection edits too.  Fall back to a
                // single-page update when hardware flags differ.
                let mut update_start = 0usize;
                while update_start < parent_updates.len() {
                    let (start_vpn, flags) = parent_updates[update_start];
                    let mut update_end = update_start + 1;
                    while update_end < parent_updates.len()
                        && parent_updates[update_end].0.0
                            == parent_updates[update_end - 1].0.0 + 1
                        && parent_updates[update_end].1 == flags
                    {
                        update_end += 1;
                    }
                    if update_end - update_start > 1 {
                        let _ = user_space.page_table.update_flags_range(
                            start_vpn,
                            update_end - update_start,
                            flags,
                        );
                    } else {
                        let _ = user_space.page_table.update_flags(start_vpn, flags);
                    }
                    update_start = update_end;
                }
            } else {
                for (vpn, _) in private_pages {
                    if memory_set.translate(vpn).is_none() {
                        memory_set.map_private_page_in_vma(vpn)?;
                    }
                    copied_private_pages += 1;
                    let src_ppn = user_space.translate(vpn).unwrap().ppn();
                    let dst_ppn = memory_set.translate(vpn).unwrap().ppn();
                    dst_ppn
                        .get_bytes_array()
                        .copy_from_slice(src_ppn.get_bytes_array());
                    debug!(
                        "[cow] fork copy private page directly: vpn={:#x} src_ppn={:#x} dst_ppn={:#x}",
                        vpn.0, src_ppn.0, dst_ppn.0
                    );
                }
            }
            // 对于已经直接映到 page cache 的文件页，子进程也直接继承当前映射。
            // `MAP_PRIVATE` 仍然保持只读，`MAP_SHARED` 在 sticky dirty 语义下保留父进程当前 `W` 状态。
            if inherit_direct_cache_pages {
                for (vpn, page) in direct_cache_pages {
                    if memory_set.translate(vpn).is_some() {
                        continue;
                    }
                    inherited_direct_cache_pages += 1;
                    let mut child_flags = user_space.translate(vpn).unwrap().flags();
                    child_flags.remove(PTEFlags::D);
                    if !file_shared {
                        child_flags.remove(PTEFlags::W);
                    }
                    debug!(
                        "[cow] fork inherit direct cache page: vpn={:#x} ppn={:#x} shared={} writable={}",
                        vpn.0,
                        page.lock().ppn().0,
                        file_shared,
                        child_flags.contains(PTEFlags::W)
                    );
                    memory_set.map_existing_direct_cache_page(vpn, page, child_flags)?;
                }
            }
        }
        if parent_tlb_needs_flush {
            user_space.flush_local_tlb_asid();
            debug!("[cow] fork flush parent local TLB after write-protecting shared private pages");
        }
        let total_ns = get_time_ns() - clone_start_ns;
        if total_ns >= FORK_MEMORYSET_TIMING_WARN_THRESHOLD_NS {
            debug!(
                "[clone-timing] from_existed_user total_ns={} parent_vmas={} shared_anon_vmas={} shared_private_vmas={} copied_private_pages={} shared_private_pages={} shared_anon_pages={} inherited_direct_cache_pages={} parent_tlb_needs_flush={}",
                total_ns,
                user_space.vmas.len(),
                shared_anon_vmas,
                shared_private_vmas,
                copied_private_pages,
                shared_private_pages,
                shared_anon_pages,
                inherited_direct_cache_pages,
                parent_tlb_needs_flush
            );
        }
        Ok((memory_set, parent_tlb_needs_flush))
    }

    /// Make a parent writable again before creating a shared-MM vfork view.
    ///
    /// A previous ordinary fork may have left this address space holding a
    /// read-only COW mapping. The vfork child must see the parent's writes
    /// directly, so detach those pages once and then share the resulting
    /// writable mapping with the child.
    pub(crate) fn prepare_shared_vfork(&mut self) -> Result<bool, MmError> {
        let area_starts: Vec<_> = self.vmas.keys().copied().collect();
        let mut changed = false;
        for area_start in area_starts {
            let Some(area) = self.vmas.get(&area_start) else {
                continue;
            };
            if !area.supports_private_page_sharing()
                || !area.map_perm.contains(MapPermission::W)
            {
                continue;
            }
            let private_pages: Vec<_> = area
                .data_frames
                .iter()
                .map(|(&vpn, page)| (vpn, Arc::clone(page)))
                .collect();
            for (vpn, page) in private_pages {
                let pte = self.translate(vpn).ok_or(MmError::NoMapping)?;
                if !page.is_cow() || pte.flags().contains(PTEFlags::W) {
                    continue;
                }
                if Arc::strong_count(&page) <= 2 {
                    // Only this VMA and the local snapshot retain the page;
                    // clear the stale fork-COW state in place instead of
                    // copying a page that is already exclusive again.
                    page.set_cow(false);
                    let mut writable_flags = pte.flags();
                    writable_flags.insert(PTEFlags::W);
                    writable_flags.remove(PTEFlags::D);
                    if !self.page_table.update_flags(vpn, writable_flags) {
                        return Err(MmError::NoMapping);
                    }
                    changed = true;
                    continue;
                }
                let writable_page = Arc::new(PrivatePage::new(
                    frame_alloc_with_reclaim().ok_or(MmError::OutOfMemory)?,
                ));
                writable_page
                    .ppn()
                    .get_bytes_array()
                    .copy_from_slice(page.ppn().get_bytes_array());
                let mut writable_flags = pte.flags();
                writable_flags.insert(PTEFlags::W);
                writable_flags.remove(PTEFlags::D);
                if !self
                    .page_table
                    .replace(vpn, writable_page.ppn(), writable_flags)
                {
                    return Err(MmError::NoMapping);
                }
                self.vmas
                    .get_mut(&area_start)
                    .ok_or(MmError::NoMapping)?
                    .data_frames
                    .insert(vpn, writable_page);
                changed = true;
            }
        }
        Ok(changed)
    }

    /// Build a temporary `MemorySet` that uses the parent's root and ASID.
    /// No root frame or page-table descendants are allocated on this path.
    pub(crate) fn from_shared_vfork_view(user_space: &Self) -> Result<Self, MmError> {
        let mut vmas = BTreeMap::new();
        for (&start_vpn, area) in &user_space.vmas {
            vmas.insert(start_vpn, area.clone_shared_view());
        }
        Ok(Self {
            page_table: PageTable::borrowed_from(&user_space.page_table),
            vmas,
            asid: user_space.asid,
            active_user_harts: AtomicUsize::new(0),
            tlb_generation: AtomicUsize::new(1),
            seen_tlb_generation: [const { AtomicUsize::new(0) }; MAX_HARTS],
            shared_vfork_view: true,
        })
    }

    /// Extract the metadata and page-table descendants owned by a shared
    /// vfork view before its borrowed root is dropped.
    pub(crate) fn take_shared_vfork_state(&mut self) -> Option<SharedMemorySetState> {
        if !self.shared_vfork_view {
            return None;
        }
        Some(SharedMemorySetState {
            vmas: core::mem::take(&mut self.vmas).into_values().collect(),
            page_table_frames: self.page_table.take_owned_frames(),
        })
    }

    /// Adopt the child's shared-view metadata and any page-table descendants
    /// allocated while the parent was blocked in vfork.
    pub(crate) fn adopt_shared_vfork_state(&mut self, state: SharedMemorySetState) {
        debug_assert!(!self.shared_vfork_view);
        self.vmas.clear();
        for area in state.vmas {
            self.vmas.insert(area.start_vpn(), area);
        }
        self.page_table.append_owned_frames(state.page_table_frames);
    }

    pub(crate) fn is_shared_vfork_view(&self) -> bool {
        self.shared_vfork_view
    }

    /// Create a vfork-compatible address space for a process-style
    /// `clone(CLONE_VM)`.
    ///
    /// The child needs a distinct page table because kernel-managed per-task
    /// pages such as trap contexts cannot be shared.  Resident user pages are
    /// nevertheless mapped to the same physical pages.  Before doing so,
    /// writable pages that still carry fork COW protection are detached for
    /// the parent and then shared writable with the CLONE_VM child.  Otherwise
    /// a child write (notably glibc's posix_spawn `args.err`) would COW into a
    /// child-private page and remain invisible to the resumed parent.
    ///
    /// Returns whether parent PTEs were relaxed/replaced and therefore require
    /// a parent-address-space TLB shootdown.
    pub fn from_existed_user_shared_vm(user_space: &mut Self) -> Result<(Self, bool), MmError> {
        let mut memory_set = Self::new_bare()?;
        memory_set.map_trampoline()?;
        let mut parent_tlb_needs_flush = false;
        let parent_vma_starts: Vec<_> = user_space.vmas.keys().copied().collect();
        for area_start in parent_vma_starts {
            let (
                share_private_pages,
                cow_private_pages,
                new_area,
                private_pages,
                direct_cache_pages,
                inherit_direct_cache_pages,
                map_perm,
            ) = {
                let Some(area) = user_space.vmas.get(&area_start) else {
                    continue;
                };
                let cow_private_pages = area.supports_private_page_sharing();
                (
                    cow_private_pages || area.shared_anon || area.is_shared_anonymous(),
                    cow_private_pages,
                    area.clone_metadata(),
                    area.data_frames
                        .iter()
                        .map(|(&vpn, page)| (vpn, Arc::clone(page)))
                        .collect::<Vec<_>>(),
                    area.direct_cache_pages
                        .iter()
                        .map(|(&vpn, page)| (vpn, Arc::clone(page)))
                        .collect::<Vec<_>>(),
                    area.file.is_some(),
                    area.map_perm,
                )
            };
            if share_private_pages {
                memory_set.register_vma_metadata(new_area)?;
            } else {
                memory_set.insert_vma(new_area, None)?;
            }

            for (vpn, mut page) in private_pages {
                if share_private_pages {
                    let mut flags = user_space.translate(vpn).ok_or(MmError::NoMapping)?.flags();

                    // A normal fork can leave a writable VMA backed by a
                    // read-only COW page.  Sharing that PTE unchanged would
                    // make the CLONE_VM child take a private COW fault, which
                    // violates vfork/posix_spawn's shared-memory contract.
                    if cow_private_pages
                        && map_perm.contains(MapPermission::W)
                        && page.is_cow()
                        && !flags.contains(PTEFlags::W)
                    {
                        let shared_page = Arc::new(PrivatePage::new(
                            frame_alloc_with_reclaim().ok_or(MmError::OutOfMemory)?,
                        ));
                        shared_page
                            .ppn()
                            .get_bytes_array()
                            .copy_from_slice(page.ppn().get_bytes_array());
                        let mut writable_flags = flags;
                        writable_flags.insert(PTEFlags::W);
                        writable_flags.remove(PTEFlags::D);
                        if !user_space
                            .page_table
                            .replace(vpn, shared_page.ppn(), writable_flags)
                        {
                            return Err(MmError::NoMapping);
                        }
                        user_space
                            .vmas
                            .get_mut(&area_start)
                            .ok_or(MmError::NoMapping)?
                            .data_frames
                            .insert(vpn, Arc::clone(&shared_page));
                        page = shared_page;
                        flags = writable_flags;
                        parent_tlb_needs_flush = true;
                    }
                    memory_set.map_existing_private_page(vpn, page, flags)?;
                    continue;
                }
                if memory_set.translate(vpn).is_none() {
                    memory_set.map_private_page_in_vma(vpn)?;
                }
                let src_ppn = user_space.translate(vpn).unwrap().ppn();
                let dst_ppn = memory_set.translate(vpn).unwrap().ppn();
                dst_ppn
                    .get_bytes_array()
                    .copy_from_slice(src_ppn.get_bytes_array());
            }

            if inherit_direct_cache_pages {
                for (vpn, page) in direct_cache_pages {
                    if memory_set.translate(vpn).is_some() {
                        continue;
                    }
                    let flags = user_space.translate(vpn).unwrap().flags();
                    memory_set.map_existing_direct_cache_page(vpn, page, flags)?;
                }
            }
        }
        Ok((memory_set, parent_tlb_needs_flush))
    }
    /// Change page table by activating the current architecture token.
    pub fn activate(&self) {
        unsafe {
            crate::hal::activate_address_space(self.token());
        }
    }
    /// Translate a virtual page number to a page table entry
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }

    /// 拆除全部用户 VMA，并把旧页对象放入延迟释放批次。
    pub(crate) fn recycle_data_pages_deferred(&mut self) -> UserReleaseBatch {
        // Exit and exec must execute prepare_msync_range() before entering
        // this non-blocking teardown phase. Performing writeback here would
        // sleep while the caller holds the process SpinNoIrqLock.
        if self.shared_vfork_view {
            // The root and all resident mappings belong to the parent.  The
            // vfork owner extracts VMA/page-table state explicitly before
            // this teardown path runs.
            self.vmas.clear();
            return UserReleaseBatch::new();
        }
        let mut batch = UserReleaseBatch::new();
        for area in self.vmas.values_mut() {
            area.teardown_user_deferred(&mut self.page_table, &mut batch);
        }
        self.vmas.clear();
        self.finish_deferred_page_table_edit();
        batch
    }

    /// Remove all VMAs
    pub fn recycle_data_pages(&mut self) {
        if self.shared_vfork_view {
            self.vmas.clear();
            return;
        }
        for area in self.vmas.values_mut() {
            let _ = area.teardown_deferred(&mut self.page_table);
        }
        self.vmas.clear();
        self.flush_local_tlb_asid();
    }

    /// 将用户区域收缩到新的上界，并延迟释放被拆下的旧页对象。
    pub(crate) fn shrink_to_deferred(
        &mut self,
        start: VirtAddr,
        new_end: VirtAddr,
    ) -> Option<UserReleaseBatch> {
        let start_vpn = start.floor();
        let Some(area) = self.vmas.get_mut(&start_vpn) else {
            return None;
        };
        let mut batch = UserReleaseBatch::new();
        area.shrink_to_deferred(&mut self.page_table, new_end.ceil(), &mut batch);
        self.finish_deferred_page_table_edit();
        Some(batch)
    }

    /// 按 `brk` 语义收缩 heap，覆盖被 `mprotect` 拆分出的所有 heap VMA。
    pub(crate) fn shrink_heap_to_deferred(
        &mut self,
        heap_start: VirtAddr,
        new_end: VirtAddr,
    ) -> Option<UserReleaseBatch> {
        let heap_start_vpn = heap_start.floor();
        let new_end_vpn = new_end.ceil();
        if new_end_vpn < heap_start_vpn {
            return None;
        }

        let keys: Vec<VirtPageNum> = self
            .vmas
            .range(heap_start_vpn..)
            .filter_map(|(start, area)| area.is_heap().then_some(*start))
            .collect();

        let mut batch = UserReleaseBatch::new();
        let mut changed = false;
        for start in keys {
            let Some(area) = self.vmas.get_mut(&start) else {
                continue;
            };
            if area.end_vpn() <= new_end_vpn {
                continue;
            }
            if area.start_vpn() < new_end_vpn {
                area.shrink_to_deferred(&mut self.page_table, new_end_vpn, &mut batch);
                changed = true;
                continue;
            }
            let mut area = self.vmas.remove(&start).unwrap();
            area.teardown_user_deferred(&mut self.page_table, &mut batch);
            changed = true;
        }
        if changed {
            self.finish_deferred_page_table_edit();
            self.merge_adjacent_vmas();
        }
        Some(batch)
    }

    /// 失效指定 inode 在 truncate 后越过 EOF 的 file-backed 用户映射。
    pub(crate) fn invalidate_file_mappings_after_truncate_deferred(
        &mut self,
        inode: InodeKey,
        new_size: usize,
    ) -> UserReleaseBatch {
        let mut batch = UserReleaseBatch::new();
        let mut pte_changed = false;
        for area in self.vmas.values_mut() {
            let Some(file) = area.file.as_ref() else {
                continue;
            };
            let Some(area_inode) = file.file.backing_inode() else {
                continue;
            };
            if InodeKey::from_inode(&area_inode) != inode {
                continue;
            }

            // A direct_cache_pages entry aliases the page-cache frame.
            // page_cache::truncate_mapping() owns zeroing the retained tail,
            // so the VM layer only removes mappings that now lie wholly
            // beyond EOF.
            let direct_vpns: Vec<_> = area.direct_cache_pages.keys().copied().collect();
            for vpn in direct_vpns {
                let Some(page_idx) = area.file_page_index(vpn) else {
                    continue;
                };
                let page_start = page_idx as usize * PAGE_SIZE;
                if page_start >= new_size {
                    area.unmap_present_one_deferred_after_truncate(
                        &mut self.page_table,
                        vpn,
                        &mut batch,
                    );
                    pte_changed = true;
                }
            }

            let private_vpns: Vec<_> = area.data_frames.keys().copied().collect();
            for vpn in private_vpns {
                let Some(page_idx) = area.file_page_index(vpn) else {
                    continue;
                };
                let page_start = page_idx as usize * PAGE_SIZE;
                if page_start >= new_size {
                    area.unmap_present_one_deferred_after_truncate(
                        &mut self.page_table,
                        vpn,
                        &mut batch,
                    );
                    pte_changed = true;
                    continue;
                }
                if new_size < page_start + PAGE_SIZE {
                    if let Some(page) = area.data_frames.get(&vpn) {
                        page.ppn().get_bytes_array()[new_size - page_start..].fill(0);
                    }
                }
            }
        }
        if pte_changed {
            self.finish_deferred_page_table_edit();
        }
        // 当前只失效已经 present 的页；未装入页依赖后续 fault 路径用新文件长度拒绝 EOF 外访问。
        batch
    }

    /// shrink the area to new_end
    #[allow(unused)]
    pub fn shrink_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        if let Some(area) = self.vmas.get_mut(&start.floor()) {
            area.shrink_present_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }

    /// 将一段 VMA 收缩到新的上界，只拆除已经实际映射的尾部页。
    pub fn shrink_metadata_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        if let Some(area) = self.vmas.get_mut(&start.floor()) {
            area.shrink_present_to(&mut self.page_table, new_end.ceil());
            self.flush_local_tlb_asid();
            true
        } else {
            false
        }
    }

    /// append the area to new_end
    #[allow(unused)]
    pub fn append_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        let new_end_vpn = new_end.ceil();
        let start_vpn = start.floor();
        let Some(old_end) = self.vmas.get(&start_vpn).map(|vma| vma.end_vpn()) else {
            return false;
        };

        if self.overlaps_vma_range(old_end, new_end_vpn) {
            return false;
        }

        let Some(area) = self.vmas.get_mut(&start_vpn) else {
            return false;
        };
        area.append_to(&mut self.page_table, new_end.ceil());
        true
    }

    /// 将一段 VMA 的元数据扩展到新的上界，不立即补齐页表映射。
    pub fn append_metadata_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        let new_end_vpn = new_end.ceil();
        let start_vpn = start.floor();
        let Some(old_end) = self.vmas.get(&start_vpn).map(|vma| vma.end_vpn()) else {
            return false;
        };
        // `brk` 增长到恰好页边界时，页粒度的 VMA 上界并不会变化；
        // 这种情况下应视为成功的 no-op，而不是误判为区间非法。
        if new_end_vpn == old_end {
            return true;
        }
        if self.overlaps_vma_range(old_end, new_end_vpn) {
            return false;
        }
        let Some(area) = self.vmas.get_mut(&start_vpn) else {
            return false;
        };
        area.vpn_range = VPNRange::new(start_vpn, new_end_vpn);
        true
    }

    /// 按 `brk` 语义扩展 heap 元数据，允许 heap 已被 `mprotect` 拆成多个 VMA。
    pub fn append_heap_metadata_to(
        &mut self,
        heap_start: VirtAddr,
        old_brk: VirtAddr,
        new_brk: VirtAddr,
        permission: MapPermission,
    ) -> Result<(), MmError> {
        let heap_start_vpn = heap_start.floor();
        let old_end_vpn = old_brk.ceil();
        let new_end_vpn = new_brk.ceil();
        if new_end_vpn <= old_end_vpn {
            return Ok(());
        }

        let has_heap = self
            .vmas
            .values()
            .any(|vma| vma.is_heap() && vma.end_vpn() > heap_start_vpn);
        let grow_start = if has_heap {
            old_end_vpn
        } else {
            heap_start_vpn
        };

        for area in self.vmas.values() {
            if area.end_vpn() <= grow_start || area.start_vpn() >= new_end_vpn {
                continue;
            }
            if !area.is_heap() {
                return Err(MmError::Conflict);
            }
        }

        let mut cursor = grow_start;
        let mut new_areas = Vec::new();
        for area in self.vmas.values() {
            if area.end_vpn() <= cursor || area.start_vpn() >= new_end_vpn {
                continue;
            }
            if area.start_vpn() > cursor {
                let end = if area.start_vpn() < new_end_vpn {
                    area.start_vpn()
                } else {
                    new_end_vpn
                };
                new_areas.push(Vma::new_heap(cursor.into(), end.into(), permission));
            }
            if area.end_vpn() > cursor {
                cursor = area.end_vpn();
            }
            if cursor >= new_end_vpn {
                break;
            }
        }
        if cursor < new_end_vpn {
            new_areas.push(Vma::new_heap(cursor.into(), new_end_vpn.into(), permission));
        }

        for area in new_areas {
            self.register_vma_metadata(area)?;
        }
        self.merge_adjacent_vmas();
        Ok(())
    }

    /// map an anonymous area with given permission, return true if success
    pub fn mmap_anonymous(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
        shared: bool,
    ) -> Result<(), MmError> {
        let start_vpn = start_va.floor();
        debug!(
            "[mmap] register anonymous VMA: start={:#x} end={:#x} perm={:?} shared={} eager={}",
            usize::from(start_va),
            usize::from(end_va),
            permission,
            shared,
            shared
        );
        let vma = if shared {
            Vma::new_shared_anonymous(start_va, end_va, permission)
        } else {
            Vma::new_anonymous(start_va, end_va, permission, false)
        };
        if shared {
            self.insert_vma_eager(vma)?;
            self.merge_vma_around(start_vpn);
            self.flush_local_tlb_range_asid(start_va.0, end_va.0);
        } else {
            self.register_vma_metadata(vma)?;
            self.merge_vma_around(start_vpn);
        }
        Ok(())
    }

    /// 登记一个 file-backed 映射区域；真正装页推迟到缺页异常时处理。
    pub fn mmap_file(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
        file: Arc<FileDescription>,
        pgoff: usize,
        shared: bool,
    ) -> Result<(), MmError> {
        debug!(
            "[mmap] register file VMA: start={:#x} end={:#x} perm={:?} pgoff={} shared={} lazy=true path={:?}",
            usize::from(start_va),
            usize::from(end_va),
            permission,
            pgoff,
            shared,
            file.path()
        );
        self.insert_vma(
            Vma::new_file(start_va, end_va, permission, file, pgoff, shared),
            None,
        )?;
        Ok(())
    }

    /// Check whether a range is fully covered by user-accessible VMAs.
    fn range_is_user_mapped(&self, start_vpn: VirtPageNum, end_vpn: VirtPageNum) -> bool {
        if start_vpn >= end_vpn {
            return false;
        }
        let mut cursor = start_vpn;
        while cursor < end_vpn {
            let Some((_, area)) = self
                .vmas
                .range(..=cursor)
                .next_back()
                .filter(|(_, area)| area.contains_vpn(cursor))
            else {
                return false;
            };
            if !area.is_user_accessible() {
                return false;
            }
            cursor = area.end_vpn().min(end_vpn);
        }
        true
    }

    /// Resize or relocate one complete user VMA.
    ///
    /// The syscall layer selects the destination address.  This method keeps
    /// the VMA metadata, present PTEs, private pages and direct page-cache
    /// references in sync.  Any pages removed from a fixed destination are
    /// returned in `UserReleaseBatch` and must only be dropped after the
    /// caller has completed the address-space shootdown.
    pub(crate) fn mremap(
        &mut self,
        old_start_va: VirtAddr,
        old_end_va: VirtAddr,
        new_start_va: VirtAddr,
        new_end_va: VirtAddr,
    ) -> Result<(VirtAddr, UserReleaseBatch), MmError> {
        let old_start_vpn = old_start_va.floor();
        let old_end_vpn = old_end_va.ceil();
        let new_start_vpn = new_start_va.floor();
        let new_end_vpn = new_end_va.ceil();

        if old_start_vpn >= old_end_vpn || new_start_vpn >= new_end_vpn {
            return Err(MmError::InvalidRange);
        }
        if new_start_vpn != old_start_vpn
            && old_start_vpn < new_end_vpn
            && new_start_vpn < old_end_vpn
        {
            return Err(MmError::InvalidRange);
        }

        let Some(source) = self.vmas.get(&old_start_vpn) else {
            return Err(MmError::NoMapping);
        };
        if source.start_vpn() != old_start_vpn
            || source.end_vpn() != old_end_vpn
            || !source.is_user_accessible()
            || !source.supports_mremap()
        {
            // Keeping the first implementation to one complete mmap VMA
            // avoids changing heap/stack bookkeeping or merging unrelated
            // permission/file regions during a relocation.
            return Err(MmError::InvalidRange);
        }

        let mut batch = UserReleaseBatch::new();

        if new_start_vpn == old_start_vpn {
            let mut area = self.vmas.remove(&old_start_vpn).ok_or(MmError::NoMapping)?;

            if new_end_vpn < old_end_vpn {
                area.shrink_to_deferred(&mut self.page_table, new_end_vpn, &mut batch);
            } else if new_end_vpn > old_end_vpn {
                if self.overlaps_vma_range(old_end_vpn, new_end_vpn) {
                    self.insert_vma_unchecked(area);
                    return Err(MmError::AddressUnavailable);
                }
                if area.should_eager_map() {
                    for vpn in VPNRange::new(old_end_vpn, new_end_vpn) {
                        if let Err(err) = self.page_table.ensure_leaf(vpn) {
                            self.insert_vma_unchecked(area);
                            return Err(err);
                        }
                    }
                    if let Err(err) = area.append_to_checked(&mut self.page_table, new_end_vpn) {
                        self.insert_vma_unchecked(area);
                        return Err(err);
                    }
                } else {
                    area.vpn_range = VPNRange::new(old_start_vpn, new_end_vpn);
                }
            }

            self.insert_vma_unchecked(area);
            self.merge_vma_around(old_start_vpn);
            self.finish_deferred_page_table_edit();
            return Ok((new_start_va, batch));
        }

        if new_end_vpn.0 > USER_SPACE_END / PAGE_SIZE {
            return Err(MmError::InvalidRange);
        }

        // MREMAP_FIXED replaces the destination mapping.  The syscall layer
        // has already rejected source/destination overlap; holes in the
        // destination are deliberately not treated as a partial replacement
        // in this first implementation.
        let destination_occupied = self.overlaps_vma_range(new_start_vpn, new_end_vpn);
        if destination_occupied && !self.range_is_user_mapped(new_start_vpn, new_end_vpn) {
            return Err(MmError::AddressUnavailable);
        }

        // PageTable::map can allocate intermediate tables.  Prepare every
        // destination leaf first, so a later PTE move cannot fail halfway
        // through after the old mapping has been modified.
        for vpn in VPNRange::new(new_start_vpn, new_end_vpn) {
            self.page_table.ensure_leaf(vpn)?;
        }

        // Shared anonymous mappings are eagerly populated in this kernel.
        // Allocate their growth pages before changing either the destination
        // or source mapping; the frame allocator can still fail atomically.
        let source_eager = source.should_eager_map();
        let source_map_perm = source.map_perm;
        let old_pages = old_end_vpn.0 - old_start_vpn.0;
        let new_pages = new_end_vpn.0 - new_start_vpn.0;
        let mut extra_pages = Vec::new();
        if source_eager && new_pages > old_pages {
            for offset in old_pages..new_pages {
                let page = Arc::new(PrivatePage::new(
                    frame_alloc_with_reclaim().ok_or(MmError::OutOfMemory)?,
                ));
                extra_pages.push((VirtPageNum(new_start_vpn.0 + offset), page));
            }
        }

        if destination_occupied {
            let mut destination_batch = self
                .munmap_deferred(new_start_va, new_end_va)
                .ok_or(MmError::AddressUnavailable)?;
            batch.append(&mut destination_batch);
        }

        let mut area = self.vmas.remove(&old_start_vpn).ok_or(MmError::NoMapping)?;

        let mut present = Vec::new();
        for offset in 0..old_pages {
            let old_vpn = VirtPageNum(old_start_vpn.0 + offset);
            if let Some(pte) = self.page_table.translate(old_vpn) {
                present.push((offset, pte));
            }
        }

        let pte_flags = Self::map_perm_to_pte_flags(source_map_perm);
        for (vpn, page) in &extra_pages {
            self.page_table
                .map(*vpn, page.ppn(), pte_flags)
                .expect("mremap destination page-table leaf was preflighted");
        }

        for (offset, pte) in &present {
            let new_vpn = VirtPageNum(new_start_vpn.0 + *offset);
            self.page_table
                .map(new_vpn, pte.ppn(), pte.flags())
                .expect("mremap destination page-table leaf was preflighted");
        }
        for offset in 0..old_pages {
            let old_vpn = VirtPageNum(old_start_vpn.0 + offset);
            let _ = self.page_table.clear(old_vpn);
        }

        let old_data_frames = core::mem::take(&mut area.data_frames);
        for (vpn, page) in old_data_frames {
            let offset = vpn.0 - old_start_vpn.0;
            area.data_frames
                .insert(VirtPageNum(new_start_vpn.0 + offset), page);
        }
        let old_direct_cache_pages = core::mem::take(&mut area.direct_cache_pages);
        for (vpn, page) in old_direct_cache_pages {
            let offset = vpn.0 - old_start_vpn.0;
            area.direct_cache_pages
                .insert(VirtPageNum(new_start_vpn.0 + offset), page);
        }
        for (vpn, page) in extra_pages {
            area.data_frames.insert(vpn, page);
        }
        area.vpn_range = VPNRange::new(new_start_vpn, new_end_vpn);

        self.insert_vma_unchecked(area);
        self.merge_vma_around(new_start_vpn);
        self.finish_deferred_page_table_edit();
        Ok((new_start_va, batch))
    }

    /// 按给定用户区间拆除映射，并返回需要在 shootdown 后释放的旧页对象。
    ///
    /// 调用方必须在锁内快照目标 hart，并在锁外完成 shootdown 后再释放返回的
    /// `UserReleaseBatch`。
    pub(crate) fn munmap_deferred(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
    ) -> Option<UserReleaseBatch> {
        let start_vpn = start_va.floor();
        let end_vpn = end_va.ceil();
        // 先按 VMA 级别验证整段区间都被用户态映射覆盖，避免按页查找导致
        // many-small munmap 在大量碎片 VMA 下退化得过于明显。
        let mut overlap_starts = Vec::new();
        let mut cursor = start_vpn;
        while cursor < end_vpn {
            let Some((area_start, area)) = self
                .vmas
                .range(..=cursor)
                .next_back()
                .filter(|(_, area)| area.contains_vpn(cursor))
            else {
                return None;
            };
            if !area.is_user_accessible() {
                return None;
            }
            overlap_starts.push(*area_start);
            cursor = area.end_vpn().min(end_vpn);
        }

        let mut batch = UserReleaseBatch::new();
        let mut merge_candidates = Vec::with_capacity(overlap_starts.len() * 2);
        for area_start in overlap_starts {
            let mut area = self.vmas.remove(&area_start)?;
            let area_start = area.start_vpn();
            let area_end = area.end_vpn();
            let overlap_start = area_start.max(start_vpn);
            let overlap_end = area_end.min(end_vpn);

            let right_area = if overlap_end < area_end {
                area.split_off(overlap_end)
            } else {
                None
            };
            let mut overlap_area = if area_start < overlap_start {
                let overlap_area = area
                    .split_off(overlap_start)
                    .expect("validated overlap split should succeed");
                let left_start = area.start_vpn();
                self.insert_vma_unchecked(area);
                merge_candidates.push(left_start);
                overlap_area
            } else {
                area
            };

            for vpn in VPNRange::new(overlap_start, overlap_end) {
                overlap_area.unmap_present_one_deferred(&mut self.page_table, vpn, &mut batch);
            }

            if let Some(right_area) = right_area {
                let right_start = right_area.start_vpn();
                self.insert_vma_unchecked(right_area);
                merge_candidates.push(right_start);
            }
        }
        for start in merge_candidates {
            self.merge_vma_around(start);
        }
        self.flush_local_tlb_range_asid(start_va.0, end_va.0);
        debug!(
            "[munmap] complete teardown: start_vpn={:#x} end_vpn={:#x}",
            start_vpn.0, end_vpn.0
        );
        Some(batch)
    }

    /// Discard resident pages in a private anonymous user range while keeping
    /// the VMA itself intact.
    ///
    /// `MADV_DONTNEED` is deliberately separate from `munmap_deferred`: the
    /// address range must remain a valid mapping, and a later access must be
    /// able to fault in a fresh zero-filled anonymous page. The returned
    /// private-page references must only be dropped after the caller has
    /// completed the remote TLB shootdown.
    pub(crate) fn madvise_dontneed_deferred(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
    ) -> Result<UserReleaseBatch, MmError> {
        let start_vpn = start_va.floor();
        let end_vpn = end_va.ceil();
        if start_vpn >= end_vpn {
            return Err(MmError::InvalidRange);
        }

        // Validate the complete range before changing any PTE or VMA-owned
        // page state. The first implementation intentionally covers only
        // private anonymous VMAs, which is the mapping type used by jemalloc.
        // Shared anonymous and file-backed mappings need different discard
        // and dirty-page semantics and must not be reported as successful.
        let mut cursor = start_vpn;
        while cursor < end_vpn {
            let Some((_, area)) = self
                .vmas
                .range(..=cursor)
                .next_back()
                .filter(|(_, area)| area.contains_vpn(cursor))
            else {
                return Err(MmError::NoMapping);
            };
            if !area.supports_lazy_user_fault() {
                return Err(MmError::Unsupported);
            }
            cursor = area.end_vpn().min(end_vpn);
        }

        let mut batch = UserReleaseBatch::new();
        cursor = start_vpn;
        while cursor < end_vpn {
            let area_start = self
                .vmas
                .range(..=cursor)
                .next_back()
                .filter(|(_, area)| area.contains_vpn(cursor))
                .map(|(start, _)| *start)
                .ok_or(MmError::NoMapping)?;
            let area_end = self
                .vmas
                .get(&area_start)
                .map(|area| area.end_vpn())
                .ok_or(MmError::NoMapping)?;
            let overlap_end = area_end.min(end_vpn);

            // Only resident anonymous pages have entries in data_frames.
            // Collect the keys first so that removing pages does not mutate a
            // BTreeMap iterator while it is being traversed.
            let resident_vpns = self
                .vmas
                .get(&area_start)
                .ok_or(MmError::NoMapping)?
                .data_frames
                .range(cursor..overlap_end)
                .map(|(&vpn, _)| vpn)
                .collect::<Vec<_>>();
            let area = self.vmas.get_mut(&area_start).ok_or(MmError::NoMapping)?;
            for vpn in resident_vpns {
                area.unmap_present_one_deferred(&mut self.page_table, vpn, &mut batch);
            }
            cursor = overlap_end;
        }

        if !batch.is_empty() {
            // The current hart may have cached the translations that were
            // just cleared. Remote harts are handled by DeferredUserReclaim
            // after the process lock is released.
            self.finish_deferred_page_table_edit();
        }
        Ok(batch)
    }

    /// 为 file-backed 缺页生成锁外慢路径所需的最小计划。
    pub fn prepare_file_page_fault(
        &self,
        fault_va: VirtAddr,
        access: PageFaultAccess,
    ) -> FilePageFaultPrepare {
        let vpn = fault_va.floor();
        if let Some(pte) = self.page_table.translate(vpn) {
            let vma_allows = self
                .find_vma_containing(vpn)
                .is_some_and(|area| area.is_user_accessible() && area.allows_fault_access(access));
            if vma_allows && Self::pte_allows_user_access(pte, access) {
                // The PTE may have been installed by another hart after the
                // trap dispatcher performed its first resident-PTE check but
                // before it reached this file-fault path.  Treat that state as
                // a resolved stale translation instead of returning a miss
                // that the trap layer would turn into SIGSEGV.
                warn!(
                    "[tlb] retry file fault with present user PTE: hart={} vpn={:#x} \
                     access={:?} pte_bits={:#x} ppn={:#x} flags={:?}",
                    crate::hal::hartid(),
                    vpn.0,
                    access,
                    pte.bits,
                    pte.ppn().0,
                    pte.flags(),
                );
                self.flush_local_tlb_page_asid(fault_va.0);
                return FilePageFaultPrepare::Resolved;
            }
            warn!(
                "[mmap] file fault sees incompatible PTE: hart={} vpn={:#x} access={:?} \
                 pte_bits={:#x} ppn={:#x} flags={:?} vma_allows={}",
                crate::hal::hartid(),
                vpn.0,
                access,
                pte.bits,
                pte.ppn().0,
                pte.flags(),
                vma_allows,
            );
            return FilePageFaultPrepare::NotHandled;
        }
        let Some(area) = self.find_vma_containing(vpn) else {
            return FilePageFaultPrepare::NotHandled;
        };
        if !area.is_user_accessible() || !area.allows_fault_access(access) {
            return FilePageFaultPrepare::NotHandled;
        }
        let Some(file) = area.file.as_ref() else {
            return FilePageFaultPrepare::NotHandled;
        };
        let Some(page_idx) = area.file_page_index(vpn) else {
            return FilePageFaultPrepare::NotHandled;
        };
        let available_pages = area.end_vpn().0.saturating_sub(vpn.0);
        let read_ahead_pages = if matches!(access, PageFaultAccess::Read | PageFaultAccess::Exec) {
            file.fault_read_window(page_idx, access, available_pages)
        } else {
            1
        };
        let plan = FilePageFaultPlan {
            vpn,
            vma_start: area.start_vpn(),
            vma_end: area.end_vpn(),
            map_perm: area.map_perm,
            file: Arc::clone(&file.file),
            page_idx,
            pgoff: file.pgoff,
            shared: file.shared,
            access,
            read_ahead_pages,
        };
        debug!(
            "[mmap] prepared lazy fault plan: va={:#x} vpn={:#x} page_idx={} access={:?} shared={} path={:?}",
            usize::from(fault_va),
            plan.vpn.0,
            plan.page_idx,
            access,
            plan.shared,
            plan.file.path()
        );
        FilePageFaultPrepare::Pending(plan)
    }

    /// 检查某个缺页计划在慢路径返回后是否仍然与当前地址空间匹配。
    pub fn can_commit_file_page_fault(&self, plan: &FilePageFaultPlan) -> bool {
        if self.page_table.translate(plan.vpn).is_some() {
            return true;
        }
        let Some(area) = self.find_vma_containing(plan.vpn) else {
            return false;
        };
        let Some(file) = area.file.as_ref() else {
            return false;
        };
        area.start_vpn() == plan.vma_start
            && area.end_vpn() == plan.vma_end
            && area.map_perm == plan.map_perm
            && file.pgoff == plan.pgoff
            && file.shared == plan.shared
            && Arc::ptr_eq(&file.file, &plan.file)
            && area.file_page_index(plan.vpn) == Some(plan.page_idx)
    }

    /// 在命中的 `Framed` 区域内为单个页分配私有页框并建立映射。
    pub fn map_private_page_in_vma(&mut self, vpn: VirtPageNum) -> Result<(), MmError> {
        if self.page_table.translate(vpn).is_some() {
            return Ok(());
        }
        let Some(area) = self.find_vma_containing_mut(vpn) else {
            return Err(MmError::NoMapping);
        };
        let map_type = area.map_type;
        let map_perm = area.map_perm;
        let ppn: PhysPageNum = match map_type {
            MapType::Identical => PhysPageNum(vpn.0),
            MapType::Direct => {
                let va = usize::from(VirtAddr::from(vpn));
                PhysAddr::from(crate::platform::direct_map_virt_to_phys(va)).floor()
            }
            MapType::Framed => {
                let frame = frame_alloc_with_reclaim().ok_or(MmError::OutOfMemory)?;
                let page = Arc::new(PrivatePage::new(frame));
                let ppn = page.ppn();
                // debug!(
                //     "[mmap] allocate private frame for lazy fault: vpn={:#x} ppn={:#x}",
                //     vpn.0,
                //     ppn.0
                // );
                area.data_frames.insert(vpn, page);
                ppn
            }
        };
        let pte_flags = Self::map_perm_to_pte_flags(map_perm);
        self.page_table.map(vpn, ppn, pte_flags)?;
        Ok(())
    }

    /// 在用户匿名/heap/user stack VMA 内按需分配并映射一个私有页。
    pub fn handle_lazy_user_fault(
        &mut self,
        fault_va: VirtAddr,
        access: PageFaultAccess,
    ) -> Result<PageFaultHandled, MmError> {
        let vpn = fault_va.floor();
        if let Some(pte) = self.page_table.translate(vpn) {
            let vma_allows = self
                .find_vma_containing(vpn)
                .is_some_and(|area| area.is_user_accessible() && area.allows_fault_access(access));
            if vma_allows && Self::pte_allows_user_access(pte, access) {
                // Another hart may have installed or relaxed this PTE after
                // the current hart cached an invalid/restrictive translation.
                // The in-memory PTE is already sufficient, so invalidate the
                // local translation and retry the faulting instruction instead
                // of incorrectly delivering SIGSEGV.
                warn!(
                    "[tlb] retry stale present user PTE: hart={} vpn={:#x} access={:?} \
                     pte_bits={:#x} ppn={:#x} flags={:?}",
                    crate::hal::hartid(),
                    vpn.0,
                    access,
                    pte.bits,
                    pte.ppn().0,
                    pte.flags(),
                );
                self.flush_local_tlb_page_asid(fault_va.0);
                return Ok(PageFaultHandled::Handled);
            }
            return Ok(PageFaultHandled::NotHandled);
        }
        self.handle_missing_lazy_user_fault(fault_va, access)
    }

    /// Handle the common store-fault case without looking up an absent PTE
    /// once in the COW path and again in the lazy-anonymous path.
    pub(crate) fn handle_user_store_fault(
        &mut self,
        fault_va: VirtAddr,
    ) -> Result<(PageFaultHandled, Option<UserReleaseBatch>), MmError> {
        if let Some(pte) = self.page_table.translate(fault_va.floor()) {
            return self.handle_private_cow_fault_with_pte(fault_va, pte);
        }
        Ok((
            self.handle_missing_lazy_user_fault(fault_va, PageFaultAccess::Write)?,
            None,
        ))
    }

    /// Materialize a lazy anonymous page after the caller established that
    /// the faulting VPN has no present PTE.
    fn handle_missing_lazy_user_fault(
        &mut self,
        fault_va: VirtAddr,
        access: PageFaultAccess,
    ) -> Result<PageFaultHandled, MmError> {
        let vpn = fault_va.floor();
        let Some(area) = self.find_vma_containing(vpn) else {
            return Ok(PageFaultHandled::NotHandled);
        };
        if !area.supports_lazy_user_fault() || !area.allows_fault_access(access) {
            return Ok(PageFaultHandled::NotHandled);
        }
        self.map_private_page_in_vma(vpn)?;
        #[cfg(feature = "cosmos-meminfo")]
        {
            record_private_anonymous_first_fault(access);
        }
        self.flush_local_tlb_page_asid(fault_va.0);
        Ok(PageFaultHandled::Handled)
    }

    /// 把一个 page cache 页直接映射进用户页表，供 `MAP_SHARED` 使用。
    pub fn map_shared_file_page(
        &mut self,
        plan: &FilePageFaultPlan,
        page: Arc<SpinNoIrqLock<CachePage>>,
    ) -> Result<PageFaultHandled, MmError> {
        if self.page_table.translate(plan.vpn).is_some() {
            // Another hart installed the demand page after our lock-free
            // prepare phase.  The current hart may still cache the invalid
            // translation that caused this fault, so retry only after a local
            // ASID fence.
            self.flush_local_tlb_vpn_asid(plan.vpn);
            return Ok(PageFaultHandled::Handled);
        }
        if !self.can_commit_file_page_fault(plan) {
            return Ok(PageFaultHandled::NotHandled);
        }
        let mut pte_flags = Self::map_perm_to_pte_flags(plan.map_perm);
        if plan.shared
            && plan.map_perm.contains(MapPermission::W)
            && plan.access != PageFaultAccess::Write
        {
            pte_flags.remove(PTEFlags::W);
        }
        if plan.shared
            && plan.map_perm.contains(MapPermission::W)
            && plan.access == PageFaultAccess::Write
        {
            mark_cached_page_dirty(&page);
        }
        let ppn = retain_mapped_page(&page);
        let area = self
            .find_vma_containing_mut(plan.vpn)
            .expect("validated file fault VMA disappeared");
        if let Some(old_page) = area.direct_cache_pages.insert(plan.vpn, Arc::clone(&page)) {
            release_mapped_page(&old_page);
        }
        self.page_table.map(plan.vpn, ppn, pte_flags)?;
        self.flush_local_tlb_vpn_asid(plan.vpn);
        debug!(
            "[mmap] committed MAP_SHARED fault: vpn={:#x} page_idx={} ppn={:#x} writable={} path={:?}",
            plan.vpn.0,
            plan.page_idx,
            ppn.0,
            pte_flags.contains(PTEFlags::W),
            plan.file.path()
        );
        Ok(PageFaultHandled::Handled)
    }

    /// Install a bounded set of already-uptodate page-cache pages around a
    /// read/execute fault.  The caller has performed the cache lookup without
    /// holding the process lock; revalidate the original VMA before changing
    /// any PTE, then populate all neighbours under one lock and one TLB flush.
    pub fn map_file_cache_pages_around(
        &mut self,
        plan: &FilePageFaultPlan,
        pages: Vec<(VirtPageNum, Arc<SpinNoIrqLock<CachePage>>)>,
    ) -> Result<PageFaultHandled, MmError> {
        if !matches!(plan.access, PageFaultAccess::Read | PageFaultAccess::Exec) {
            return Ok(PageFaultHandled::NotHandled);
        }
        if !self.can_commit_file_page_fault(plan) {
            return Ok(PageFaultHandled::NotHandled);
        }

        // Classify each candidate once while the process lock excludes other
        // page-table writers.  The old path translated every page again in
        // the commit loop after this preflight.
        let mut mapped_fault_page = false;
        let mut pending_pages = Vec::with_capacity(pages.len());
        for (vpn, page) in pages {
            if vpn < plan.vma_start || vpn >= plan.vma_end {
                continue;
            }
            if self.page_table.translate(vpn).is_some() {
                mapped_fault_page |= vpn == plan.vpn;
            } else {
                pending_pages.push((vpn, page));
            }
        }

        // Allocate every required intermediate page-table page before taking
        // cache-page mapping references.  One leaf table covers many adjacent
        // PTEs, so preflight it once rather than once per candidate page.
        let leaf_vpn_span = 1usize << crate::hal::page_table_index_bits();
        let mut leaf_preflights = 0usize;
        let mut previous_leaf_base = None;
        for (vpn, _) in &pending_pages {
            let leaf_base = vpn.0 & !(leaf_vpn_span - 1);
            if previous_leaf_base != Some(leaf_base) {
                self.page_table.ensure_leaf(*vpn)?;
                leaf_preflights += 1;
                previous_leaf_base = Some(leaf_base);
            }
        }

        let mut pte_flags = Self::map_perm_to_pte_flags(plan.map_perm);
        if plan.shared {
            if plan.map_perm.contains(MapPermission::W) {
                pte_flags.remove(PTEFlags::W);
            }
        } else {
            pte_flags.remove(PTEFlags::W);
            pte_flags.remove(PTEFlags::D);
        }

        let mut mapped_any = false;
        let mut mapped_pages = 0usize;
        let mut mapped_start = usize::MAX;
        let mut mapped_end = 0usize;
        if !pending_pages.is_empty() {
            let area = self
                .vmas
                .get_mut(&plan.vma_start)
                .expect("validated fault-around VMA disappeared");
            for (vpn, page) in pending_pages {
                let ppn = retain_mapped_page(&page);
                self.page_table
                    .map_preallocated_leaf(vpn, ppn, pte_flags)?;
                if let Some(old_page) = area.direct_cache_pages.insert(vpn, Arc::clone(&page)) {
                    release_mapped_page(&old_page);
                }
                mapped_any = true;
                mapped_pages += 1;
                mapped_fault_page |= vpn == plan.vpn;
                let page_start = VirtAddr::from(vpn).0;
                mapped_start = mapped_start.min(page_start);
                mapped_end = mapped_end.max(page_start + PAGE_SIZE);
            }
        }
        let flush_pages = if mapped_any {
            (mapped_end - mapped_start) / PAGE_SIZE
        } else {
            0
        };
        record_fault_around_commit(mapped_pages, leaf_preflights, flush_pages);
        if mapped_any {
            self.flush_local_tlb_range_asid(mapped_start, mapped_end);
        } else if mapped_fault_page {
            // The demand page can have been installed concurrently after the
            // prepare phase even when this fault-around batch added no page.
            self.flush_local_tlb_vpn_asid(plan.vpn);
        }
        Ok(if mapped_fault_page {
            PageFaultHandled::Handled
        } else {
            PageFaultHandled::NotHandled
        })
    }

    /// Snapshot MAP_SHARED file ranges and mark resident writable pages dirty.
    ///
    /// This phase is safe while the process address-space lock is held. The
    /// returned plan must be executed only after that lock has been released,
    /// because page-cache writeback may wait for block-device I/O.
    pub(crate) fn prepare_msync_range(
        &self,
        start_va: VirtAddr,
        end_va: VirtAddr,
    ) -> FileMappingSyncPlan {
        let start_vpn = start_va.floor();
        let end_vpn = end_va.ceil();
        if start_vpn >= end_vpn {
            return FileMappingSyncPlan { ranges: Vec::new() };
        }

        let mut ranges = Vec::new();
        for area in self.vmas.values() {
            let Some(file) = area.file.as_ref() else {
                continue;
            };
            if !file.shared {
                continue;
            }
            let overlap_start = area.start_vpn().max(start_vpn);
            let overlap_end = area.end_vpn().min(end_vpn);
            if overlap_start >= overlap_end {
                continue;
            }
            let Some(inode) = file.file.backing_inode() else {
                continue;
            };
            let start_idx = overlap_start.0 - area.start_vpn().0;
            let page_count = overlap_end.0 - overlap_start.0;
            let file_offset = (file.pgoff + start_idx) * PAGE_SIZE;
            let byte_len = page_count * PAGE_SIZE;

            // A writable MAP_SHARED page normally becomes dirty on its first
            // write-protection fault.  A file writer can, however, receive a
            // writable PTE through a prior read/population fault and then
            // modify it without another trap.  At teardown there is no later
            // fault to repair the missed notification.  All pages that are
            // actually present in a writable shared mapping are therefore
            // conservatively flushed here.  Pages that were never faulted in
            // are absent and are not touched.
            if file.shared && area.map_perm.contains(MapPermission::W) {
                for page in area
                    .direct_cache_pages
                    .range(overlap_start..overlap_end)
                    .map(|(_, page)| page)
                {
                    mark_cached_page_dirty(page);
                }
            }
            ranges.push(FileMappingSyncRange {
                inode,
                file_offset,
                byte_len,
            });
        }

        FileMappingSyncPlan { ranges }
    }

    /// 将 file-backed `MAP_PRIVATE` 的缓存页以只读方式直接接入页表。
    fn map_private_file_cache_page(
        &mut self,
        plan: &FilePageFaultPlan,
        page: Arc<SpinNoIrqLock<CachePage>>,
    ) -> Result<PageFaultHandled, MmError> {
        if self.page_table.translate(plan.vpn).is_some() {
            return Ok(PageFaultHandled::Handled);
        }
        if !self.can_commit_file_page_fault(plan) {
            return Ok(PageFaultHandled::NotHandled);
        }
        let mut pte_flags = Self::map_perm_to_pte_flags(plan.map_perm);
        pte_flags.remove(PTEFlags::W);
        pte_flags.remove(PTEFlags::D);
        let ppn = retain_mapped_page(&page);
        let area = self
            .find_vma_containing_mut(plan.vpn)
            .expect("validated private file fault VMA disappeared");
        if let Some(old_page) = area.direct_cache_pages.insert(plan.vpn, Arc::clone(&page)) {
            release_mapped_page(&old_page);
        }
        self.page_table.map(plan.vpn, ppn, pte_flags)?;
        self.flush_local_tlb_vpn_asid(plan.vpn);
        trace!(
            "[cow] install MAP_PRIVATE readonly cache page: vpn={:#x} page_idx={} ppn={:#x} access={:?} path={:?}",
            plan.vpn.0,
            plan.page_idx,
            ppn.0,
            plan.access,
            plan.file.path()
        );
        Ok(PageFaultHandled::Handled)
    }

    /// 处理共享可写页的首次写入通知缺页。
    pub fn handle_shared_write_fault(&mut self, fault_va: VirtAddr) -> bool {
        let vpn = fault_va.floor();
        let Some(pte) = self.page_table.translate(vpn) else {
            return false;
        };
        if pte.writable() {
            return false;
        }
        let (page, path) = {
            let Some(area) = self.find_vma_containing(vpn) else {
                return false;
            };
            if !area.is_user_accessible() || !area.allows_fault_access(PageFaultAccess::Write) {
                return false;
            }
            let Some(file) = area.file.as_ref() else {
                return false;
            };
            if !file.shared {
                return false;
            }
            let Some(page) = area.direct_cache_pages.get(&vpn).cloned() else {
                return false;
            };
            (page, file.file.path())
        };
        let mut new_flags = pte.flags();
        new_flags.insert(PTEFlags::W);
        if !self.page_table.update_flags(vpn, new_flags) {
            return false;
        }
        // 首次写 fault 时立即把 page cache 页记脏，避免等待 teardown 才传播脏状态。
        mark_cached_page_dirty(&page);
        self.flush_local_tlb_vpn_asid(vpn);
        debug!(
            "[mmap] shared write-notify fault: vpn={:#x} ppn={:#x} path={:?}",
            vpn.0,
            pte.ppn().0,
            path
        );
        true
    }

    /// 处理私有页的写时复制缺页。
    pub(crate) fn handle_private_cow_fault(
        &mut self,
        fault_va: VirtAddr,
    ) -> Result<(PageFaultHandled, Option<UserReleaseBatch>), MmError> {
        let vpn = fault_va.floor();
        let Some(pte) = self.page_table.translate(vpn) else {
            return Ok((PageFaultHandled::NotHandled, None));
        };
        self.handle_private_cow_fault_with_pte(fault_va, pte)
    }

    /// Continue private-COW handling with the caller's already translated PTE.
    fn handle_private_cow_fault_with_pte(
        &mut self,
        fault_va: VirtAddr,
        pte: PageTableEntry,
    ) -> Result<(PageFaultHandled, Option<UserReleaseBatch>), MmError> {
        let mut batch = UserReleaseBatch::new();
        let vpn = fault_va.floor();
        if pte.writable() {
            // 可能是其他 hart 已经把该页从 COW 只读状态放宽为可写，
            // 当前 hart 仍命中了陈旧的只读 TLB。刷新本地后让用户态重试。
            self.flush_local_tlb_vpn_asid(vpn);
            return Ok((PageFaultHandled::Handled, Some(batch)));
        }
        let file_private_cache_page = {
            let Some(area) = self.find_vma_containing(vpn) else {
                return Ok((PageFaultHandled::NotHandled, None));
            };
            if !area.supports_private_page_sharing()
                || !area.allows_fault_access(PageFaultAccess::Write)
            {
                return Ok((PageFaultHandled::NotHandled, None));
            }
            match area.file.as_ref() {
                Some(file) if !file.shared => area.direct_cache_pages.get(&vpn).cloned(),
                _ => None,
            }
        };
        if let Some(cache_page) = file_private_cache_page {
            let path = self
                .find_vma_containing(vpn)
                .and_then(|area| area.file.as_ref().and_then(|file| file.file.path()));
            let new_page = Arc::new(PrivatePage::new(
                frame_alloc_with_reclaim().ok_or(MmError::OutOfMemory)?,
            ));
            new_page
                .ppn()
                .get_bytes_array()
                .copy_from_slice(cache_page.lock().ppn().get_bytes_array());
            let mut writable_flags = pte.flags();
            writable_flags.insert(PTEFlags::W);
            writable_flags.remove(PTEFlags::D);
            let Some(area) = self.find_vma_containing_mut(vpn) else {
                return Ok((PageFaultHandled::NotHandled, None));
            };
            if let Some(old_page) = area.direct_cache_pages.remove(&vpn) {
                batch.push_direct_cache(old_page);
            }
            area.data_frames.insert(vpn, Arc::clone(&new_page));
            if !self.page_table.replace(vpn, new_page.ppn(), writable_flags) {
                return Ok((PageFaultHandled::NotHandled, None));
            }
            self.flush_local_tlb_vpn_asid(vpn);
            trace!(
                "[cow] materialize MAP_PRIVATE page on write fault: vpn={:#x} cache_ppn={:#x} new_ppn={:#x} path={:?}",
                vpn.0,
                cache_page.lock().ppn().0,
                new_page.ppn().0,
                path
            );
            return Ok((PageFaultHandled::Handled, Some(batch)));
        }
        let (page, path) = {
            let Some(area) = self.find_vma_containing(vpn) else {
                return Ok((PageFaultHandled::NotHandled, None));
            };
            if !area.supports_private_page_sharing()
                || !area.allows_fault_access(PageFaultAccess::Write)
            {
                return Ok((PageFaultHandled::NotHandled, None));
            }
            let Some(page) = area.data_frames.get(&vpn).cloned() else {
                return Ok((PageFaultHandled::NotHandled, None));
            };
            if !page.is_cow() {
                return Ok((PageFaultHandled::NotHandled, None));
            }
            (page, area.file.as_ref().and_then(|file| file.file.path()))
        };
        let mut writable_flags = pte.flags();
        writable_flags.insert(PTEFlags::W);
        writable_flags.remove(PTEFlags::D);
        // debug!(
        //     "[cow] private write fault hit: vpn={:#x} ppn={:#x} refcnt={} cow={} path={:?}",
        //     vpn.0,
        //     page.ppn().0,
        //     Arc::strong_count(&page),
        //     page.is_cow(),
        //     path
        // );

        // TODO: 这里暂时用 `Arc::strong_count` 近似判断是否仍有其他地址空间共享该页；
        // 后续若引入更复杂的页生命周期管理，需要改成显式引用计数或反向映射。
        // `page` 此时至少被当前 VMA 和局部变量各持有一次；若强引用数不超过 2，说明已经没有其他地址空间共享它。
        if Arc::strong_count(&page) <= 2 {
            page.set_cow(false);
            if !self.page_table.update_flags(vpn, writable_flags) {
                return Ok((PageFaultHandled::NotHandled, None));
            }
            self.flush_local_tlb_vpn_asid(vpn);
            trace!(
                "[cow] reuse exclusive private page: vpn={:#x} ppn={:#x} path={:?}",
                vpn.0,
                page.ppn().0,
                path
            );
            return Ok((PageFaultHandled::Handled, Some(batch)));
        }

        let new_page = Arc::new(PrivatePage::new(
            frame_alloc_with_reclaim().ok_or(MmError::OutOfMemory)?,
        ));
        new_page
            .ppn()
            .get_bytes_array()
            .copy_from_slice(page.ppn().get_bytes_array());
        let Some(area) = self.find_vma_containing_mut(vpn) else {
            return Ok((PageFaultHandled::NotHandled, None));
        };
        if let Some(old_page) = area.data_frames.insert(vpn, Arc::clone(&new_page)) {
            batch.push_private(old_page);
        }
        if !self.page_table.replace(vpn, new_page.ppn(), writable_flags) {
            return Ok((PageFaultHandled::NotHandled, None));
        }
        self.flush_local_tlb_vpn_asid(vpn);
        trace!(
            "[cow] copy private page on write fault: vpn={:#x} old_ppn={:#x} new_ppn={:#x} path={:?}",
            vpn.0,
            page.ppn().0,
            new_page.ppn().0,
            path
        );
        Ok((PageFaultHandled::Handled, Some(batch)))
    }

    /// 为 `MAP_PRIVATE` 缺页分配私有页框，并以 page cache 作为填充源。
    pub fn map_private_file_page(
        &mut self,
        plan: &FilePageFaultPlan,
        page: Arc<SpinNoIrqLock<CachePage>>,
    ) -> Result<PageFaultHandled, MmError> {
        if self.page_table.translate(plan.vpn).is_some() {
            // A concurrent fault resolved this page after prepare.  Discard
            // the current hart's stale invalid translation before retrying.
            self.flush_local_tlb_vpn_asid(plan.vpn);
            return Ok(PageFaultHandled::Handled);
        }
        if !self.can_commit_file_page_fault(plan) {
            return Ok(PageFaultHandled::NotHandled);
        }
        if plan.access != PageFaultAccess::Write {
            // A read/execute fault can share the page-cache frame even when
            // the VMA is writable.  Keep the PTE read-only and let the first
            // store take the existing file-cache COW path.  This avoids
            // allocating and copying writable ELF/data pages which are never
            // modified by the process.
            return self.map_private_file_cache_page(plan, page);
        }
        self.map_private_page_in_vma(plan.vpn)?;
        let dst_ppn = self.page_table.translate(plan.vpn).unwrap().ppn();
        let dst = dst_ppn.get_bytes_array();
        let page_guard = page.lock();
        let src = page_guard.ppn().get_bytes_array();
        dst.copy_from_slice(src);
        self.flush_local_tlb_vpn_asid(plan.vpn);
        trace!(
            "[cow] materialize MAP_PRIVATE page on first write fault: vpn={:#x} page_idx={} dst_ppn={:#x} path={:?}",
            plan.vpn.0,
            plan.page_idx,
            dst_ppn.0,
            plan.file.path()
        );
        Ok(PageFaultHandled::Handled)
    }

    /// Change permissions of a range in the address space.
    /// Returns true on success. The operation is performed in two phases:
    /// 1) verify the whole range is mapped and user-accessible;
    /// 2) perform VMA splits (if necessary) and update PTE flags.
    pub fn mprotect_range(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) -> bool {
        debug!(
            "mprotect_range: [{:#x}, {:#x}) with permission {:?}",
            start_va.0, end_va.0, permission
        );

        let start_vpn = start_va.floor();
        let end_vpn = end_va.ceil();

        let mut affected = Vec::new();
        let mut cursor = start_vpn;
        while cursor < end_vpn {
            let Some((area_key, area)) = self.vmas.range(..=cursor).next_back() else {
                return false;
            };
            if !area.contains_vpn(cursor) || !area.is_user_accessible() {
                return false;
            }
            let overlap_end = if area.end_vpn() < end_vpn {
                area.end_vpn()
            } else {
                end_vpn
            };
            for vpn in VPNRange::new(cursor, overlap_end) {
                if let Some(pte) = self.page_table.translate(vpn) {
                    if !pte.flags().contains(PTEFlags::U) {
                        return false;
                    }
                }
            }
            affected.push((*area_key, cursor, overlap_end));
            cursor = overlap_end;
        }

        let pte_flags = Self::map_perm_to_pte_flags(permission);
        let mut changed_keys = Vec::new();
        for (area_key, overlap_start, overlap_end) in affected {
            let Some(mut area) = self.vmas.remove(&area_key) else {
                return false;
            };
            let area_start = area.start_vpn();
            let mut to_insert = Vec::new();

            if area_start < overlap_start {
                let Some(right) = area.split_off(overlap_start) else {
                    return false;
                };
                to_insert.push(area);
                area = right;
            }

            if overlap_end < area.end_vpn() {
                let Some(right) = area.split_off(overlap_end) else {
                    return false;
                };
                to_insert.push(right);
            }

            for vpn in VPNRange::new(overlap_start, overlap_end) {
                if self.page_table.translate(vpn).is_some() {
                    self.page_table.update_flags(vpn, pte_flags);
                }
            }
            area.map_perm = permission;
            changed_keys.push(area.start_vpn());
            to_insert.push(area);

            for area in to_insert {
                self.insert_vma_unchecked(area);
            }
        }
        for key in changed_keys {
            self.merge_vma_around(key);
        }
        self.flush_local_tlb_range_asid(start_va.0, end_va.0);
        true
    }
}

/// 用于描述一段虚拟地址区间在地址空间中的语义角色。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VmaKind {
    /// 内核地址空间中的固定映射区域。
    Kernel,
    /// 来自 ELF 装载段的用户态区域。
    Elf,
    /// 预留给 brk/sbrk 管理的进程堆区域。
    Heap,
    /// 某个线程的用户栈区域。
    UserStack {
        /// 用户栈所属线程编号。
        tid: usize,
    },
    /// 某个线程的 Trap 上下文页。
    TrapContext {
        /// Trap 上下文所属线程编号。
        tid: usize,
    },
    /// 普通匿名映射区域。
    Anonymous,
    /// `MAP_SHARED | MAP_ANONYMOUS` 映射区域。
    SharedAnonymous,
    /// 文件映射区域。
    File,
    /// 用户态 vDSO/trampoline 区域。
    Vdso,
}

/// 文件映射区域附带的底层对象信息。
pub struct FileVma {
    /// 建立映射时引用的打开文件描述。
    pub file: Arc<FileDescription>,
    /// 文件页偏移，单位为页。
    pub pgoff: usize,
    /// 是否为 `MAP_SHARED` 映射。
    pub shared: bool,
    /// 当前 VMA 自己的 fault readahead 流状态。VMA 分裂和 fork 时复制
    /// 状态快照，与同一文件上的其他独立 mmap 流互不干扰。
    fault_read_ahead: Arc<SpinNoIrqLock<FileVmaReadAheadState>>,
}

impl Clone for FileVma {
    fn clone(&self) -> Self {
        let fault_read_ahead = *self.fault_read_ahead.lock();
        Self {
            file: Arc::clone(&self.file),
            pgoff: self.pgoff,
            shared: self.shared,
            fault_read_ahead: Arc::new(SpinNoIrqLock::new(fault_read_ahead)),
        }
    }
}

const FILE_FAULT_READAHEAD_PAGES: usize = 32;
const FILE_RANDOM_EXEC_READAHEAD_PAGES: usize = 16;

#[derive(Clone, Copy, Debug, Default)]
struct FileVmaReadAheadState {
    previous_fault_page: Option<u64>,
    window_end: u64,
}

impl FileVma {
    fn fault_read_window(
        &self,
        page_idx: u64,
        access: PageFaultAccess,
        available_pages: usize,
    ) -> usize {
        let mut state = self.fault_read_ahead.lock();
        let begins_mapping = page_idx == self.pgoff as u64;
        let adjacent = state
            .previous_fault_page
            .is_some_and(|previous| page_idx == previous.saturating_add(1));
        let extends_window = page_idx == state.window_end;
        let sequential = begins_mapping || adjacent || extends_window;
        state.previous_fault_page = Some(page_idx);
        let desired_pages = if sequential {
            FILE_FAULT_READAHEAD_PAGES
        } else if access == PageFaultAccess::Exec {
            FILE_RANDOM_EXEC_READAHEAD_PAGES
        } else {
            return 1;
        };

        let count = desired_pages.min(available_pages).max(1);
        state.window_end = page_idx.saturating_add(count as u64);
        count
    }
}

/// 页错误对应的访问类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageFaultAccess {
    /// 读缺页。
    Read,
    /// 写缺页。
    Write,
    /// 指令取值缺页。
    Exec,
}

/// Result of checking a file-backed page fault while the process MM lock is held.
pub enum FilePageFaultPrepare {
    /// A compatible resident PTE already resolves the fault after a local flush.
    Resolved,
    /// The page is absent and must be loaded through the file/page-cache slow path.
    Pending(FilePageFaultPlan),
    /// The address is not a compatible file-backed user mapping.
    NotHandled,
}

/// file-backed 缺页在锁外执行慢路径时携带的最小计划。
#[derive(Clone)]
pub struct FilePageFaultPlan {
    /// 发生缺页的虚拟页号。
    pub vpn: VirtPageNum,
    /// 发生缺页时命中的 VMA 起始页号。
    pub vma_start: VirtPageNum,
    /// 发生缺页时命中的 VMA 结束页号。
    pub vma_end: VirtPageNum,
    /// 缺页区域的访问权限。
    pub map_perm: MapPermission,
    /// 建立映射时持有的打开文件描述。
    pub file: Arc<FileDescription>,
    /// 缺页对应的文件页号。
    pub page_idx: u64,
    /// 建立映射时的文件页偏移。
    pub pgoff: usize,
    /// 是否为 `MAP_SHARED`。
    pub shared: bool,
    /// 触发本次缺页的访问类型。
    pub access: PageFaultAccess,
    /// 本次 fault 应同步装入的向前页窗口。
    pub read_ahead_pages: usize,
}

/// 一张可在多个地址空间之间共享的私有页。
pub struct PrivatePage {
    /// 实际承载数据的物理页框。
    frame: FrameTracker,
    /// 当前页是否处于写时复制保护状态。
    cow: AtomicBool,
}

impl PrivatePage {
    /// 基于新分配的页框创建一张私有页。
    pub fn new(frame: FrameTracker) -> Self {
        Self {
            frame,
            cow: AtomicBool::new(false),
        }
    }

    /// 返回当前私有页对应的物理页号。
    pub fn ppn(&self) -> PhysPageNum {
        self.frame.ppn
    }

    /// 设置当前页是否启用 COW。
    pub fn set_cow(&self, cow: bool) {
        self.cow.store(cow, Ordering::Release);
    }

    /// 判断当前页是否正处于 COW 保护状态。
    pub fn is_cow(&self) -> bool {
        self.cow.load(Ordering::Acquire)
    }

    /// 消费当前私有页并返回底层页框。
    pub fn into_frame(self) -> FrameTracker {
        self.frame
    }
}

/// 一段带有权限、来源和页框信息的虚拟内存区域描述。
pub struct Vma {
    /// 覆盖的虚拟页号半开区间。
    pub vpn_range: VPNRange,
    /// 对于 framed 映射，记录每个虚拟页对应的物理页框。
    pub data_frames: BTreeMap<VirtPageNum, Arc<PrivatePage>>,
    /// 该区域采用的映射方式。
    pub map_type: MapType,
    /// 该区域在页表中的访问权限。
    pub map_perm: MapPermission,
    /// 该区域在地址空间中的用途标签。
    pub kind: VmaKind,
    /// 文件映射附带的底层对象信息；匿名区域为 `None`。
    pub file: Option<FileVma>,
    /// 匿名映射是否带有 `MAP_SHARED` 语义。
    pub shared_anon: bool,
    /// 当前直接映射到用户页表的 page cache 页。
    /// `MAP_SHARED` 与首次只读接入的 `MAP_PRIVATE` 都会使用这里记录映射关系。
    pub direct_cache_pages: BTreeMap<VirtPageNum, Arc<SpinNoIrqLock<CachePage>>>,
}

impl Vma {
    /// 根据给定区间、映射方式、权限与语义类型构造一段新的虚拟内存区域。
    pub fn new(
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_type: MapType,
        map_perm: MapPermission,
        kind: VmaKind,
    ) -> Self {
        let start_vpn: VirtPageNum = start_va.floor();
        let end_vpn: VirtPageNum = end_va.ceil();
        Self {
            vpn_range: VPNRange::new(start_vpn, end_vpn),
            data_frames: BTreeMap::new(),
            map_type,
            map_perm,
            kind,
            file: None,
            shared_anon: false,
            direct_cache_pages: BTreeMap::new(),
        }
    }
    /// 为 ELF 装载段创建一段带有用户态访问语义的区域描述。
    pub fn new_elf(start_va: VirtAddr, end_va: VirtAddr, map_perm: MapPermission) -> Self {
        Self::new(start_va, end_va, MapType::Framed, map_perm, VmaKind::Elf)
    }
    /// 为后续通过 brk/sbrk 管理的数据段扩展区预留专用区域类型。
    pub fn new_heap(start_va: VirtAddr, end_va: VirtAddr, map_perm: MapPermission) -> Self {
        Self::new(start_va, end_va, MapType::Framed, map_perm, VmaKind::Heap)
    }
    /// 为某个线程生成用户栈对应的区域描述，并附带线程编号。
    pub fn new_user_stack(start_va: VirtAddr, end_va: VirtAddr, tid: usize) -> Self {
        Self::new(
            start_va,
            end_va,
            MapType::Framed,
            MapPermission::R | MapPermission::W | MapPermission::U,
            VmaKind::UserStack { tid },
        )
    }
    /// 为某个线程生成 Trap 上下文页对应的区域描述。
    pub fn new_trap_context(start_va: VirtAddr, end_va: VirtAddr, tid: usize) -> Self {
        let map_perm =
            MapPermission::from_bits_truncate(crate::hal::trap_context_flags().bits() as u8);
        Self::new(
            start_va,
            end_va,
            MapType::Framed,
            map_perm,
            VmaKind::TrapContext { tid },
        )
    }
    /// 创建用户态 vDSO/trampoline 区域描述。
    pub fn new_vdso(start_va: VirtAddr, end_va: VirtAddr) -> Self {
        Self::new(
            start_va,
            end_va,
            MapType::Framed,
            MapPermission::R | MapPermission::X | MapPermission::U,
            VmaKind::Vdso,
        )
    }
    /// 为匿名映射场景生成一段普通用户区域。
    pub fn new_anonymous(
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_perm: MapPermission,
        shared: bool,
    ) -> Self {
        let mut vma = Self::new(
            start_va,
            end_va,
            MapType::Framed,
            map_perm,
            VmaKind::Anonymous,
        );
        vma.shared_anon = shared;
        vma
    }
    /// 为 `MAP_SHARED | MAP_ANONYMOUS` 创建一段可跨 fork 共享的匿名区域。
    pub fn new_shared_anonymous(
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_perm: MapPermission,
    ) -> Self {
        let mut vma = Self::new(
            start_va,
            end_va,
            MapType::Framed,
            map_perm,
            VmaKind::SharedAnonymous,
        );
        vma.shared_anon = true;
        vma
    }
    /// 为文件映射场景保留文件偏移等来源信息。
    pub fn new_file(
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_perm: MapPermission,
        file: Arc<FileDescription>,
        pgoff: usize,
        shared: bool,
    ) -> Self {
        let mut vma = Self::new(start_va, end_va, MapType::Framed, map_perm, VmaKind::File);
        vma.file = Some(FileVma {
            file,
            pgoff,
            shared,
            fault_read_ahead: Arc::new(SpinNoIrqLock::new(FileVmaReadAheadState::default())),
        });
        vma
    }
    /// 复制一份仅包含区间属性的区域元数据，不携带已有物理页分配结果。
    pub fn clone_metadata(&self) -> Self {
        Self {
            vpn_range: VPNRange::new(self.start_vpn(), self.end_vpn()),
            data_frames: BTreeMap::new(),
            map_type: self.map_type,
            map_perm: self.map_perm,
            kind: self.kind.clone(),
            file: self.file.clone(),
            shared_anon: self.shared_anon,
            direct_cache_pages: BTreeMap::new(),
        }
    }

    /// Clone a VMA for a shared-MM vfork view, retaining the resident page
    /// references without changing page-cache mapping counters.  The view is
    /// temporary and its metadata is adopted by the parent when it finishes.
    fn clone_shared_view(&self) -> Self {
        Self {
            vpn_range: VPNRange::new(self.start_vpn(), self.end_vpn()),
            data_frames: self
                .data_frames
                .iter()
                .map(|(&vpn, page)| (vpn, Arc::clone(page)))
                .collect(),
            map_type: self.map_type,
            map_perm: self.map_perm,
            kind: self.kind.clone(),
            file: self.file.clone(),
            shared_anon: self.shared_anon,
            direct_cache_pages: self
                .direct_cache_pages
                .iter()
                .map(|(&vpn, page)| (vpn, Arc::clone(page)))
                .collect(),
        }
    }
    /// 返回该区域覆盖的起始虚拟页号，便于统一做区间级操作。
    pub fn start_vpn(&self) -> VirtPageNum {
        self.vpn_range.get_start()
    }
    /// 返回该区域末尾的虚拟页号上界，用于配合半开区间判断。
    pub fn end_vpn(&self) -> VirtPageNum {
        self.vpn_range.get_end()
    }
    /// 判断某个虚拟页是否落在当前区域内部。
    pub fn contains_vpn(&self, vpn: VirtPageNum) -> bool {
        self.start_vpn() <= vpn && vpn < self.end_vpn()
    }
    /// 判断当前区域是否被标记为进程堆，便于后续 brk 语义接入。
    pub fn is_heap(&self) -> bool {
        matches!(self.kind, VmaKind::Heap)
    }
    /// 判断当前区域是否表示某个线程的用户栈。
    pub fn is_user_stack(&self) -> bool {
        matches!(self.kind, VmaKind::UserStack { .. })
    }
    /// 判断当前区域是否表示某个线程的 Trap 上下文页。
    pub fn is_trap_context(&self) -> bool {
        matches!(self.kind, VmaKind::TrapContext { .. })
    }
    /// Whether this VMA is a relocatable mmap-style user mapping.
    pub fn supports_mremap(&self) -> bool {
        matches!(
            &self.kind,
            VmaKind::Anonymous | VmaKind::SharedAnonymous | VmaKind::File
        )
    }
    /// 返回区域覆盖的字节长度。
    pub fn byte_len(&self) -> usize {
        self.end_vpn().0.saturating_sub(self.start_vpn().0) * PAGE_SIZE
    }
    /// 依据权限位判断该区域是否允许用户态直接访问。
    pub fn is_user_accessible(&self) -> bool {
        self.map_perm.contains(MapPermission::U)
    }
    /// 判断两段相邻区域在元数据层面是否具备合并条件。
    pub fn can_merge_with(&self, other: &Self) -> bool {
        self.end_vpn() == other.start_vpn()
            && self.map_type == other.map_type
            && self.map_perm == other.map_perm
            && self.kind == other.kind
            && self.shared_anon == other.shared_anon
            && self.file.is_none()
            && other.file.is_none()
    }
    /// 将一段可合并的相邻区域吸收到当前区域中，并保留已有映射页信息。
    pub fn absorb(&mut self, other: Self) {
        debug_assert!(self.can_merge_with(&other));
        self.vpn_range = VPNRange::new(self.start_vpn(), other.end_vpn());
        self.data_frames.extend(other.data_frames);
    }
    /// 判断当前区域中的私有页是否适合在 `fork` 时共享。
    pub fn supports_private_page_sharing(&self) -> bool {
        if self.map_type != MapType::Framed {
            return false;
        }
        if matches!(self.kind, VmaKind::TrapContext { .. }) {
            return false;
        }
        if self.shared_anon {
            return false;
        }
        !matches!(self.file.as_ref(), Some(file) if file.shared)
    }
    /// 是否为 `MAP_SHARED | MAP_ANONYMOUS` 区域。
    pub fn is_shared_anonymous(&self) -> bool {
        matches!(self.kind, VmaKind::SharedAnonymous)
    }
    /// 判断指定虚拟页是否属于匿名帧映射区域，供当前匿名 unmap 逻辑复用。
    pub fn is_anonymous_framed_containing(&self, vpn: VirtPageNum) -> bool {
        self.map_type == MapType::Framed
            && matches!(self.kind, VmaKind::Anonymous)
            && self.contains_vpn(vpn)
    }
    /// 判断当前区域是否允许指定类型的缺页访问。
    pub fn allows_fault_access(&self, access: PageFaultAccess) -> bool {
        match access {
            PageFaultAccess::Read => self.map_perm.contains(MapPermission::R),
            PageFaultAccess::Write => self.map_perm.contains(MapPermission::W),
            PageFaultAccess::Exec => self.map_perm.contains(MapPermission::X),
        }
    }
    /// 判断当前区域是否适合通过用户态懒缺页来物化私有页。
    pub fn supports_lazy_user_fault(&self) -> bool {
        self.map_type == MapType::Framed
            && self.is_user_accessible()
            && !self.shared_anon
            && matches!(
                self.kind,
                VmaKind::Anonymous
                    | VmaKind::SharedAnonymous
                    | VmaKind::Heap
                    | VmaKind::UserStack { .. }
            )
    }
    /// 判断当前区域是否需要在建 VMA 时立即分配并建立页表映射。
    pub fn should_eager_map(&self) -> bool {
        self.file.is_none() && !self.supports_lazy_user_fault()
    }
    /// 计算某个虚拟页在底层文件中的页号。
    pub fn file_page_index(&self, vpn: VirtPageNum) -> Option<u64> {
        let file = self.file.as_ref()?;
        let delta = vpn.0.checked_sub(self.start_vpn().0)?;
        Some((file.pgoff + delta) as u64)
    }
    /// 从当前区域中按 `split_vpn` 处分裂出右半部分区域。
    pub fn split_off(&mut self, split_vpn: VirtPageNum) -> Option<Self> {
        if split_vpn <= self.start_vpn() || split_vpn >= self.end_vpn() {
            return None;
        }
        let old_end = self.end_vpn();
        let right_data_frames = self.data_frames.split_off(&split_vpn);
        let right_direct_cache_pages = self.direct_cache_pages.split_off(&split_vpn);
        let mut right_file = self.file.clone();
        if let Some(file) = right_file.as_mut() {
            file.pgoff += split_vpn.0 - self.start_vpn().0;
        }
        self.vpn_range = VPNRange::new(self.start_vpn(), split_vpn);
        Some(Self {
            vpn_range: VPNRange::new(split_vpn, old_end),
            data_frames: right_data_frames,
            map_type: self.map_type,
            map_perm: self.map_perm,
            kind: self.kind.clone(),
            file: right_file,
            shared_anon: self.shared_anon,
            direct_cache_pages: right_direct_cache_pages,
        })
    }
    /// 按当前实际映射状态拆除单页映射，并延迟释放旧页对象。
    pub(crate) fn unmap_present_one_deferred(
        &mut self,
        page_table: &mut PageTable,
        vpn: VirtPageNum,
        batch: &mut UserReleaseBatch,
    ) {
        self.unmap_present_one_deferred_inner(page_table, vpn, batch, true);
    }

    /// Remove a file mapping after truncate has committed.
    ///
    /// Bytes beyond the new EOF are intentionally discarded, so a dirty PTE
    /// must not re-dirty a cache page that `page_cache::truncate_mapping()` may
    /// already have removed from the mapping.
    pub(crate) fn unmap_present_one_deferred_after_truncate(
        &mut self,
        page_table: &mut PageTable,
        vpn: VirtPageNum,
        batch: &mut UserReleaseBatch,
    ) {
        self.unmap_present_one_deferred_inner(page_table, vpn, batch, false);
    }

    fn unmap_present_one_deferred_inner(
        &mut self,
        page_table: &mut PageTable,
        vpn: VirtPageNum,
        batch: &mut UserReleaseBatch,
        mark_shared_dirty: bool,
    ) {
        if let Some(page) = self.direct_cache_pages.remove(&vpn) {
            let shared_file_mapping = self.file.as_ref().map(|file| file.shared).unwrap_or(false);
            trace!(
                "[munmap] defer file cache mapping release: vpn={:#x} shared={}",
                vpn.0,
                shared_file_mapping
            );
            if let Some(old_pte) = page_table.clear(vpn) {
                if mark_shared_dirty && shared_file_mapping && old_pte.flags().contains(PTEFlags::D)
                {
                    mark_cached_page_dirty(&page);
                }
            }
            batch.push_direct_cache(page);
            return;
        }
        if self.map_type == MapType::Framed {
            if let Some(page) = self.data_frames.remove(&vpn) {
                batch.push_private(page);
            }
        }
        let _ = page_table.clear(vpn);
    }
    /// 按当前实际映射状态拆除单页映射，不保留旧页对象。
    pub(crate) fn unmap_present_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        if let Some(page) = self.direct_cache_pages.remove(&vpn) {
            let shared_file_mapping = self.file.as_ref().map(|file| file.shared).unwrap_or(false);
            if let Some(old_pte) = page_table.clear(vpn) {
                if shared_file_mapping && old_pte.flags().contains(PTEFlags::D) {
                    mark_cached_page_dirty(&page);
                }
            }
            release_mapped_page(&page);
            return;
        }
        if self.map_type == MapType::Framed {
            let _ = self.data_frames.remove(&vpn);
        }
        let _ = page_table.clear(vpn);
    }
    /// 依据当前区域实际映射状态拆除全部页表项，并延迟释放旧页对象。
    pub(crate) fn teardown_user_deferred(
        &mut self,
        page_table: &mut PageTable,
        batch: &mut UserReleaseBatch,
    ) {
        let shared_vpns: alloc::vec::Vec<_> = self.direct_cache_pages.keys().copied().collect();
        for vpn in shared_vpns {
            self.unmap_present_one_deferred(page_table, vpn, batch);
        }
        let framed_vpns: alloc::vec::Vec<_> = self.data_frames.keys().copied().collect();
        for vpn in framed_vpns {
            self.unmap_present_one_deferred(page_table, vpn, batch);
        }
        if matches!(self.map_type, MapType::Identical | MapType::Direct) {
            for vpn in self.vpn_range {
                let _ = page_table.clear(vpn);
            }
        }
    }
    /// 依据当前区域实际映射状态拆除全部页表项，并返回 framed 私有页对应的页框。
    ///
    /// 当前调用方只覆盖 kernel stack，因此这里要求每张私有页都具有独占所有权。
    /// TODO：若未来需要推广到更一般的 deferred reclaim，应补齐共享私有页与
    /// direct cache page 的处理分支。
    pub fn teardown_deferred(&mut self, page_table: &mut PageTable) -> Vec<FrameTracker> {
        let shared_vpns: alloc::vec::Vec<_> = self.direct_cache_pages.keys().copied().collect();
        for vpn in shared_vpns {
            if let Some(page) = self.direct_cache_pages.remove(&vpn) {
                let shared_file_mapping =
                    self.file.as_ref().map(|file| file.shared).unwrap_or(false);
                if let Some(old_pte) = page_table.clear(vpn) {
                    if shared_file_mapping && old_pte.flags().contains(PTEFlags::D) {
                        mark_cached_page_dirty(&page);
                    }
                }
                release_mapped_page(&page);
            }
        }
        let framed_vpns: alloc::vec::Vec<_> = self.data_frames.keys().copied().collect();
        let mut frames = Vec::with_capacity(framed_vpns.len());
        for vpn in framed_vpns {
            let Some(page) = self.data_frames.remove(&vpn) else {
                continue;
            };
            let _ = page_table.clear(vpn);
            let page = match Arc::try_unwrap(page) {
                Ok(page) => page,
                Err(_) => panic!("deferred framed reclaim requires exclusive page ownership"),
            };
            frames.push(page.into_frame());
        }
        if matches!(self.map_type, MapType::Identical | MapType::Direct) {
            for vpn in self.vpn_range {
                let _ = page_table.clear(vpn);
            }
        }
        frames
    }
    /// 为指定虚拟页建立单页映射，并在需要时分配新的物理页框。
    pub fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) -> Result<(), MmError> {
        let ppn: PhysPageNum;
        match self.map_type {
            MapType::Identical => {
                ppn = PhysPageNum(vpn.0);
            }
            MapType::Direct => {
                let va = usize::from(VirtAddr::from(vpn));
                ppn = PhysAddr::from(crate::platform::direct_map_virt_to_phys(va)).floor();
            }
            MapType::Framed => {
                let page = Arc::new(PrivatePage::new(
                    frame_alloc_with_reclaim().ok_or(MmError::OutOfMemory)?,
                ));
                ppn = page.ppn();
                self.data_frames.insert(vpn, page);
            }
        }
        let pte_flags = MemorySet::map_perm_to_pte_flags(self.map_perm);
        page_table.map(vpn, ppn, pte_flags)?;
        Ok(())
    }
    /// 为当前区域覆盖的全部虚拟页建立映射。
    pub fn map(&mut self, page_table: &mut PageTable) -> Result<(), MmError> {
        let start = self.vpn_range.get_start();
        let mut current = start;
        while current < self.vpn_range.get_end() {
            if let Err(err) = self.map_one(page_table, current) {
                for rollback_vpn in VPNRange::new(start, current) {
                    self.unmap_present_one(page_table, rollback_vpn);
                }
                return Err(err);
            }
            current.step();
        }
        Ok(())
    }
    /// 将当前区域收缩到新的上界，并把尾部页对象加入延迟释放批次。
    pub(crate) fn shrink_to_deferred(
        &mut self,
        page_table: &mut PageTable,
        new_end: VirtPageNum,
        batch: &mut UserReleaseBatch,
    ) {
        for vpn in VPNRange::new(new_end, self.vpn_range.get_end()) {
            self.unmap_present_one_deferred(page_table, vpn, batch)
        }
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end);
    }

    /// 将当前区域收缩到新的上界，只拆除尾部已实际映射的页。
    pub fn shrink_present_to(&mut self, page_table: &mut PageTable, new_end: VirtPageNum) {
        for vpn in VPNRange::new(new_end, self.vpn_range.get_end()) {
            self.unmap_present_one(page_table, vpn);
        }
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end);
    }

    #[allow(unused)]
    /// 将当前区域向高地址扩展到新的上界，并补齐新增页映射。
    pub fn append_to(&mut self, page_table: &mut PageTable, new_end: VirtPageNum) {
        self.append_to_checked(page_table, new_end)
            .expect("failed to append eagerly mapped VMA");
    }

    /// Extend an eagerly mapped VMA, rolling back newly mapped pages on error.
    pub fn append_to_checked(
        &mut self,
        page_table: &mut PageTable,
        new_end: VirtPageNum,
    ) -> Result<(), MmError> {
        let old_end = self.vpn_range.get_end();
        if new_end <= old_end {
            return Ok(());
        }

        let mut mapped = Vec::new();
        for vpn in VPNRange::new(old_end, new_end) {
            if page_table.translate(vpn).is_some() {
                for rollback_vpn in mapped {
                    self.unmap_present_one(page_table, rollback_vpn);
                }
                return Err(MmError::Conflict);
            }
            if let Err(err) = self.map_one(page_table, vpn) {
                self.unmap_present_one(page_table, vpn);
                for rollback_vpn in mapped {
                    self.unmap_present_one(page_table, rollback_vpn);
                }
                return Err(err);
            }
            mapped.push(vpn);
        }
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end);
        Ok(())
    }
    /// data: start-aligned but maybe with shorter length
    /// assume that all frames were cleared before
    pub fn copy_data(&mut self, page_table: &mut PageTable, data: &[u8]) {
        assert_eq!(self.map_type, MapType::Framed);
        let mut start: usize = 0;
        let mut current_vpn = self.vpn_range.get_start();
        let len = data.len();
        loop {
            let src = &data[start..len.min(start + PAGE_SIZE)];
            let dst = &mut page_table
                .translate(current_vpn)
                .unwrap()
                .ppn()
                .get_bytes_array()[..src.len()];
            dst.copy_from_slice(src);
            start += PAGE_SIZE;
            if start >= len {
                break;
            }
            current_vpn.step();
        }
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum MapType {
    Identical,
    /// Platform kernel direct-map VA translated back to its physical page.
    Direct,
    Framed,
}

bitflags! {
    /// map permission corresponding to that in pte: `R W X U`
    pub struct MapPermission: u8 {
        ///Readable
        const R = 1 << 1;
        ///Writable
        const W = 1 << 2;
        ///Excutable
        const X = 1 << 3;
        ///Accessible in U mode
        const U = 1 << 4;
    }
}

/// test map function in page table
#[allow(unused)]
pub fn remap_test() {
    let mut kernel_space = KERNEL_SPACE.lock();
    let mid_text: VirtAddr = ((stext as usize + etext as usize) / 2).into();
    let mid_rodata: VirtAddr = ((srodata as usize + erodata as usize) / 2).into();
    let mid_data: VirtAddr = ((sdata as usize + edata as usize) / 2).into();
    assert!(!kernel_space
        .page_table
        .translate(mid_text.floor())
        .unwrap()
        .writable(),);
    assert!(!kernel_space
        .page_table
        .translate(mid_rodata.floor())
        .unwrap()
        .writable(),);
    assert!(!kernel_space
        .page_table
        .translate(mid_data.floor())
        .unwrap()
        .executable(),);
    println!("remap_test passed!");
}
