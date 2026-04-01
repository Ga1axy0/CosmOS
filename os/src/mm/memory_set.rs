//! Address Space [`MemorySet`] management of Process

use super::{frame_alloc, FrameTracker};
use super::{PTEFlags, PageTable, PageTableEntry};
use super::{PhysAddr, PhysPageNum, VirtAddr, VirtPageNum};
use super::{StepByOne, VPNRange};
use crate::config::{MEMORY_END, MMIO, PAGE_SIZE, TRAMPOLINE, USER_MMAP_BASE, USER_STACK_BASE, USER_STACK_SIZE};
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
            area.unmap(&mut self.page_table);
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
    /// 按起始虚拟页号查找一段可变区域，供扩缩容等操作复用。
    pub fn find_vma_mut(&mut self, start_vpn: VirtPageNum) -> Option<&mut Vma> {
        self.vmas.iter_mut().find(|vma| vma.start_vpn() == start_vpn)
    }
    /// 将一段区域登记到地址空间并立即建立页表映射；若与现有区域冲突则失败。
    pub fn insert_vma(&mut self, mut vma: Vma, data: Option<&[u8]>) -> bool {
        if self.overlaps_vma_range(vma.start_vpn(), vma.end_vpn()) {
            return false;
        }
        vma.map(&mut self.page_table);
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
            // copy data from another space
            for vpn in area.vpn_range {
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
        if !self.insert_vma(Vma::new_anonymous(start_va, end_va, permission), None) {
            return false;
        }
        unsafe {
            asm!("sfence.vma");
        }
        true
    }

    /// unmap an anonymous area, return true if success
    pub fn munmap_anonymous(&mut self, start_va: VirtAddr, end_va: VirtAddr) -> bool {
        let start_vpn = start_va.floor();
        let end_vpn = end_va.ceil();
        for vpn in VPNRange::new(start_vpn, end_vpn) {
            let Some(pte) = self.page_table.translate(vpn) else {
                return false;
            };
            if !pte.flags().contains(PTEFlags::U) {
                return false;
            }
            if !self
                .vmas
                .iter()
                .any(|area| area.is_anonymous_framed_containing(vpn))
            {
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

            for vpn in VPNRange::new(overlap_start, overlap_end) {
                area.unmap_one(&mut self.page_table, vpn);
            }

            if area_start < overlap_start {
                let left_data_frames = area.data_frames.split_off(&overlap_start);
                let left_area = Vma {
                    vpn_range: VPNRange::new(area_start, overlap_start),
                    data_frames: area.data_frames,
                    map_type: area.map_type,
                    map_perm: area.map_perm,
                    kind: area.kind.clone(),
                };
                area.data_frames = left_data_frames;
                new_areas.push(left_area);
            }

            if overlap_end < area_end {
                let right_data_frames = area.data_frames.split_off(&overlap_end);
                let right_area = Vma {
                    vpn_range: VPNRange::new(overlap_end, area_end),
                    data_frames: right_data_frames,
                    map_type: area.map_type,
                    map_perm: area.map_perm,
                    kind: area.kind.clone(),
                };
                new_areas.push(right_area);
            }
        }
        self.vmas = new_areas;
        self.merge_adjacent_vmas();
        unsafe {
            asm!("sfence.vma");
        }
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
        for vpn in VPNRange::new(start_vpn, end_vpn) {
            let Some(pte) = self.page_table.translate(vpn) else {
                return false;
            };
            if !pte.flags().contains(PTEFlags::U) {
                return false;
            }
            if !self
                .vmas
                .iter()
                .any(|area| area.is_user_accessible() && area.contains_vpn(vpn))
            {
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
                let left_area = Vma {
                    vpn_range: VPNRange::new(area_start, overlap_start),
                    data_frames: area.data_frames,
                    map_type: area.map_type,
                    map_perm: area.map_perm,
                    kind: area.kind.clone(),
                };
                area.data_frames = left_data_frames;
                new_areas.push(left_area);
            }

            // right part exists -> split and handle middle separately
            if overlap_end < area_end {
                let right_data_frames = area.data_frames.split_off(&overlap_end);
                let right_area = Vma {
                    vpn_range: VPNRange::new(overlap_end, area_end),
                    data_frames: right_data_frames,
                    map_type: area.map_type,
                    map_perm: area.map_perm,
                    kind: area.kind.clone(),
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
    File {
        /// 文件映射在底层对象中的页对齐偏移。
        offset: usize,
    },
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
        offset: usize,
    ) -> Self {
        Self::new(
            start_va,
            end_va,
            MapType::Framed,
            map_perm,
            VmaKind::File { offset },
        )
    }
    /// 复制一份仅包含区间属性的区域元数据，不携带已有物理页分配结果。
    pub fn clone_metadata(&self) -> Self {
        Self {
            vpn_range: VPNRange::new(self.start_vpn(), self.end_vpn()),
            data_frames: BTreeMap::new(),
            map_type: self.map_type,
            map_perm: self.map_perm,
            kind: self.kind.clone(),
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
