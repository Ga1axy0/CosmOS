//! Implementation of [`PageTableEntry`] and [`PageTable`].
use super::{
    frame_alloc, frame_alloc_with_reclaim, FrameTracker, MmError, PhysAddr, PhysPageNum, StepByOne,
    VirtAddr, VirtPageNum, USER_SPACE_END,
};
use crate::config::PAGE_SIZE;
use crate::hal::traits::{AddressSpaceToken, PTEFlags, PagingArch};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
#[cfg(feature = "cosmos-meminfo")]
use core::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "cosmos-meminfo")]
static PAGE_TABLE_ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static PAGE_TABLE_FREE_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "cosmos-meminfo")]
static PAGE_TABLE_UNTRACKED_ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "cosmos-meminfo")]
#[derive(Clone, Copy, Debug, Default)]
/// Runtime counters for page-table frame ownership and permanent mappings.
pub struct PageTableStats {
    /// Tracked page-table frames allocated through an owned `PageTable`.
    pub alloc_calls: usize,
    /// Tracked page-table frames released when an owned `PageTable` is dropped.
    pub free_calls: usize,
    /// Page-table frames allocated without ownership tracking (kernel tables).
    pub untracked_alloc_calls: usize,
}

/// Reset page-table counters after the boot allocator has been initialized.
#[cfg(feature = "cosmos-meminfo")]
pub fn reset_page_table_stats() {
    PAGE_TABLE_ALLOC_CALLS.store(0, Ordering::Release);
    PAGE_TABLE_FREE_CALLS.store(0, Ordering::Release);
    PAGE_TABLE_UNTRACKED_ALLOC_CALLS.store(0, Ordering::Release);
}

/// Return page-table frame allocation counters.
#[cfg(feature = "cosmos-meminfo")]
pub fn page_table_stats() -> PageTableStats {
    PageTableStats {
        alloc_calls: PAGE_TABLE_ALLOC_CALLS.load(Ordering::Acquire),
        free_calls: PAGE_TABLE_FREE_CALLS.load(Ordering::Acquire),
        untracked_alloc_calls: PAGE_TABLE_UNTRACKED_ALLOC_CALLS.load(Ordering::Acquire),
    }
}

#[inline]
#[cfg(feature = "cosmos-meminfo")]
fn account_tracked_page_table_alloc() {
    PAGE_TABLE_ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
}

#[inline]
#[cfg(feature = "cosmos-meminfo")]
fn account_untracked_page_table_alloc() {
    PAGE_TABLE_UNTRACKED_ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
}

#[derive(Copy, Clone)]
#[repr(C)]
/// page table entry structure
pub struct PageTableEntry {
    /// bits of page table entry
    pub bits: usize,
}

