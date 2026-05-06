//! Address Space [`MemorySet`] management of Process

use super::{frame_alloc, shootdown, FrameTracker, ShootdownKind};
use super::{PTEFlags, PageTable, PageTableEntry};
use super::{PhysAddr, PhysPageNum, VirtAddr, VirtPageNum};
use super::{StepByOne, VPNRange};
use crate::config::{MEMORY_END, MMIO, PAGE_SIZE, TRAMPOLINE, TRAP_CONTEXT_BASE, USER_MMAP_BASE, USER_STACK_BASE, USER_STACK_SIZE};
use crate::fs::{
    mark_cached_page_dirty, release_mapped_page, retain_mapped_page, CachePage, FileDescription,
};
use crate::sync::{SpinNoIrqLock};
use crate::task::ProcessControlBlock;
use crate::syscall::errno::ERRNO;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use fs::Inode;
use lazy_static::*;
use riscv::register::satp;

extern "C" {
    fn stext();
    fn etext();
    fn srodata();
    fn erodata();
    fn sdata();
    fn edata();
    fn sbss_with_stack();
    fn ebss();
    fn ekernel();
    fn strampoline();
}

/// ELF 加载结果，包含动态链接所需的额外信息
pub struct ElfLoadInfo {
    /// 程序入口点
    pub entry_point: usize,
    /// 程序头表在内存中的地址（用于 AT_PHDR）
    pub phdr_vaddr: usize,
    /// 程序头表项大小（用于 AT_PHENT）
    pub phent_size: usize,
    /// 程序头表项数量（用于 AT_PHNUM）
    pub phnum: usize,
    /// 动态链接器路径（如果存在 INTERP 段）
    pub interp_path: Option<String>,
}

lazy_static! {
    /// The kernel's initial memory mapping(kernel address space)
    pub static ref KERNEL_SPACE: Arc<SpinNoIrqLock<MemorySet>> =
        Arc::new(unsafe { SpinNoIrqLock::new(MemorySet::new_kernel()) });
    /// file-backed mmap 的反向映射注册表，用于 truncate 时找到需要失效的用户页表。
    static ref FILE_MAPPING_REGISTRY: SpinNoIrqLock<Vec<FileMappingEntry>> =
        SpinNoIrqLock::new(Vec::new());
}

/// the kernel token
pub fn kernel_token() -> usize {
    KERNEL_SPACE.lock().token()
}
/// 用于稳定标识一个底层 inode。
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct InodeKey {
    /// 文件系统编号。
    fs_id: u64,
    /// inode 编号。
    ino: u64,
}

