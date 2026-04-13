//! Address Space [`MemorySet`] management of Process

use super::{frame_alloc, FrameTracker};
use super::{PTEFlags, PageTable, PageTableEntry};
use super::{PhysAddr, PhysPageNum, VirtAddr, VirtPageNum};
use super::{StepByOne, VPNRange};
use crate::config::{MEMORY_END, MMIO, PAGE_SIZE, TRAMPOLINE, USER_MMAP_BASE, USER_STACK_BASE, USER_STACK_SIZE};
use crate::fs::{
    mark_cached_page_dirty, release_mapped_page, retain_mapped_page, CachePage, FileDescription,
};
use crate::sync::{SpinNoIrqLock};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::asm;
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

lazy_static! {
    /// The kernel's initial memory mapping(kernel address space)
    pub static ref KERNEL_SPACE: Arc<SpinNoIrqLock<MemorySet>> =
        Arc::new(unsafe { SpinNoIrqLock::new(MemorySet::new_kernel()) });
}

/// the kernel token
pub fn kernel_token() -> usize {
    KERNEL_SPACE.lock().token()
}

/// address space
pub struct MemorySet {
    /// page table
    pub page_table: PageTable,
    /// virtual memory areas
    pub vmas: Vec<Vma>,
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

impl MemorySet {
    /// Create a new empty `MemorySet`.
    pub fn new_bare() -> Self {
        Self {
            page_table: PageTable::new(),
            vmas: Vec::new(),
        }
    }
    /// Get he page table token
    pub fn token(&self) -> usize {
        self.page_table.token()
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
    /// 根据起始虚拟页号删除一段已经登记的区域。
    pub fn remove_vma_with_start_vpn(&mut self, start_vpn: VirtPageNum) {
        if let Some((idx, area)) = self
            .vmas
            .iter_mut()
            .enumerate()
            .find(|(_, area)| area.start_vpn() == start_vpn)
        {
            area.teardown(&mut self.page_table);
            self.vmas.remove(idx);
            unsafe {
                asm!("sfence.vma");
            }
        }
    }
    /// 判断给定区间是否与当前地址空间中的任意区域重叠。
    pub fn overlaps_vma_range(&self, start_vpn: VirtPageNum, end_vpn: VirtPageNum) -> bool {
        self.vmas.iter().any(|vma| {
            let vma_start = vma.start_vpn();
            let vma_end = vma.end_vpn();
            start_vpn < vma_end && vma_start < end_vpn
        })
    }
    /// 按起始虚拟页号查找一段区域。
    pub fn find_vma(&self, start_vpn: VirtPageNum) -> Option<&Vma> {
        self.vmas.iter().find(|vma| vma.start_vpn() == start_vpn)
    }
    /// 按任意落点虚拟页查找所属区域。
    pub fn find_vma_containing(&self, vpn: VirtPageNum) -> Option<&Vma> {
        self.vmas.iter().find(|vma| vma.contains_vpn(vpn))
    }
    /// 按起始虚拟页号查找一段可变区域，供扩缩容等操作复用。
    pub fn find_vma_mut(&mut self, start_vpn: VirtPageNum) -> Option<&mut Vma> {
        self.vmas.iter_mut().find(|vma| vma.start_vpn() == start_vpn)
    }
    /// 按任意落点虚拟页查找可变区域。
    pub fn find_vma_containing_mut(&mut self, vpn: VirtPageNum) -> Option<&mut Vma> {
        self.vmas.iter_mut().find(|vma| vma.contains_vpn(vpn))
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
        self.vmas.push(vma);
        true
    }
    /// 在完成分裂、删除或追加后整理可合并的相邻区域。
    /// TODO：考虑使用有序数据结构维护vmas，避免每一次都需要排序。
    pub fn merge_adjacent_vmas(&mut self) {
        self.vmas.sort_by_key(|vma| vma.start_vpn().0);
        let mut idx = 0;
        while idx + 1 < self.vmas.len() {
            if self.vmas[idx].can_merge_with(&self.vmas[idx + 1]) {
                let right = self.vmas.remove(idx + 1);
                self.vmas[idx].absorb(right);
            } else {
                idx += 1;
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
    pub fn from_elf(elf_data: &[u8]) -> Result<(Self, UserSpaceLayout, usize), ()> {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map program headers of elf, with U flag
        let elf = xmas_elf::ElfFile::new(elf_data).map_err(|_| ())?;
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");
        let ph_count = elf_header.pt2.ph_count();
        let mut max_end_vpn = VirtPageNum(0);
        for i in 0..ph_count {
            let ph = elf.program_header(i).map_err(|_| ())?;
            if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
                let start_va: VirtAddr = (ph.virtual_addr() as usize).into();
                let end_va: VirtAddr = ((ph.virtual_addr() + ph.mem_size()) as usize).into();
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
        Ok((
            memory_set,
            layout,
            elf.header.pt2.entry_point() as usize,
        ))
    }
    /// Create a new address space by copy code&data from a exited process's address space.
    pub fn from_existed_user(user_space: &Self) -> Self {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // copy data sections/trap_context/user_stack
        for area in user_space.vmas.iter() {
            let new_area = area.clone_metadata();
            let _ = memory_set.insert_vma(new_area, None);
            // 仅复制当前地址空间真正拥有的私有页框。
            // 对于 file-backed MAP_SHARED，子进程后续通过缺页再次接入同一份 page cache。
            for vpn in area.data_frames.keys().copied() {
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
            }
        }
        memory_set
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

    ///Remove all VMAs
    pub fn recycle_data_pages(&mut self) {
        for area in self.vmas.iter_mut() {
            area.teardown(&mut self.page_table);
        }
        self.vmas.clear();
    }

    /// shrink the area to new_end
    #[allow(unused)]
    pub fn shrink_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        if let Some(idx) = self.vmas.iter().position(|vma| vma.start_vpn() == start.floor()) {
            self.vmas[idx].shrink_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }

    /// append the area to new_end
    #[allow(unused)]
    pub fn append_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        let new_end_vpn = new_end.ceil();
        let old_end_vpn = match self.find_vma(start.floor()) {
            Some(area) => area.end_vpn(),
            None => return false,
        };
        if self.overlaps_vma_range(old_end_vpn, new_end_vpn) {
            return false;
        }
        if let Some(idx) = self.vmas.iter().position(|vma| vma.start_vpn() == start.floor()) {
            self.vmas[idx].append_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
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

    /// 按给定用户区间拆除映射，支持匿名区域与文件映射区域。
    pub fn munmap(&mut self, start_va: VirtAddr, end_va: VirtAddr) -> bool {
        let start_vpn = start_va.floor();
        let end_vpn = end_va.ceil();
        debug!(
            "[munmap] begin teardown: start={:#x} end={:#x} start_vpn={:#x} end_vpn={:#x}",
            usize::from(start_va),
            usize::from(end_va),
            start_vpn.0,
            end_vpn.0
        );
        for vpn in VPNRange::new(start_vpn, end_vpn) {
            let Some(area) = self.find_vma_containing(vpn) else {
                return false;
            };
            if !area.is_user_accessible() {
                return false;
            }
        }

        let mut new_areas: Vec<Vma> = Vec::with_capacity(self.vmas.len() + 1);
        for mut area in self.vmas.drain(..) {
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

            debug!(
                "[munmap] overlap VMA: area_start={:#x} area_end={:#x} overlap_start={:#x} overlap_end={:#x} file_backed={} shared_pages={} private_pages={}",
                area_start.0,
                area_end.0,
                overlap_start.0,
                overlap_end.0,
                area.file.is_some(),
                area.shared_pages.len(),
                area.data_frames.len()
            );

            for vpn in VPNRange::new(overlap_start, overlap_end) {
                area.unmap_present_one(&mut self.page_table, vpn);
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
        self.vmas = new_areas;
        self.merge_adjacent_vmas();
        unsafe {
            asm!("sfence.vma");
        }
        debug!(
            "[munmap] complete teardown: start_vpn={:#x} end_vpn={:#x}",
            start_vpn.0,
            end_vpn.0
        );
        true
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
        let Some(idx) = self.vmas.iter().position(|vma| vma.contains_vpn(vpn)) else {
            return false;
        };
        let map_type = self.vmas[idx].map_type;
        let map_perm = self.vmas[idx].map_perm;
        let ppn = match map_type {
            MapType::Identical => PhysPageNum(vpn.0),
            MapType::Framed => {
                let frame = frame_alloc().unwrap();
                let ppn = frame.ppn;
                debug!(
                    "[mmap] allocate private frame for lazy fault: vpn={:#x} ppn={:#x}",
                    vpn.0,
                    ppn.0
                );
                self.vmas[idx].data_frames.insert(vpn, frame);
                ppn
            }
        };
        let pte_flags = PTEFlags::from_bits(map_perm.bits).unwrap();
        self.page_table.map(vpn, ppn, pte_flags);
        true
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
        let pte_flags = PTEFlags::from_bits(plan.map_perm.bits).unwrap();
        let ppn = page.lock().ppn();
        retain_mapped_page(&page);
        let area = self
            .find_vma_containing_mut(plan.vpn)
            .expect("validated file fault VMA disappeared");
        if let Some(old_page) = area.shared_pages.insert(plan.vpn, Arc::clone(&page)) {
            release_mapped_page(&old_page);
        }
        self.page_table.map(plan.vpn, ppn, pte_flags);
        unsafe {
            asm!("sfence.vma");
        }
        debug!(
            "[mmap] committed MAP_SHARED fault: vpn={:#x} page_idx={} ppn={:#x} path={:?}",
            plan.vpn.0,
            plan.page_idx,
            ppn.0,
            plan.file.path()
        );
        true
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
            "[mmap] committed MAP_PRIVATE fault: vpn={:#x} page_idx={} dst_ppn={:#x} path={:?}",
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
        for area in &self.vmas {
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

        // Now walk pages once, checking mapping, user PTE flag and that each page lies
        // within some user-accessible VMA range.
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
            // Page must be mapped and user-accessible in the page table.
            let Some(pte) = self.page_table.translate(vpn) else {
                return false;
            };
            if !pte.flags().contains(PTEFlags::U) {
                return false;
            }
        }

        // Modification: split VMAs as needed and update page table flags.
        let mut new_areas: Vec<Vma> = Vec::with_capacity(self.vmas.len() + 1);
        for mut area in self.vmas.drain(..) {
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
                let left_shared_pages = area.shared_pages.split_off(&overlap_start);
                let left_area = Vma {
                    vpn_range: VPNRange::new(area_start, overlap_start),
                    data_frames: area.data_frames,
                    map_type: area.map_type,
                    map_perm: area.map_perm,
                    kind: area.kind.clone(),
                    file: area.file.clone(),
                    shared_pages: area.shared_pages,
                };
                area.data_frames = left_data_frames;
                area.shared_pages = left_shared_pages;
                if let Some(file) = area.file.as_mut() {
                    file.pgoff += overlap_start.0 - area_start.0;
                }
                new_areas.push(left_area);
            }

            // right part exists -> split and handle middle separately
            if overlap_end < area_end {
                let right_data_frames = area.data_frames.split_off(&overlap_end);
                let right_shared_pages = area.shared_pages.split_off(&overlap_end);
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
                    shared_pages: right_shared_pages,
                };

                // update middle pages' PTE flags
                let pte_flags = PTEFlags::from_bits(permission.bits).unwrap();
                for vpn in VPNRange::new(overlap_start, overlap_end) {
                    self.page_table.update_flags(vpn, pte_flags);
                }

                area.vpn_range = VPNRange::new(overlap_start, overlap_end);
                area.map_perm = permission;
                new_areas.push(area);
                new_areas.push(right_area);
            } else {
                // no right split, area becomes the middle area
                let pte_flags = PTEFlags::from_bits(permission.bits).unwrap();
                for vpn in VPNRange::new(overlap_start, overlap_end) {
                    self.page_table.update_flags(vpn, pte_flags);
                }
                area.vpn_range = VPNRange::new(overlap_start, overlap_end);
                area.map_perm = permission;
                new_areas.push(area);
            }
        }

        self.vmas = new_areas;
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
}

/// 一段带有权限、来源和页框信息的虚拟内存区域描述。
pub struct Vma {
    /// 覆盖的虚拟页号半开区间。
    pub vpn_range: VPNRange,
    /// 对于 framed 映射，记录每个虚拟页对应的物理页框。
    pub data_frames: BTreeMap<VirtPageNum, FrameTracker>,
    /// 该区域采用的映射方式。
    pub map_type: MapType,
    /// 该区域在页表中的访问权限。
    pub map_perm: MapPermission,
    /// 该区域在地址空间中的用途标签。
    pub kind: VmaKind,
    /// 文件映射附带的底层对象信息；匿名区域为 `None`。
    pub file: Option<FileVma>,
    /// 当前直接映射到 page cache 的共享页。
    pub shared_pages: BTreeMap<VirtPageNum, Arc<SpinNoIrqLock<CachePage>>>,
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
            shared_pages: BTreeMap::new(),
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
            shared_pages: BTreeMap::new(),
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
        let right_shared_pages = self.shared_pages.split_off(&split_vpn);
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
            shared_pages: right_shared_pages,
        })
    }
    /// 按当前实际已经建立的映射状态拆除单页映射。
    pub fn unmap_present_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        if let Some(page) = self.shared_pages.remove(&vpn) {
            debug!(
                "[munmap] release shared page mapping: vpn={:#x}",
                vpn.0
            );
            if let Some(old_pte) = page_table.clear(vpn) {
                if old_pte.flags().contains(PTEFlags::D) {
                    mark_cached_page_dirty(&page);
                }
            }
            release_mapped_page(&page);
            return;
        }
        if self.map_type == MapType::Framed {
            debug!(
                "[munmap] release private frame mapping: vpn={:#x}",
                vpn.0
            );
            self.data_frames.remove(&vpn);
        }
        let _ = page_table.clear(vpn);
    }
    /// 依据当前区域实际映射状态拆除全部页表项。
    pub fn teardown(&mut self, page_table: &mut PageTable) {
        let shared_vpns: alloc::vec::Vec<_> = self.shared_pages.keys().copied().collect();
        for vpn in shared_vpns {
            self.unmap_present_one(page_table, vpn);
        }
        let framed_vpns: alloc::vec::Vec<_> = self.data_frames.keys().copied().collect();
        for vpn in framed_vpns {
            self.unmap_present_one(page_table, vpn);
        }
        if self.map_type == MapType::Identical {
            for vpn in self.vpn_range {
                let _ = page_table.clear(vpn);
            }
        }
    }
    /// 为指定虚拟页建立单页映射，并在需要时分配新的物理页框。
    pub fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        let ppn: PhysPageNum;
        match self.map_type {
            MapType::Identical => {
                ppn = PhysPageNum(vpn.0);
            }
            MapType::Framed => {
                let frame = frame_alloc().unwrap();
                ppn = frame.ppn;
                self.data_frames.insert(vpn, frame);
            }
        }
        let pte_flags = PTEFlags::from_bits(self.map_perm.bits).unwrap();
        page_table.map(vpn, ppn, pte_flags);
    }
    /// 撤销指定虚拟页的映射，并释放对应的页框记录。
    pub fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        if self.map_type == MapType::Framed {
            self.data_frames.remove(&vpn);
        }
        page_table.unmap(vpn);
    }
    /// 为当前区域覆盖的全部虚拟页建立映射。
    pub fn map(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.map_one(page_table, vpn);
        }
    }
    /// 撤销当前区域覆盖的全部虚拟页映射。
    pub fn unmap(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.unmap_one(page_table, vpn);
        }
    }
    #[allow(unused)]
    /// 将当前区域收缩到新的上界，并同步拆除尾部页映射。
    pub fn shrink_to(&mut self, page_table: &mut PageTable, new_end: VirtPageNum) {
        for vpn in VPNRange::new(new_end, self.vpn_range.get_end()) {
            self.unmap_one(page_table, vpn)
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