impl PageTableEntry {
    /// Create a new page table entry
    pub fn new(ppn: PhysPageNum, flags: PTEFlags) -> Self {
        PageTableEntry {
            bits: crate::hal::make_pte(ppn.0, flags),
        }
    }
    /// Create an empty page table entry
    pub fn empty() -> Self {
        PageTableEntry { bits: 0 }
    }
    /// Get the physical page number from the page table entry
    pub fn ppn(&self) -> PhysPageNum {
        crate::hal::pte_ppn(self.bits).into()
    }
    /// Get the flags from the page table entry
    pub fn flags(&self) -> PTEFlags {
        crate::hal::pte_flags(self.bits)
    }
    /// The page pointered by page table entry is valid?
    pub fn is_valid(&self) -> bool {
        crate::hal::pte_is_valid(self.bits)
    }
    /// The page pointered by page table entry is readable?
    pub fn readable(&self) -> bool {
        (self.flags() & PTEFlags::R) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is writable?
    pub fn writable(&self) -> bool {
        (self.flags() & PTEFlags::W) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is executable?
    pub fn executable(&self) -> bool {
        (self.flags() & PTEFlags::X) != PTEFlags::empty()
    }
    /// 判断该页表项是否允许用户态访问。
    pub fn is_user(&self) -> bool {
        (self.flags() & PTEFlags::U) != PTEFlags::empty()
    }
}

/// Owned root frame of one hardware address space.
///
/// A hart may keep a process page table installed while it runs the idle
/// scheduler.  Keeping the root frame independently reference-counted lets the
/// process tear down or replace its `MemorySet` without invalidating the
/// hardware page-table root still loaded by that hart.
struct PageTableRootFrame {
    _frame: FrameTracker,
}

impl Drop for PageTableRootFrame {
    fn drop(&mut self) {
        #[cfg(feature = "cosmos-meminfo")]
        PAGE_TABLE_FREE_CALLS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Strong lifetime guard for a hardware page-table root and its token.
///
/// The guard intentionally pins only the root frame.  While a hart is idle it
/// executes solely through permanent kernel mappings: Sv39 process roots share
/// the kernel-half directory frames with `KERNEL_SPACE`, and LoongArch kernel
/// execution uses DMW mappings plus the shared kernel-heap root entry. User
/// page-table descendants may therefore be reclaimed after exit/exec while the
/// old root remains borrowed by idle.
#[derive(Clone)]
pub struct AddressSpaceRoot {
    token: AddressSpaceToken,
    _root: Arc<PageTableRootFrame>,
}

impl AddressSpaceRoot {
    /// Hardware token selecting this root.
    #[inline]
    pub fn token(&self) -> AddressSpaceToken {
        self.token
    }
}

/// page table structure
pub struct PageTable {
    root_ppn: PhysPageNum,
    root_frame: Option<Arc<PageTableRootFrame>>,
    frames: Vec<FrameTracker>,
}

impl PageTable {
    /// Construct a page table directly in its final early-boot destination.
    ///
    /// This avoids passing a `PageTable` aggregate by value before the normal
    /// kernel address space and allocator environment are fully established.
    pub(crate) unsafe fn init_new_at(output: *mut Self) -> Result<(), MmError> {
        let frame = frame_alloc_with_reclaim().ok_or(MmError::OutOfMemory)?;
        #[cfg(feature = "cosmos-meminfo")]
        account_tracked_page_table_alloc();
        let root_ppn = frame.ppn;

        let root_frame = Arc::new(PageTableRootFrame { _frame: frame });

        core::ptr::addr_of_mut!((*output).root_ppn).write(root_ppn);
        core::ptr::addr_of_mut!((*output).root_frame).write(Some(root_frame));
        core::ptr::addr_of_mut!((*output).frames).write(Vec::new());
        Ok(())
    }

    /// Create a new page table
    pub fn new() -> Result<Self, MmError> {
        let frame = frame_alloc_with_reclaim().ok_or(MmError::OutOfMemory)?;
        #[cfg(feature = "cosmos-meminfo")]
        account_tracked_page_table_alloc();
        let root_ppn = frame.ppn;
        let root_frame = Arc::new(PageTableRootFrame { _frame: frame });
        Ok(PageTable {
            root_ppn,
            root_frame: Some(root_frame),
            frames: Vec::new(),
        })
    }

    /// Create a view of an existing owned root without allocating another
    /// hardware page-table root.  The root guard keeps the parent root alive
    /// while the view is installed on a hart.
    pub(crate) fn borrowed_from(other: &PageTable) -> Self {
        debug_assert!(
            other.root_frame.is_some(),
            "a borrowed page table must originate from an owned root"
        );
        Self {
            root_ppn: other.root_ppn,
            root_frame: other.root_frame.as_ref().map(Arc::clone),
            frames: Vec::new(),
        }
    }

    /// Share the complete Sv39 kernel half from the permanent kernel page
    /// table. Only this root frame is process-owned; the referenced kernel
    /// directory frames live for the lifetime of `KERNEL_SPACE`.
    #[cfg(target_arch = "riscv64")]
    pub fn share_kernel_half_from(&mut self, kernel: &PageTable) {
        let entry_count = 1usize << crate::hal::page_table_index_bits();
        let kernel_start = entry_count / 2;
        let dst = self.root_ppn.get_pte_array();
        let src = kernel.root_ppn.get_pte_array();
        dst[kernel_start..entry_count].copy_from_slice(&src[kernel_start..entry_count]);
    }

    /// Mark every present Sv39 kernel root entry global. A global non-leaf
    /// entry makes every translation below it global as well.
    #[cfg(target_arch = "riscv64")]
    pub fn mark_kernel_half_global(&mut self) {
        let entry_count = 1usize << crate::hal::page_table_index_bits();
        for pte in &mut self.root_ppn.get_pte_array()[entry_count / 2..entry_count] {
            if pte.is_valid() {
                pte.bits |= PTEFlags::G.bits() as usize;
            }
        }
    }

    /// Share the low-address kernel heap subtree with a LoongArch user root.
    ///
    /// LoongArch kernel text, data and stacks use DMW mappings, but the
    /// growable kernel heap lives in a low virtual-address window and is
    /// backed by the kernel page table. Keep that one root entry shared so
    /// the kernel can retain the current process PGDL across user traps.
    #[cfg(target_arch = "loongarch64")]
    pub fn share_kernel_heap_from(&mut self, kernel: &PageTable) {
        let heap_vpn = VirtAddr::from(crate::config::KERNEL_HEAP_BASE).floor();
        let root_index = crate::hal::vpn_index(heap_vpn.0, 0);
        let kernel_entry = kernel.root_ppn.get_pte_array()[root_index];
        debug_assert!(
            kernel_entry.is_valid(),
            "kernel heap root entry must be initialized before user roots"
        );
        debug_assert!(
            !self.root_ppn.get_pte_array()[root_index].is_valid(),
            "user root unexpectedly owns the kernel heap root entry"
        );
        self.root_ppn.get_pte_array()[root_index] = kernel_entry;
    }

    /// Temporarily used to get arguments from user space.
    pub fn from_token(token: AddressSpaceToken) -> Self {
        Self {
            root_ppn: PhysPageNum::from(crate::hal::root_ppn_from_token(token)),
            root_frame: None,
            frames: Vec::new(),
        }
    }
    fn find_pte_create(
        &mut self,
        vpn: VirtPageNum,
    ) -> Result<Option<&mut PageTableEntry>, MmError> {
        let levels = crate::hal::page_table_levels();
        let mut ppn = self.root_ppn;
        for level in 0..levels {
            let idx = crate::hal::vpn_index(vpn.0, level);
            let pte = &mut ppn.get_pte_array()[idx];
            if level + 1 == levels {
                return Ok(Some(pte));
            }
            if !pte.is_valid() {
                let frame = frame_alloc_with_reclaim().ok_or(MmError::OutOfMemory)?;
                #[cfg(feature = "cosmos-meminfo")]
                account_tracked_page_table_alloc();
                pte.bits = crate::hal::make_dir_entry(frame.ppn.0);
                self.frames.push(frame);
            }
            ppn = pte.ppn();
        }
        Ok(None)
    }
    fn find_pte_create_untracked(
        &mut self,
        vpn: VirtPageNum,
    ) -> Result<Option<&mut PageTableEntry>, MmError> {
        let levels = crate::hal::page_table_levels();
        let mut ppn = self.root_ppn;
        for level in 0..levels {
            let idx = crate::hal::vpn_index(vpn.0, level);
            let pte = &mut ppn.get_pte_array()[idx];
            if level + 1 == levels {
                return Ok(Some(pte));
            }
            if !pte.is_valid() {
                let frame = frame_alloc_with_reclaim().ok_or(MmError::OutOfMemory)?;
                #[cfg(feature = "cosmos-meminfo")]
                account_untracked_page_table_alloc();
                pte.bits = crate::hal::make_dir_entry(frame.ppn.0);
                core::mem::forget(frame);
            }
            ppn = pte.ppn();
        }
        Ok(None)
    }
    fn find_pte(&self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let levels = crate::hal::page_table_levels();
        let mut ppn = self.root_ppn;
        for level in 0..levels {
            let idx = crate::hal::vpn_index(vpn.0, level);
            let pte = &mut ppn.get_pte_array()[idx];
            if level + 1 == levels {
                return Some(pte);
            }
            if !pte.is_valid() {
                return None;
            }
            ppn = pte.ppn();
        }
        None
    }

    /// Return the leaf table covering `vpn`, assuming all intermediate levels
    /// have already been created.
    fn find_leaf_table(&self, vpn: VirtPageNum) -> Option<PhysPageNum> {
        let levels = crate::hal::page_table_levels();
        let mut ppn = self.root_ppn;
        for level in 0..levels.saturating_sub(1) {
            let idx = crate::hal::vpn_index(vpn.0, level);
            let pte = &ppn.get_pte_array()[idx];
            if !pte.is_valid() {
                return None;
            }
            ppn = pte.ppn();
        }
        Some(ppn)
    }
    /// set the map between virtual page number and physical page number
    #[allow(unused)]
    pub fn map(
        &mut self,
        vpn: VirtPageNum,
        ppn: PhysPageNum,
        flags: PTEFlags,
    ) -> Result<(), MmError> {
        let pte = self.find_pte_create(vpn)?.ok_or(MmError::NoMapping)?;
        debug_assert!(!pte.is_valid(), "vpn {:?} is mapped before mapping", vpn);
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
        Ok(())
    }
    /// Ensure that the leaf page-table slot for `vpn` exists.
    ///
    /// This is used by virtual-address relocation paths to preflight all
    /// intermediate page-table allocations before changing the old mapping.
    pub fn ensure_leaf(&mut self, vpn: VirtPageNum) -> Result<(), MmError> {
        self.find_pte_create(vpn)?
            .ok_or(MmError::NoMapping)
            .map(|_| ())
    }
    /// Map one leaf whose intermediate page-table levels were preallocated by
    /// [`Self::ensure_leaf`].  Batch mapping paths use this after completing
    /// all fallible allocation work, avoiding a second create-mode walk.
    pub fn map_preallocated_leaf(
        &mut self,
        vpn: VirtPageNum,
        ppn: PhysPageNum,
        flags: PTEFlags,
    ) -> Result<(), MmError> {
        let pte = self.find_pte(vpn).ok_or(MmError::NoMapping)?;
        if pte.is_valid() {
            return Err(MmError::Conflict);
        }
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
        Ok(())
    }

    /// Map a consecutive run of already allocated physical pages.
    ///
    /// Intermediate page-table pages are allocated once per leaf table, then
    /// the leaf PTEs are written directly.  This is the hot path used by
    /// fork's COW inheritance, where a VMA commonly contains many adjacent
    /// resident pages.
    pub(crate) fn map_preallocated_range(
        &mut self,
        start_vpn: VirtPageNum,
        entries: &[(PhysPageNum, PTEFlags)],
    ) -> Result<(), MmError> {
        if entries.is_empty() {
            return Ok(());
        }
        let end = start_vpn
            .0
            .checked_add(entries.len())
            .ok_or(MmError::InvalidRange)?;
        let leaf_span = 1usize << crate::hal::page_table_index_bits();

        // Complete all fallible intermediate allocations before touching leaf
        // PTEs.  A leaf table covers `leaf_span` consecutive virtual pages.
        let mut previous_leaf_base = None;
        for vpn_index in 0..entries.len() {
            let vpn = VirtPageNum(start_vpn.0 + vpn_index);
            let leaf_base = vpn.0 & !(leaf_span - 1);
            if previous_leaf_base != Some(leaf_base) {
                self.ensure_leaf(vpn)?;
                previous_leaf_base = Some(leaf_base);
            }
        }

        let mut offset = 0usize;
        while offset < entries.len() {
            let vpn = VirtPageNum(start_vpn.0 + offset);
            let leaf_ppn = self.find_leaf_table(vpn).ok_or(MmError::NoMapping)?;
            let leaf_index = crate::hal::vpn_index(
                vpn.0,
                crate::hal::page_table_levels().saturating_sub(1),
            );
            let chunk_len = core::cmp::min(entries.len() - offset, leaf_span - leaf_index);
            let leaf_entries = &mut leaf_ppn.get_pte_array()[leaf_index..leaf_index + chunk_len];
            for (pte, (ppn, flags)) in leaf_entries
                .iter_mut()
                .zip(entries[offset..offset + chunk_len].iter())
            {
                if pte.is_valid() {
                    return Err(MmError::Conflict);
                }
                *pte = PageTableEntry::new(*ppn, *flags | PTEFlags::V);
            }
            offset += chunk_len;
        }
        let _ = end;
        Ok(())
    }

    /// Update flags for a consecutive run without repeating a full page-table
    /// walk for every page.
    pub(crate) fn update_flags_range(
        &mut self,
        start_vpn: VirtPageNum,
        len: usize,
        flags: PTEFlags,
    ) -> Result<(), MmError> {
        if len == 0 {
            return Ok(());
        }
        let _end = start_vpn
            .0
            .checked_add(len)
            .ok_or(MmError::InvalidRange)?;
        let leaf_span = 1usize << crate::hal::page_table_index_bits();
        let mut offset = 0usize;
        while offset < len {
            let vpn = VirtPageNum(start_vpn.0 + offset);
            let leaf_ppn = self.find_leaf_table(vpn).ok_or(MmError::NoMapping)?;
            let leaf_index = crate::hal::vpn_index(
                vpn.0,
                crate::hal::page_table_levels().saturating_sub(1),
            );
            let chunk_len = core::cmp::min(len - offset, leaf_span - leaf_index);
            let leaf_entries = &mut leaf_ppn.get_pte_array()[leaf_index..leaf_index + chunk_len];
            for pte in leaf_entries.iter_mut() {
                if !pte.is_valid() {
                    return Err(MmError::NoMapping);
                }
                let ppn = pte.ppn();
                *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
            }
            offset += chunk_len;
        }
        Ok(())
    }

    /// Move descendants allocated by a borrowed view back to its owning
    /// address space before the view is dropped.
    pub(crate) fn take_owned_frames(&mut self) -> Vec<FrameTracker> {
        core::mem::take(&mut self.frames)
    }

    /// Adopt page-table descendants transferred from a shared view.
    pub(crate) fn append_owned_frames(&mut self, mut frames: Vec<FrameTracker>) {
        self.frames.append(&mut frames);
    }
    /// Map a permanent kernel page without recording page-table frames in `frames`.
    pub fn map_kernel_untracked(
        &mut self,
        vpn: VirtPageNum,
        ppn: PhysPageNum,
        flags: PTEFlags,
    ) -> Result<(), MmError> {
        let pte = self
            .find_pte_create_untracked(vpn)?
            .ok_or(MmError::NoMapping)?;
        debug_assert!(!pte.is_valid(), "vpn {:?} is mapped before mapping", vpn);
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
        Ok(())
    }
    /// Ensure the first-level subtree table under the root covering `vpn`
    /// exists, creating it untracked if necessary, and return its physical page
    /// number.
    ///
    /// Used to pre-build the kernel-heap window's root-entry subtree once at
    /// boot so that subsequent heap growth can install leaf PTEs into a
    /// disjoint subtree without re-walking (and re-locking) the global kernel
    /// page table.
    pub fn ensure_subtree_root_untracked(&mut self, vpn: VirtPageNum) -> PhysPageNum {
        debug_assert!(
            crate::hal::page_table_levels() >= 2,
            "kernel heap subtree caching requires a multi-level page table"
        );
        let idx = crate::hal::vpn_index(vpn.0, 0);
        let pte = &mut self.root_ppn.get_pte_array()[idx];
        if !pte.is_valid() {
            let frame = frame_alloc().unwrap();
            #[cfg(feature = "cosmos-meminfo")]
            account_untracked_page_table_alloc();
            pte.bits = crate::hal::make_dir_entry(frame.ppn.0);
            core::mem::forget(frame);
        }
        #[cfg(target_arch = "riscv64")]
        {
            pte.bits |= PTEFlags::G.bits() as usize;
        }
        pte.ppn()
    }
    /// remove the map between virtual page number and physical page number
    #[allow(unused)]
    pub fn unmap(&mut self, vpn: VirtPageNum) {
        let pte = self.find_pte(vpn).unwrap();
        debug_assert!(pte.is_valid(), "vpn {:?} is invalid before unmapping", vpn);
        *pte = PageTableEntry::empty();
    }
    /// 清除一个已经存在的页表项，并返回旧值；若原本未映射则返回 `None`。
    pub fn clear(&mut self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        let pte = self.find_pte(vpn)?;
        if !pte.is_valid() {
            return None;
        }
        let old = *pte;
        *pte = PageTableEntry::empty();
        Some(old)
    }
    /// 仅更新一个已经存在页表项的权限位，保持物理页号不变。
    pub fn update_flags(&mut self, vpn: VirtPageNum, flags: PTEFlags) -> bool {
        let pte = match self.find_pte(vpn) {
            Some(pte) if pte.is_valid() => pte,
            _ => return false,
        };
        let ppn = pte.ppn();
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
        true
    }
    /// 用新的物理页号和权限替换一个已经存在的页表项。
    pub fn replace(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) -> bool {
        let pte = match self.find_pte(vpn) {
            Some(pte) if pte.is_valid() => pte,
            _ => return false,
        };
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
        true
    }
    /// get the page table entry from the virtual page number
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.find_pte(vpn)
            .and_then(|pte| if pte.is_valid() { Some(*pte) } else { None })
    }
    /// get the physical address from the virtual address
    pub fn translate_va(&self, va: VirtAddr) -> Option<PhysAddr> {
        self.translate(va.floor()).map(|pte| {
            let aligned_pa: PhysAddr = pte.ppn().into();
            let offset = va.page_offset();
            let aligned_pa_usize: usize = aligned_pa.into();
            (aligned_pa_usize + offset).into()
        })
    }
    /// get the token from the page table
    pub fn token(&self) -> AddressSpaceToken {
        crate::hal::make_address_space_token(self.root_ppn.0)
    }

    /// Pin this owned root while a hart may retain it in the hardware walker.
    pub fn address_space_root(&self, token: AddressSpaceToken) -> AddressSpaceRoot {
        AddressSpaceRoot {
            token,
            _root: Arc::clone(
                self.root_frame
                    .as_ref()
                    .expect("cannot pin a borrowed page-table token"),
            ),
        }
    }
}

impl Drop for PageTable {
    fn drop(&mut self) {
        #[cfg(feature = "cosmos-meminfo")]
        {
            // The independently reference-counted root accounts for its own
            // release when the final active-address-space guard disappears.
            PAGE_TABLE_FREE_CALLS.fetch_add(self.frames.len(), Ordering::Relaxed);
        }
    }
}

fn checked_user_va(va: usize) -> Option<VirtAddr> {
    (va < USER_SPACE_END).then_some(VirtAddr(va))
}

fn checked_user_range(start: usize, len: usize) -> Option<usize> {
    if len == 0 {
        return Some(start);
    }
    if start >= USER_SPACE_END {
        return None;
    }
    let end = start.checked_add(len)?;
    (end <= USER_SPACE_END).then_some(end)
}

/// Create mutable `Vec<u8>` slice in kernel space from ptr in other address space. NOTICE: the content pointed to by the pointer `ptr` can cross physical pages.
pub fn translated_byte_buffer(
    token: AddressSpaceToken,
    ptr: *const u8,
    len: usize,
) -> Option<Vec<&'static mut [u8]>> {
    let page_table = PageTable::from_token(token);
    let mut start = ptr as usize;
    let end = checked_user_range(start, len)?;
    let mut v = Vec::new();
    while start < end {
        let start_va = checked_user_va(start)?;
        let mut vpn = start_va.floor();
        if let Some(ppn) = page_table.translate(vpn).map(|pte| pte.ppn()) {
            vpn.step();
            let chunk_end = VirtAddr::from(vpn).0.min(end);
            if chunk_end % PAGE_SIZE == 0 {
                v.push(&mut ppn.get_bytes_array()[start_va.page_offset()..]);
            } else {
                v.push(&mut ppn.get_bytes_array()[start_va.page_offset()..(chunk_end % PAGE_SIZE)]);
            }
            start = chunk_end;
        } else {
            return None;
        }
    }
    Some(v)
}

/// Create String in kernel address space from u8 Array(end with 0) in other address space
pub fn translated_str(token: AddressSpaceToken, ptr: *const u8) -> Option<String> {
    let page_table = PageTable::from_token(token);
    let mut string = String::new();
    let mut va = ptr as usize;
    loop {
        let pa = match checked_user_va(va).and_then(|va| page_table.translate_va(va)) {
            Some(pa) => pa,
            None => return None,
        };
        let ch: u8 = *pa.get_mut();
        if ch == 0 {
            break;
        }
        string.push(ch as char);
        va = va.checked_add(1)?;
        if va >= USER_SPACE_END {
            return None;
        }
    }
    Some(string)
}

/// translate a pointer `ptr` in other address space to a immutable u8 slice in kernel address space. NOTICE: the content pointed to by the pointer `ptr` cannot cross physical pages, otherwise translated_byte_buffer should be used.
pub fn translated_ref<T>(token: AddressSpaceToken, ptr: *const T) -> Option<&'static T> {
    let page_table = PageTable::from_token(token);
    checked_user_range(ptr as usize, core::mem::size_of::<T>().max(1))?;
    page_table
        .translate_va(VirtAddr(ptr as usize))
        .map(|pa| pa.get_ref())
}

/// translate a pointer `ptr` in other address space to a mutable u8 slice in kernel address space. NOTICE: the content pointed to by the pointer `ptr` cannot cross physical pages, otherwise translated_byte_buffer should be used.
pub fn translated_refmut<T>(token: AddressSpaceToken, ptr: *mut T) -> Option<&'static mut T> {
    let page_table = PageTable::from_token(token);
    let va = ptr as usize;
    checked_user_range(va, core::mem::size_of::<T>().max(1))?;
    page_table.translate_va(VirtAddr(va)).map(|pa| pa.get_mut())
}