impl InodeKey {
    /// 从 inode 中提取稳定 key。
    pub fn from_inode(inode: &Arc<Inode>) -> Self {
        Self {
            fs_id: inode.fs_id(),
            ino: inode.ino(),
        }
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

/// address space
pub struct MemorySet {
    /// page table
    pub page_table: PageTable,
    /// virtual memory areas, keyed by start VPN.
    pub vmas: BTreeMap<VirtPageNum, Vma>,
    /// 当前仍在用户态装载该地址空间的 hart 掩码。
    loaded_user_harts: AtomicUsize,
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
    /// shootdown 完成后才能释放的旧页对象。
    batch: UserReleaseBatch,
}

impl DeferredUserReclaim {
    /// 基于锁内快照创建一次用户页表延迟回收动作。
    pub(crate) fn new(token: usize, mask: usize, batch: UserReleaseBatch) -> Self {
        Self { token, mask, batch }
    }

    /// 判断本次回收是否实际持有旧页对象。
    pub fn is_empty(&self) -> bool {
        self.batch.is_empty()
    }

    /// 在目标 hart 完成 TLB shootdown 后释放旧页对象。
    pub fn flush_then_release(self) {
        if self.mask != 0 && !self.batch.is_empty() {
            debug!(
                "[tlb] deferred user reclaim shootdown: token={:#x} mask={:#b}",
                self.token,
                self.mask
            );
            shootdown(self.mask, ShootdownKind::AddressSpace { satp: self.token });
        }
        // self 在函数返回时析构，batch 的 Drop 会真正释放旧页引用。
    }
}

impl MemorySet {
    /// 完成一次会返回延迟回收 batch 的本地页表修改。
    fn finish_deferred_page_table_edit(&self) {
        // 本地 hart 可能刚刚使用过被拆除的翻译，必须先清掉本地 TLB；
        // 远端 hart 的同步由调用方构造 `DeferredUserReclaim` 后在锁外完成。
        unsafe {
            asm!("sfence.vma");
        }
    }

    /// Create a new empty `MemorySet`.
    pub fn new_bare() -> Self {
        Self {
            page_table: PageTable::new(),
            vmas: BTreeMap::new(),
            loaded_user_harts: AtomicUsize::new(0),
        }
    }
    /// Get he page table token
    pub fn token(&self) -> usize {
        self.page_table.token()
    }
    /// 标记某个 hart 即将返回用户态并装载该地址空间。
    pub fn mark_user_loaded(&self, hart_id: usize) {
        let bit = 1usize << hart_id;
        let mask = self.loaded_user_harts.fetch_or(bit, Ordering::AcqRel) | bit;
        trace!(
            "[tlb] user mm loaded on hart {} token={:#x} mask={:#b}",
            hart_id,
            self.token(),
            mask
        );
    }
    /// 标记某个 hart 已经离开用户态，不再需要作为该地址空间的远端 shootdown 目标。
    pub fn mark_user_unloaded(&self, hart_id: usize) {
        let bit = 1usize << hart_id;
        let mask = self.loaded_user_harts.fetch_and(!bit, Ordering::AcqRel) & !bit;
        trace!(
            "[tlb] user mm unloaded from hart {} token={:#x} mask={:#b}",
            hart_id,
            self.token(),
            mask
        );
    }
    /// 返回当前仍在用户态装载该地址空间的 hart 掩码。
    pub fn loaded_user_harts(&self) -> usize {
        self.loaded_user_harts.load(Ordering::Acquire)
    }
    /// 对当前仍在用户态装载该地址空间的 hart 发起同步 TLB shootdown。
    ///
    /// 调用方不能持有对应进程锁等待 ack。用户态 IPI 进入内核后会先更新进程
    /// 运行态信息，持锁等待可能导致远端 hart 无法进入 softirq 分支。
    ///
    /// 这里依赖当前 trap 语义：hart 从用户态进入内核时已经切到 kernel satp
    /// 并执行本地 `sfence.vma`，因此不在该掩码中的 hart 不应再持有这个用户
    /// 地址空间的旧翻译。若后续去掉 trap 入口 flush 或引入 ASID，需要重新审查。
    pub fn shootdown_loaded_user_harts(&self) {
        let mask = self.loaded_user_harts();
        self.shootdown_user_harts(mask);
    }
    /// 对指定 hart 掩码发起该地址空间的同步 TLB shootdown。
    ///
    /// 这个接口用于调用方已经在锁内快照出目标 mask，随后释放锁再执行同步等待
    /// 的场景。
    ///
    /// snapshot 只覆盖“页表修改完成时仍在用户态运行该 mm”的 hart。修改完成后
    /// 才从内核态返回用户态的 hart，必须已经经过 trap 入口的本地 flush 同步点。
    pub fn shootdown_user_harts(&self, mask: usize) {
        if mask == 0 {
            return;
        }
        debug!(
            "[tlb] shootdown user mm token={:#x} loaded_mask={:#b}",
            self.token(),
            mask
        );
        shootdown(mask, ShootdownKind::AddressSpace { satp: self.token() });
    }
    /// Assume that no conflicts.
    pub fn insert_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) {
        let _ = self.insert_vma(
            Vma::new(start_va, end_va, MapType::Framed, permission, VmaKind::Anonymous),
            None,
        );
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
            area.teardown(&mut self.page_table);
            unsafe {
                asm!("sfence.vma");
            }
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
    pub fn insert_vma(&mut self, mut vma: Vma, data: Option<&[u8]>) -> bool {
        if self.overlaps_vma_range(vma.start_vpn(), vma.end_vpn()) {
            return false;
        }
        if vma.should_eager_map() {
            vma.map(&mut self.page_table);
        }
        if let Some(data) = data {
            vma.copy_data(&mut self.page_table, data);
        }
        self.insert_vma_unchecked(vma);
        true
    }
    /// 仅登记一段 VMA 元数据，不立即建立页表映射。
    pub fn register_vma_metadata(&mut self, vma: Vma) -> bool {
        if self.overlaps_vma_range(vma.start_vpn(), vma.end_vpn()) {
            return false;
        }
        self.insert_vma_unchecked(vma);
        true
    }
    /// 把一张已有私有页接入指定虚拟页，供 `fork` 共享与后续 COW 使用。
    pub fn map_existing_private_page(
        &mut self,
        vpn: VirtPageNum,
        page: Arc<PrivatePage>,
        flags: PTEFlags,
    ) -> bool {
        if self.page_table.translate(vpn).is_some() {
            return false;
        }
        let Some(area) = self.find_vma_containing_mut(vpn) else {
            return false;
        };
        area.data_frames.insert(vpn, Arc::clone(&page));
        self.page_table.map(vpn, page.ppn(), flags);
        // debug!(
        //     "[cow] install shared private page: vpn={:#x} ppn={:#x} writable={} cow={}",
        //     vpn.0,
        //     page.ppn().0,
        //     flags.contains(PTEFlags::W),
        //     page.is_cow()
        // );
        true
    }
    /// 把一张已有的 page cache 页直接接入指定虚拟页，供 `fork` 继承只读文件私有映射。
    pub fn map_existing_direct_cache_page(
        &mut self,
        vpn: VirtPageNum,
        page: Arc<SpinNoIrqLock<CachePage>>,
        flags: PTEFlags,
    ) -> bool {
        if self.page_table.translate(vpn).is_some() {
            return false;
        }
        let Some(area) = self.find_vma_containing_mut(vpn) else {
            return false;
        };
        retain_mapped_page(&page);
        area.direct_cache_pages.insert(vpn, Arc::clone(&page));
        self.page_table.map(vpn, page.lock().ppn(), flags);
        // debug!(
        //     "[cow] install inherited direct cache page: vpn={:#x} ppn={:#x} writable={}",
        //     vpn.0,
        //     page.lock().ppn().0,
        //     flags.contains(PTEFlags::W)
        // );
        true
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
    /// Find a free user mmap range using a hint first, then wrap to the base.
    pub fn find_free_mmap_area(&self, hint: usize, base: usize, len: usize) -> Option<usize> {
        let upper = TRAP_CONTEXT_BASE;
        let start = align_up(hint.max(base), PAGE_SIZE)?;
        self.find_free_area_in_range(start, upper, len)
            .or_else(|| {
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
    /// Mention that trampoline is not collected by areas.
    fn map_trampoline(&mut self) {
        self.page_table.map(
            VirtAddr::from(TRAMPOLINE).into(),
            PhysAddr::from(strampoline as usize).into(),
            PTEFlags::R | PTEFlags::X,
        );
    }
    /// Without kernel stacks.
    pub fn new_kernel() -> Self {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map kernel sections
        info!(".text [{:#x}, {:#x})", stext as usize, etext as usize);
        info!(".rodata [{:#x}, {:#x})", srodata as usize, erodata as usize);
        info!(".data [{:#x}, {:#x})", sdata as usize, edata as usize);
        info!(
            ".bss [{:#x}, {:#x})",
            sbss_with_stack as usize, ebss as usize
        );
        info!("mapping .text section");
        let _ = memory_set.insert_vma(
            Vma::new(
                (stext as usize).into(),
                (etext as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::X,
                VmaKind::Kernel,
            ),
            None,
        );
        info!("mapping .rodata section");
        let _ = memory_set.insert_vma(
            Vma::new(
                (srodata as usize).into(),
                (erodata as usize).into(),
                MapType::Identical,
                MapPermission::R,
                VmaKind::Kernel,
            ),
            None,
        );
        info!("mapping .data section");
        let _ = memory_set.insert_vma(
            Vma::new(
                (sdata as usize).into(),
                (edata as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
                VmaKind::Kernel,
            ),
            None,
        );
        info!("mapping .bss section");
        let _ = memory_set.insert_vma(
            Vma::new(
                (sbss_with_stack as usize).into(),
                (ebss as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
                VmaKind::Kernel,
            ),
            None,
        );
        info!("mapping physical memory");
        let _ = memory_set.insert_vma(
            Vma::new(
                (ekernel as usize).into(),
                MEMORY_END.into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
                VmaKind::Kernel,
            ),
            None,
        );
        info!("mapping memory-mapped registers");
        for pair in MMIO {
            let _ = memory_set.insert_vma(
                Vma::new(
                    (*pair).0.into(),
                    ((*pair).0 + (*pair).1).into(),
                    MapType::Identical,
                    MapPermission::R | MapPermission::W,
                    VmaKind::Kernel,
                ),
                None,
            );
        }
        memory_set
    }
    /// Include ELF segments and trampoline, and compute initial process VM layout.
    /// Returns (MemorySet, UserSpaceLayout, ElfLoadInfo)
    pub fn from_elf(elf_data: &[u8]) -> Result<(Self, UserSpaceLayout, ElfLoadInfo), ERRNO> {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map program headers of elf, with U flag
        let elf = xmas_elf::ElfFile::new(elf_data).map_err(|_| ERRNO::ENOEXEC)?;
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");
        let ph_count = elf_header.pt2.ph_count();
        let mut max_end_vpn = VirtPageNum(0);

        // 收集动态链接信息
        let mut interp_path: Option<String> = None;
        let phdr_vaddr = elf_header.pt2.ph_offset() as usize; // 程序头表文件偏移
        let mut phdr_load_vaddr: Option<usize> = None; // 程序头表加载后的虚拟地址

        for i in 0..ph_count {
            let ph = elf.program_header(i).map_err(|_| ERRNO::ENOEXEC)?;

            // 检查 INTERP 段
            if ph.get_type().unwrap() == xmas_elf::program::Type::Interp {
                debug!("Found INTERP segment in ELF program header");
                let offset = ph.offset() as usize;
                let size = ph.file_size() as usize;
                if size > 0 && offset + size <= elf_data.len() {
                    let interp_bytes = &elf_data[offset..offset + size];
                    // INTERP 段内容是以 null 结尾的字符串
                    let end = interp_bytes.iter().position(|&b| b == 0).unwrap_or(interp_bytes.len());
                    if let Ok(path) = core::str::from_utf8(&interp_bytes[..end]) {
                        interp_path = Some(String::from(path));
                        debug!("Found INTERP segment: {}", path);
                    }
                }
            }

            if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
                let start_va: VirtAddr = (ph.virtual_addr() as usize).into();
                let end_va: VirtAddr = ((ph.virtual_addr() + ph.mem_size()) as usize).into();

                // 检查程序头表是否在这个 LOAD 段内
                if phdr_load_vaddr.is_none() {
                    let seg_file_start = ph.offset() as usize;
                    let seg_file_end = seg_file_start + ph.file_size() as usize;
                    if phdr_vaddr >= seg_file_start && phdr_vaddr < seg_file_end {
                        // 程序头表在此段内，计算其虚拟地址
                        let offset_in_seg = phdr_vaddr - seg_file_start;
                        phdr_load_vaddr = Some(ph.virtual_addr() as usize + offset_in_seg);
                    }
                }

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
                debug!("mapping ELF segment: [{:#x}, {:#x}) with flags {:?}",
                    &(usize::from(start_va)), &(usize::from(end_va)), map_perm);
                let vma = Vma::new_elf(start_va, end_va, map_perm);
                max_end_vpn = vma.end_vpn();
                // start_va may not be page-aligned (p_vaddr % p_align == p_offset % p_align).
                // copy_data writes from the start of the first mapped page, so we must pad
                // the data with zeros equal to start_va's within-page offset so that each
                // ELF byte lands at the correct virtual address.
                let page_off = start_va.page_offset();
                let raw = &elf.input[ph.offset() as usize..(ph.offset() + ph.file_size()) as usize];
                let padded: Vec<u8>;
                let seg_data: &[u8] = if page_off != 0 {
                    let mut buf = alloc::vec![0u8; page_off + raw.len()];
                    buf[page_off..].copy_from_slice(raw);
                    padded = buf;
                    &padded
                } else {
                    raw
                };
                let _ = memory_set.insert_vma(vma, Some(seg_data));
            }
        }
        let max_end_va: VirtAddr = max_end_vpn.into();
        let start_brk: usize = max_end_va.into();
        let layout = UserSpaceLayout {
            start_brk,
            mmap_base: USER_MMAP_BASE,
            ustack_base: USER_STACK_BASE,
            start_stack: USER_STACK_BASE + USER_STACK_SIZE,
        };

        let load_info = ElfLoadInfo {
            entry_point: elf.header.pt2.entry_point() as usize,
            phdr_vaddr: phdr_load_vaddr.unwrap_or(0),
            phent_size: elf.header.pt2.ph_entry_size() as usize,
            phnum: ph_count as usize,
            interp_path,
        };

        Ok((memory_set, layout, load_info))
    }
    /// Create a new address space by copy code&data from a exited process's address space.
    pub fn from_existed_user(user_space: &mut Self) -> (Self, bool) {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        let mut parent_tlb_needs_flush = false;
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
            // debug!(
            //     "[cow] fork inspect VMA: start={:#x} end={:#x} kind={:?} share_private_pages={} private_pages={} direct_cache_pages={}",
            //     area.start_vpn().0,
            //     area.end_vpn().0,
            //     area.kind,
            //     share_private_pages,
            //     area.data_frames.len(),
            //     area.direct_cache_pages.len()
            // );
            if share_private_pages {
                let _ = memory_set.register_vma_metadata(new_area);
            } else {
                let _ = memory_set.insert_vma(new_area, None);
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
            for (vpn, page) in private_pages {
                if share_private_pages {
                    let mut child_flags = user_space.translate(vpn).unwrap().flags();
                    child_flags.remove(PTEFlags::D);
                    if map_perm.contains(MapPermission::W) {
                        // 将父子双方都降为只读，后续写入通过缺页走 COW。
                        page.set_cow(true);
                        child_flags.remove(PTEFlags::W);
                        let _ = user_space.page_table.update_flags(vpn, child_flags);
                        parent_tlb_needs_flush = true;
                    }
                    // debug!(
                    //     "[cow] fork share private page: vpn={:#x} ppn={:#x} writable={} child_writable={} cow={}",
                    //     vpn.0,
                    //     page.ppn().0,
                    //     map_perm.contains(MapPermission::W),
                    //     child_flags.contains(PTEFlags::W),
                    //     page.is_cow()
                    // );
                    let _ = memory_set.map_existing_private_page(vpn, page, child_flags);
                    continue;
                }
                if memory_set.translate(vpn).is_none() {
                    if !memory_set.map_private_page_in_vma(vpn) {
                        continue;
                    }
                }
                let src_ppn = user_space.translate(vpn).unwrap().ppn();
                let dst_ppn = memory_set.translate(vpn).unwrap().ppn();
                dst_ppn
                    .get_bytes_array()
                    .copy_from_slice(src_ppn.get_bytes_array());
                debug!(
                    "[cow] fork copy private page directly: vpn={:#x} src_ppn={:#x} dst_ppn={:#x}",
                    vpn.0,
                    src_ppn.0,
                    dst_ppn.0
                );
            }
            // 对于已经直接映到 page cache 的文件页，子进程也直接继承当前映射。
            // `MAP_PRIVATE` 仍然保持只读，`MAP_SHARED` 在 sticky dirty 语义下保留父进程当前 `W` 状态。
            if inherit_direct_cache_pages {
                for (vpn, page) in direct_cache_pages {
                    if memory_set.translate(vpn).is_some() {
                        continue;
                    }
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
                    let _ = memory_set.map_existing_direct_cache_page(vpn, page, child_flags);
                }
            }
        }
        if parent_tlb_needs_flush {
            unsafe {
                asm!("sfence.vma");
            }
            debug!("[cow] fork flush parent local TLB after write-protecting shared private pages");
        }
        (memory_set, parent_tlb_needs_flush)
    }
    /// Change page table by writing satp CSR Register.
    pub fn activate(&self) {
        let satp = self.page_table.token();
        unsafe {
            satp::write(satp);
            asm!("sfence.vma");
        }
    }
    /// Translate a virtual page number to a page table entry
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }

    /// 拆除全部用户 VMA，并把旧页对象放入延迟释放批次。
    pub(crate) fn recycle_data_pages_deferred(&mut self) -> UserReleaseBatch {
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
        for area in self.vmas.values_mut() {
            area.teardown(&mut self.page_table);
        }
        self.vmas.clear();
        unsafe {
            asm!("sfence.vma");
        }
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

            let direct_vpns: Vec<_> = area.direct_cache_pages.keys().copied().collect();
            for vpn in direct_vpns {
                let Some(page_idx) = area.file_page_index(vpn) else {
                    continue;
                };
                let page_start = page_idx as usize * PAGE_SIZE;
                if page_start >= new_size {
                    area.unmap_present_one_deferred(&mut self.page_table, vpn, &mut batch);
                    pte_changed = true;
                    continue;
                }
                if new_size < page_start + PAGE_SIZE {
                    if let Some(page) = area.direct_cache_pages.get(&vpn) {
                        let page = page.lock();
                        page.ppn().get_bytes_array()[new_size - page_start..].fill(0);
                    }
                }
            }

            let private_vpns: Vec<_> = area.data_frames.keys().copied().collect();
            for vpn in private_vpns {
                let Some(page_idx) = area.file_page_index(vpn) else {
                    continue;
                };
                let page_start = page_idx as usize * PAGE_SIZE;
                if page_start >= new_size {
                    area.unmap_present_one_deferred(&mut self.page_table, vpn, &mut batch);
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
            area.shrink_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }

    /// 将一段 VMA 收缩到新的上界，只拆除已经实际映射的尾部页。
    pub fn shrink_metadata_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        if let Some(area) = self.vmas.get_mut(&start.floor()) {
            area.shrink_present_to(&mut self.page_table, new_end.ceil());
            unsafe {
                asm!("sfence.vma");
            }
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
        if self.overlaps_vma_range(old_end, new_end_vpn) {
            return false;
        }
        let Some(area) = self.vmas.get_mut(&start_vpn) else {
            return false;
        };
        area.vpn_range = VPNRange::new(start_vpn, new_end_vpn);
        true
    }

    /// map an anonymous area with given permission, return true if success
    pub fn mmap_anonymous(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) -> bool {
        debug!(
            "[mmap] register anonymous VMA: start={:#x} end={:#x} perm={:?} eager=true",
            usize::from(start_va),
            usize::from(end_va),
            permission
        );
        if !self.insert_vma(Vma::new_anonymous(start_va, end_va, permission), None) {
            return false;
        }
        unsafe {
            asm!("sfence.vma");
        }
        true
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
    ) -> bool {
        debug!(
            "[mmap] register file VMA: start={:#x} end={:#x} perm={:?} pgoff={} shared={} lazy=true path={:?}",
            usize::from(start_va),
            usize::from(end_va),
            permission,
            pgoff,
            shared,
            file.path()
        );
        if !self.insert_vma(Vma::new_file(start_va, end_va, permission, file, pgoff, shared), None) {
            return false;
        }
        unsafe {
            asm!("sfence.vma");
        }
        true
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
        // debug!(
        //     "[munmap] begin teardown: start={:#x} end={:#x} start_vpn={:#x} end_vpn={:#x}",
        //     usize::from(start_va),
        //     usize::from(end_va),
        //     start_vpn.0,
        //     end_vpn.0
        // );
        for vpn in VPNRange::new(start_vpn, end_vpn) {
            let Some(area) = self.find_vma_containing(vpn) else {
                return None;
            };
            if !area.is_user_accessible() {
                return None;
            }
        }

        let mut batch = UserReleaseBatch::new();
        let old_vmas = core::mem::take(&mut self.vmas);
        let mut new_areas: Vec<Vma> = Vec::with_capacity(old_vmas.len() + 1);
        for mut area in old_vmas.into_values() {
            let area_start = area.start_vpn();
            let area_end = area.end_vpn();
            let overlap_start = if area_start > start_vpn {
                area_start
            } else {
                start_vpn
            };
            let overlap_end = if area_end < end_vpn {
                area_end
            } else {
                end_vpn
            };

            if overlap_start >= overlap_end {
                new_areas.push(area);
                continue;
            }

            // debug!(
            //     "[munmap] overlap VMA: area_start={:#x} area_end={:#x} overlap_start={:#x} overlap_end={:#x} file_backed={} direct_cache_pages={} private_pages={}",
            //     area_start.0,
            //     area_end.0,
            //     overlap_start.0,
            //     overlap_end.0,
            //     area.file.is_some(),
            //     area.direct_cache_pages.len(),
            //     area.data_frames.len()
            // );

            for vpn in VPNRange::new(overlap_start, overlap_end) {
                area.unmap_present_one_deferred(&mut self.page_table, vpn, &mut batch);
            }

            if area_start < overlap_start {
                if let Some(left_tail) = area.split_off(overlap_start) {
                    let overlap_area = left_tail;
                    new_areas.push(area);
                    area = overlap_area;
                }
            }

            if overlap_end < area_end {
                if let Some(right_area) = area.split_off(overlap_end) {
                    new_areas.push(right_area);
                }
            }
        }
        self.rebuild_vmas_from_vec(new_areas);
        self.merge_adjacent_vmas();
        self.finish_deferred_page_table_edit();
        debug!(
            "[munmap] complete teardown: start_vpn={:#x} end_vpn={:#x}",
            start_vpn.0,
            end_vpn.0
        );
        Some(batch)
    }

    /// 为 file-backed 缺页生成锁外慢路径所需的最小计划。
    pub fn prepare_file_page_fault(
        &self,
        fault_va: VirtAddr,
        access: PageFaultAccess,
    ) -> Option<FilePageFaultPlan> {
        let vpn = fault_va.floor();
        if self.page_table.translate(vpn).is_some() {
            return None;
        }
        let area = self.find_vma_containing(vpn)?;
        if !area.is_user_accessible() || !area.allows_fault_access(access) {
            return None;
        }
        let file = area.file.as_ref()?;
        let plan = FilePageFaultPlan {
            vpn,
            vma_start: area.start_vpn(),
            vma_end: area.end_vpn(),
            map_perm: area.map_perm,
            file: Arc::clone(&file.file),
            page_idx: area.file_page_index(vpn)?,
            pgoff: file.pgoff,
            shared: file.shared,
            access,
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
        Some(plan)
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
    pub fn map_private_page_in_vma(&mut self, vpn: VirtPageNum) -> bool {
        if self.page_table.translate(vpn).is_some() {
            return true;
        }
        let Some(area) = self.find_vma_containing_mut(vpn) else {
            return false;
        };
        let map_type = area.map_type;
        let map_perm = area.map_perm;
        let ppn: PhysPageNum = match map_type {
            MapType::Identical => PhysPageNum(vpn.0),
            MapType::Framed => {
                let frame = frame_alloc().unwrap();
                let page = Arc::new(PrivatePage::new(frame));
                let ppn = page.ppn();
                debug!(
                    "[mmap] allocate private frame for lazy fault: vpn={:#x} ppn={:#x}",
                    vpn.0,
                    ppn.0
                );
                area.data_frames.insert(vpn, page);
                ppn
            }
        };
        let pte_flags = PTEFlags::from_bits(map_perm.bits).unwrap();
        self.page_table.map(vpn, ppn, pte_flags);
        true
    }

    /// 在 heap VMA 内按需分配并映射一个私有页。
    pub fn handle_lazy_heap_fault(&mut self, fault_va: VirtAddr, access: PageFaultAccess) -> bool {
        let vpn = fault_va.floor();
        if self.page_table.translate(vpn).is_some() {
            return false;
        }
        let Some(area) = self.find_vma_containing(vpn) else {
            return false;
        };
        if !area.is_heap() || !area.is_user_accessible() || !area.allows_fault_access(access) {
            return false;
        }
        let committed = self.map_private_page_in_vma(vpn);
        if committed {
            unsafe {
                asm!("sfence.vma");
            }
        }
        committed
    }

    /// 把一个 page cache 页直接映射进用户页表，供 `MAP_SHARED` 使用。
    pub fn map_shared_file_page(
        &mut self,
        plan: &FilePageFaultPlan,
        page: Arc<SpinNoIrqLock<CachePage>>,
    ) -> bool {
        if self.page_table.translate(plan.vpn).is_some() {
            return true;
        }
        if !self.can_commit_file_page_fault(plan) {
            return false;
        }
        let mut pte_flags = PTEFlags::from_bits(plan.map_perm.bits).unwrap();
        if plan.shared
            && plan.map_perm.contains(MapPermission::W)
            && plan.access != PageFaultAccess::Write
        {
            pte_flags.remove(PTEFlags::W);
        }
        let ppn = page.lock().ppn();
        if plan.shared
            && plan.map_perm.contains(MapPermission::W)
            && plan.access == PageFaultAccess::Write
        {
            mark_cached_page_dirty(&page);
        }
        retain_mapped_page(&page);
        let area = self
            .find_vma_containing_mut(plan.vpn)
            .expect("validated file fault VMA disappeared");
        if let Some(old_page) = area.direct_cache_pages.insert(plan.vpn, Arc::clone(&page)) {
            release_mapped_page(&old_page);
        }
        self.page_table.map(plan.vpn, ppn, pte_flags);
        unsafe {
            asm!("sfence.vma");
        }
        debug!(
            "[mmap] committed MAP_SHARED fault: vpn={:#x} page_idx={} ppn={:#x} writable={} path={:?}",
            plan.vpn.0,
            plan.page_idx,
            ppn.0,
            pte_flags.contains(PTEFlags::W),
            plan.file.path()
        );
        true
    }

    /// 将 file-backed `MAP_PRIVATE` 的缓存页以只读方式直接接入页表。
    fn map_private_file_cache_page(
        &mut self,
        plan: &FilePageFaultPlan,
        page: Arc<SpinNoIrqLock<CachePage>>,
    ) -> bool {
        if self.page_table.translate(plan.vpn).is_some() {
            return true;
        }
        if !self.can_commit_file_page_fault(plan) {
            return false;
        }
        let mut pte_flags = PTEFlags::from_bits(plan.map_perm.bits).unwrap();
        pte_flags.remove(PTEFlags::W);
        pte_flags.remove(PTEFlags::D);
        let ppn = page.lock().ppn();
        retain_mapped_page(&page);
        let area = self
            .find_vma_containing_mut(plan.vpn)
            .expect("validated private file fault VMA disappeared");
        if let Some(old_page) = area.direct_cache_pages.insert(plan.vpn, Arc::clone(&page)) {
            release_mapped_page(&old_page);
        }
        self.page_table.map(plan.vpn, ppn, pte_flags);
        unsafe {
            asm!("sfence.vma");
        }
        debug!(
            "[cow] install MAP_PRIVATE readonly cache page: vpn={:#x} page_idx={} ppn={:#x} access={:?} path={:?}",
            plan.vpn.0,
            plan.page_idx,
            ppn.0,
            plan.access,
            plan.file.path()
        );
        true
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
        unsafe {
            asm!("sfence.vma");
        }
        debug!(
            "[mmap] shared write-notify fault: vpn={:#x} ppn={:#x} path={:?}",
            vpn.0,
            pte.ppn().0,
            path
        );
        true
    }

    /// 处理私有页的写时复制缺页。
    pub(crate) fn handle_private_cow_fault(&mut self, fault_va: VirtAddr) -> Option<UserReleaseBatch> {
        let mut batch = UserReleaseBatch::new();
        let vpn = fault_va.floor();
        let Some(pte) = self.page_table.translate(vpn) else {
            return None;
        };
        if pte.writable() {
            // 可能是其他 hart 已经把该页从 COW 只读状态放宽为可写，
            // 当前 hart 仍命中了陈旧的只读 TLB。刷新本地后让用户态重试。
            self.finish_deferred_page_table_edit();
            return Some(batch);
        }
        let file_private_cache_page = {
            let Some(area) = self.find_vma_containing(vpn) else {
                return None;
            };
            if !area.supports_private_page_sharing() || !area.allows_fault_access(PageFaultAccess::Write) {
                return None;
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
            let new_page = Arc::new(PrivatePage::new(frame_alloc().unwrap()));
            new_page
                .ppn()
                .get_bytes_array()
                .copy_from_slice(cache_page.lock().ppn().get_bytes_array());
            let mut writable_flags = pte.flags();
            writable_flags.insert(PTEFlags::W);
            writable_flags.remove(PTEFlags::D);
            let Some(area) = self.find_vma_containing_mut(vpn) else {
                return None;
            };
            if let Some(old_page) = area.direct_cache_pages.remove(&vpn) {
                batch.push_direct_cache(old_page);
            }
            area.data_frames.insert(vpn, Arc::clone(&new_page));
            if !self.page_table.replace(vpn, new_page.ppn(), writable_flags) {
                return None;
            }
            self.finish_deferred_page_table_edit();
            debug!(
                "[cow] materialize MAP_PRIVATE page on write fault: vpn={:#x} cache_ppn={:#x} new_ppn={:#x} path={:?}",
                vpn.0,
                cache_page.lock().ppn().0,
                new_page.ppn().0,
                path
            );
            return Some(batch);
        }
        let (page, path) = {
            let Some(area) = self.find_vma_containing(vpn) else {
                return None;
            };
            if !area.supports_private_page_sharing() || !area.allows_fault_access(PageFaultAccess::Write) {
                return None;
            }
            let Some(page) = area.data_frames.get(&vpn).cloned() else {
                return None;
            };
            if !page.is_cow() {
                return None;
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
                return None;
            }
            self.finish_deferred_page_table_edit();
            debug!(
                "[cow] reuse exclusive private page: vpn={:#x} ppn={:#x} path={:?}",
                vpn.0,
                page.ppn().0,
                path
            );
            return Some(batch);
        }

        let new_page = Arc::new(PrivatePage::new(frame_alloc().unwrap()));
        new_page
            .ppn()
            .get_bytes_array()
            .copy_from_slice(page.ppn().get_bytes_array());
        let Some(area) = self.find_vma_containing_mut(vpn) else {
            return None;
        };
        if let Some(old_page) = area.data_frames.insert(vpn, Arc::clone(&new_page)) {
            batch.push_private(old_page);
        }
        if !self.page_table.replace(vpn, new_page.ppn(), writable_flags) {
            return None;
        }
        self.finish_deferred_page_table_edit();
        debug!(
            "[cow] copy private page on write fault: vpn={:#x} old_ppn={:#x} new_ppn={:#x} path={:?}",
            vpn.0,
            page.ppn().0,
            new_page.ppn().0,
            path
        );
        Some(batch)
    }

    /// 为 `MAP_PRIVATE` 缺页分配私有页框，并以 page cache 作为填充源。
    pub fn map_private_file_page(
        &mut self,
        plan: &FilePageFaultPlan,
        page: Arc<SpinNoIrqLock<CachePage>>,
    ) -> bool {
        if self.page_table.translate(plan.vpn).is_some() {
            return true;
        }
        if !self.can_commit_file_page_fault(plan) {
            return false;
        }
        if plan.access != PageFaultAccess::Write {
            // 首次读/执行缺页先共享只读 page cache 页，首次写再通过 COW 物化私有页。
            return self.map_private_file_cache_page(plan, page);
        }
        if !self.map_private_page_in_vma(plan.vpn) {
            return false;
        }
        let dst_ppn = self.page_table.translate(plan.vpn).unwrap().ppn();
        let dst = dst_ppn.get_bytes_array();
        let page_guard = page.lock();
        let src = page_guard.ppn().get_bytes_array();
        dst.copy_from_slice(src);
        unsafe {
            asm!("sfence.vma");
        }
        debug!(
            "[cow] materialize MAP_PRIVATE page on first write fault: vpn={:#x} page_idx={} dst_ppn={:#x} path={:?}",
            plan.vpn.0,
            plan.page_idx,
            dst_ppn.0,
            plan.file.path()
        );
        true
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
        debug!("mprotect_range: [{:#x}, {:#x}) with permission {:?}", start_va.0, end_va.0, permission);

        let start_vpn = start_va.floor();
        let end_vpn = end_va.ceil();

        // Validation: every page must be mapped, user-accessible and belong to some user VMA.
        //
        // To avoid O(pages × vmas) behavior, first collect and merge all user-accessible
        // VMA subranges that overlap [start_vpn, end_vpn), then validate pages against
        // this compact list in a single linear pass.
        let mut user_ranges: Vec<(VirtPageNum, VirtPageNum)> = Vec::new();
        for area in self.vmas.values() {
            if !area.is_user_accessible() {
                continue;
            }
            let area_start = area.start_vpn();
            let area_end = area.end_vpn();
            if area_end <= start_vpn || area_start >= end_vpn {
                // No overlap with requested range.
                continue;
            }
            let overlap_start = if area_start > start_vpn { area_start } else { start_vpn };
            let overlap_end = if area_end < end_vpn { area_end } else { end_vpn };
            if overlap_start >= overlap_end {
                continue;
            }
            if let Some((last_start, last_end)) = user_ranges.last_mut() {
                // Merge adjacent overlaps to keep the list compact.
                if *last_end == overlap_start {
                    *last_end = overlap_end;
                    continue;
                }
            }
            user_ranges.push((overlap_start, overlap_end));
        }

        // Now walk pages once, checking that each page lies within a user-accessible VMA range.
        let mut range_idx = 0usize;
        let mut current_range = user_ranges.get(range_idx).cloned();
        for vpn in VPNRange::new(start_vpn, end_vpn) {
            // Ensure there is a current range that may cover this vpn.
            while let Some((_, range_end)) = current_range {
                if vpn < range_end {
                    break;
                }
                range_idx += 1;
                current_range = user_ranges.get(range_idx).cloned();
            }
            let Some((range_start, range_end)) = current_range else {
                // No more user-accessible ranges but still pages left to validate.
                return false;
            };
            if vpn < range_start || vpn >= range_end {
                // Hole in user-accessible coverage.
                return false;
            }
            if let Some(pte) = self.page_table.translate(vpn) {
                if !pte.flags().contains(PTEFlags::U) {
                    return false;
                }
            }
        }

        // Modification: split VMAs as needed and update page table flags.
        let old_vmas = core::mem::take(&mut self.vmas);
        let mut new_areas: Vec<Vma> = Vec::with_capacity(old_vmas.len() + 1);
        for mut area in old_vmas.into_values() {
            let area_start = area.start_vpn();
            let area_end = area.end_vpn();
            let overlap_start = if area_start > start_vpn { area_start } else { start_vpn };
            let overlap_end = if area_end < end_vpn { area_end } else { end_vpn };

            if overlap_start >= overlap_end {
                new_areas.push(area);
                continue;
            }

            // left part
            if area_start < overlap_start {
                let left_data_frames = area.data_frames.split_off(&overlap_start);
                let left_direct_cache_pages = area.direct_cache_pages.split_off(&overlap_start);
                let left_area = Vma {
                    vpn_range: VPNRange::new(area_start, overlap_start),
                    data_frames: area.data_frames,
                    map_type: area.map_type,
                    map_perm: area.map_perm,
                    kind: area.kind.clone(),
                    file: area.file.clone(),
                    direct_cache_pages: area.direct_cache_pages,
                };
                area.data_frames = left_data_frames;
                area.direct_cache_pages = left_direct_cache_pages;
                if let Some(file) = area.file.as_mut() {
                    file.pgoff += overlap_start.0 - area_start.0;
                }
                new_areas.push(left_area);
            }

            // right part exists -> split and handle middle separately
            if overlap_end < area_end {
                let right_data_frames = area.data_frames.split_off(&overlap_end);
                let right_direct_cache_pages = area.direct_cache_pages.split_off(&overlap_end);
                let mut right_file = area.file.clone();
                if let Some(file) = right_file.as_mut() {
                    file.pgoff += overlap_end.0 - area_start.0;
                }
                let right_area = Vma {
                    vpn_range: VPNRange::new(overlap_end, area_end),
                    data_frames: right_data_frames,
                    map_type: area.map_type,
                    map_perm: area.map_perm,
                    kind: area.kind.clone(),
                    file: right_file,
                    direct_cache_pages: right_direct_cache_pages,
                };

                // update middle pages' PTE flags
                let pte_flags = PTEFlags::from_bits(permission.bits).unwrap();
                for vpn in VPNRange::new(overlap_start, overlap_end) {
                    if self.page_table.translate(vpn).is_some() {
                        self.page_table.update_flags(vpn, pte_flags);
                    }
                }

                area.vpn_range = VPNRange::new(overlap_start, overlap_end);
                area.map_perm = permission;
                new_areas.push(area);
                new_areas.push(right_area);
            } else {
                // no right split, area becomes the middle area
                let pte_flags = PTEFlags::from_bits(permission.bits).unwrap();
                for vpn in VPNRange::new(overlap_start, overlap_end) {
                    if self.page_table.translate(vpn).is_some() {
                        self.page_table.update_flags(vpn, pte_flags);
                    }
                }
                area.vpn_range = VPNRange::new(overlap_start, overlap_end);
                area.map_perm = permission;
                new_areas.push(area);
            }
        }

        self.rebuild_vmas_from_vec(new_areas);
        self.merge_adjacent_vmas();
        unsafe {
            asm!("sfence.vma");
        }
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
    /// 文件映射区域。
    File,
}

/// 文件映射区域附带的底层对象信息。
#[derive(Clone)]
pub struct FileVma {
    /// 建立映射时引用的打开文件描述。
    pub file: Arc<FileDescription>,
    /// 文件页偏移，单位为页。
    pub pgoff: usize,
    /// 是否为 `MAP_SHARED` 映射。
    pub shared: bool,
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
        Self::new(
            start_va,
            end_va,
            MapType::Framed,
            MapPermission::R | MapPermission::W,
            VmaKind::TrapContext { tid },
        )
    }
    /// 为匿名映射场景生成一段普通用户区域。
    pub fn new_anonymous(start_va: VirtAddr, end_va: VirtAddr, map_perm: MapPermission) -> Self {
        Self::new(
            start_va,
            end_va,
            MapType::Framed,
            map_perm,
            VmaKind::Anonymous,
        )
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
        let mut vma = Self::new(
            start_va,
            end_va,
            MapType::Framed,
            map_perm,
            VmaKind::File,
        );
        vma.file = Some(FileVma {
            file,
            pgoff,
            shared,
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
            direct_cache_pages: BTreeMap::new(),
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
        !matches!(self.file.as_ref(), Some(file) if file.shared)
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
    /// 判断当前区域是否需要在建 VMA 时立即分配并建立页表映射。
    pub fn should_eager_map(&self) -> bool {
        self.file.is_none()
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
        if let Some(page) = self.direct_cache_pages.remove(&vpn) {
            let shared_file_mapping = self.file.as_ref().map(|file| file.shared).unwrap_or(false);
            debug!(
                "[munmap] defer file cache mapping release: vpn={:#x} shared={}",
                vpn.0,
                shared_file_mapping
            );
            if let Some(old_pte) = page_table.clear(vpn) {
                if shared_file_mapping && old_pte.flags().contains(PTEFlags::D) {
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
        if self.map_type == MapType::Identical {
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
                let shared_file_mapping = self.file.as_ref().map(|file| file.shared).unwrap_or(false);
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
        if self.map_type == MapType::Identical {
            for vpn in self.vpn_range {
                let _ = page_table.clear(vpn);
            }
        }
        frames
    }
    /// 为指定虚拟页建立单页映射，并在需要时分配新的物理页框。
    pub fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        let ppn: PhysPageNum;
        match self.map_type {
            MapType::Identical => {
                ppn = PhysPageNum(vpn.0);
            }
            MapType::Framed => {
                let page = Arc::new(PrivatePage::new(frame_alloc().unwrap()));
                ppn = page.ppn();
                self.data_frames.insert(vpn, page);
            }
        }
        let pte_flags = PTEFlags::from_bits(self.map_perm.bits).unwrap();
        page_table.map(vpn, ppn, pte_flags);
    }
    /// 为当前区域覆盖的全部虚拟页建立映射。
    pub fn map(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.map_one(page_table, vpn);
        }
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
        for vpn in VPNRange::new(self.vpn_range.get_end(), new_end) {
            self.map_one(page_table, vpn)
        }
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end);
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
