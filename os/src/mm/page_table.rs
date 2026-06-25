//! Implementation of [`PageTableEntry`] and [`PageTable`].
use super::{
    frame_alloc, frame_alloc_with_reclaim, FrameTracker, MmError, PhysAddr, PhysPageNum, StepByOne,
    VirtAddr, VirtPageNum, USER_SPACE_END,
};
use crate::config::PAGE_SIZE;
use crate::hal::traits::{AddressSpaceToken, PTEFlags, PagingArch};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

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

/// page table structure
pub struct PageTable {
    root_ppn: PhysPageNum,
    frames: Vec<FrameTracker>,
}

impl PageTable {
    /// Create a new page table
    pub fn new() -> Result<Self, MmError> {
        let frame = frame_alloc_with_reclaim().ok_or(MmError::OutOfMemory)?;
        Ok(PageTable {
            root_ppn: frame.ppn,
            frames: vec![frame],
        })
    }
    /// Temporarily used to get arguments from user space.
    pub fn from_token(token: AddressSpaceToken) -> Self {
        Self {
            root_ppn: PhysPageNum::from(crate::hal::root_ppn_from_token(token)),
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
            pte.bits = crate::hal::make_dir_entry(frame.ppn.0);
            core::mem::forget(frame);
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