/// An abstraction over a buffer passed from user space to kernel space
pub struct UserBuffer {
    /// A list of buffers
    pub buffers: Vec<&'static mut [u8]>,
}

impl UserBuffer {
    /// Constuct UserBuffer
    pub fn new(buffers: Vec<&'static mut [u8]>) -> Self {
        Self { buffers }
    }
    /// Get the length of the buffer
    pub fn len(&self) -> usize {
        let mut total: usize = 0;
        for b in self.buffers.iter() {
            total += b.len();
        }
        total
    }
}

impl IntoIterator for UserBuffer {
    type Item = *mut u8;
    type IntoIter = UserBufferIterator;
    fn into_iter(self) -> Self::IntoIter {
        UserBufferIterator {
            buffers: self.buffers,
            current_buffer: 0,
            current_idx: 0,
        }
    }
}

/// An iterator over a UserBuffer
pub struct UserBufferIterator {
    buffers: Vec<&'static mut [u8]>,
    current_buffer: usize,
    current_idx: usize,
}

impl Iterator for UserBufferIterator {
    type Item = *mut u8;
    fn next(&mut self) -> Option<Self::Item> {
        if self.current_buffer >= self.buffers.len() {
            None
        } else {
            let r = &mut self.buffers[self.current_buffer][self.current_idx] as *mut _;
            if self.current_idx + 1 == self.buffers[self.current_buffer].len() {
                self.current_idx = 0;
                self.current_buffer += 1;
            } else {
                self.current_idx += 1;
            }
            Some(r)
        }
    }
}
