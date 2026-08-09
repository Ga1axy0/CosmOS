//! ELF image parsing and loading into a [`MemorySet`].

use super::memory_set::{MapPermission, MemorySet, Vma};
use super::{MmError, VirtAddr, VirtPageNum};
use crate::config::{PAGE_SIZE, USER_PIE_BASE};
use crate::fs::{AccessMode, File, FileDescription, FileStatusFlags, OSInode};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::min;

/// ELF 加载结果，包含动态链接所需的额外信息。
pub struct ElfLoadInfo {
    /// 程序入口点。
    pub entry_point: usize,
    /// 程序头表在内存中的地址（用于 AT_PHDR）。
    pub phdr_vaddr: usize,
    /// 程序头表项大小（用于 AT_PHENT）。
    pub phent_size: usize,
    /// 程序头表项数量（用于 AT_PHNUM）。
    pub phnum: usize,
    /// 动态链接器路径（如果存在 INTERP 段）。
    pub interp_path: Option<String>,
}

/// The result of loading one ELF image into an existing address space.
pub(crate) struct LoadedElf {
    pub(crate) info: ElfLoadInfo,
    pub(crate) image_end: usize,
}

/// Load ELF images into an existing address space.
pub(crate) struct ElfLoader<'a> {
    memory_set: &'a mut MemorySet,
}

/// `R_RISCV_RELATIVE` relocation used by static PIE executables.
const R_RISCV_RELATIVE: u32 = 3;
/// Keep ELF metadata and the streaming I/O buffer bounded.  The transition
/// loader below deliberately never allocates a buffer proportional to the
/// executable size.
const ELF_METADATA_MAX_SIZE: usize = 1024 * 1024;
const ELF_IO_CHUNK_SIZE: usize = 16 * 1024;

impl<'a> ElfLoader<'a> {
    /// Create a loader targeting an existing address space.
    pub(crate) fn new(memory_set: &'a mut MemorySet) -> Self {
        Self { memory_set }
    }

    /// Load the main executable from an inode.
    ///
    /// Static PIE keeps the existing full-image path because relocation
    /// processing currently needs the dynamic relocation bytes as one slice.
    /// A directly executed runtime linker is also `ET_DYN` without
    /// `PT_INTERP`, so it uses the same eager mapping path, but it does not
    /// carry `DF_1_PIE` and must be allowed to perform its own bootstrap
    /// relocations. Other images use the bounded streaming path.
    pub(crate) fn load_file(&mut self, file: &Arc<OSInode>) -> Result<LoadedElf, MmError> {
        let metadata = read_elf_metadata(file)?;
        let elf = xmas_elf::ElfFile::new(&metadata).map_err(|_| MmError::InvalidElf)?;
        if is_et_dyn_without_interp(&elf)? {
            let elf_data = file.read_all();
            return self.load_bytes(&elf_data);
        }
        self.load_file_at(file, None)
    }

    /// Load one in-memory ELF image into the target address space.
    pub(crate) fn load_bytes(&mut self, elf_data: &[u8]) -> Result<LoadedElf, MmError> {
        let elf = xmas_elf::ElfFile::new(elf_data).map_err(|_| MmError::InvalidElf)?;
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");
        let elf_type = elf_header.pt2.type_().as_type();
        let load_bias = if elf_type == xmas_elf::header::Type::SharedObject {
            USER_PIE_BASE
        } else {
            0
        };
        let ph_count = elf_header.pt2.ph_count();
        let mut max_end_vpn = VirtPageNum(0);

        // 收集动态链接信息。
        let mut interp_path: Option<String> = None;
        let phdr_vaddr = elf_header.pt2.ph_offset() as usize;
        let mut phdr_load_vaddr: Option<usize> = None;

        for i in 0..ph_count {
            let ph = elf.program_header(i).map_err(|_| MmError::InvalidElf)?;
            let ph_type = ph.get_type().map_err(|_| MmError::InvalidElf)?;

            // 检查 INTERP 段。
            if ph_type == xmas_elf::program::Type::Interp {
                debug!("Found INTERP segment in ELF program header");
                let offset = ph.offset() as usize;
                let size = ph.file_size() as usize;
                if size > 0 && offset + size <= elf_data.len() {
                    let interp_bytes = &elf_data[offset..offset + size];
                    // INTERP 段内容是以 null 结尾的字符串。
                    let end = interp_bytes
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(interp_bytes.len());
                    if let Ok(path) = core::str::from_utf8(&interp_bytes[..end]) {
                        interp_path = Some(String::from(path));
                        debug!("Found INTERP segment: {}", path);
                    }
                }
            }

            if ph_type == xmas_elf::program::Type::Load {
                let start_va: VirtAddr =
                    add_load_bias(ph.virtual_addr() as usize, load_bias)?.into();
                let end_va: VirtAddr =
                    add_load_bias((ph.virtual_addr() + ph.mem_size()) as usize, load_bias)?.into();

                // 检查程序头表是否在这个 LOAD 段内。
                if phdr_load_vaddr.is_none() {
                    let seg_file_start = ph.offset() as usize;
                    let seg_file_end = seg_file_start + ph.file_size() as usize;
                    if phdr_vaddr >= seg_file_start && phdr_vaddr < seg_file_end {
                        // 程序头表在此段内，计算其虚拟地址。
                        let offset_in_seg = phdr_vaddr - seg_file_start;
                        phdr_load_vaddr = Some(add_load_bias(
                            ph.virtual_addr() as usize + offset_in_seg,
                            load_bias,
                        )?);
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
                debug!(
                    "mapping ELF segment: [{:#x}, {:#x}) with flags {:?}",
                    &(usize::from(start_va)),
                    &(usize::from(end_va)),
                    map_perm
                );
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
                self.memory_set.insert_vma(vma, Some(seg_data))?;
            }
        }
        if needs_kernel_static_pie_relocations(&elf)? {
            apply_static_pie_relocations(self.memory_set, &elf, load_bias)?;
        }
        let max_end_va: VirtAddr = max_end_vpn.into();
        let info = ElfLoadInfo {
            entry_point: add_load_bias(elf.header.pt2.entry_point() as usize, load_bias)?,
            phdr_vaddr: phdr_load_vaddr.unwrap_or(0),
            phent_size: elf.header.pt2.ph_entry_size() as usize,
            phnum: ph_count as usize,
            interp_path,
        };

        Ok(LoadedElf {
            info,
            image_end: usize::from(max_end_va),
        })
    }

    /// Load one ELF file into an existing address space. `forced_load_bias`
    /// is used for the dynamic linker; `None` selects the normal
    /// ET_EXEC/PIE policy used by the main executable.
    ///
    /// Complete interior file pages are registered as private file mappings
    /// and are populated by the normal page-fault/page-cache path. At most
    /// the first and last partial file pages are copied while constructing the
    /// address space. The zero-filled tail is represented by anonymous lazy
    /// mappings.
    pub(crate) fn load_file_at(
        &mut self,
        file: &Arc<OSInode>,
        forced_load_bias: Option<usize>,
    ) -> Result<LoadedElf, MmError> {
        let metadata = read_elf_metadata(file)?;
        let elf = xmas_elf::ElfFile::new(&metadata).map_err(|_| MmError::InvalidElf)?;
        let elf_type = elf.header.pt2.type_().as_type();
        let load_bias =
            forced_load_bias.unwrap_or(if elf_type == xmas_elf::header::Type::SharedObject {
                USER_PIE_BASE
            } else {
                0
            });
        let ph_count = elf.header.pt2.ph_count();
        let phdr_file_offset = elf.header.pt2.ph_offset() as usize;
        let mut max_end_vpn = VirtPageNum(0);
        let mut interp_path = None;
        let mut phdr_load_vaddr = None;
        let file_description = elf_file_description(file);

        for index in 0..ph_count {
            let ph = elf.program_header(index).map_err(|_| MmError::InvalidElf)?;
            let ph_type = ph.get_type().map_err(|_| MmError::InvalidElf)?;

            if ph_type == xmas_elf::program::Type::Interp {
                let offset = usize::try_from(ph.offset()).map_err(|_| MmError::InvalidElf)?;
                let size = usize::try_from(ph.file_size()).map_err(|_| MmError::InvalidElf)?;
                if size == 0 || size > 4096 {
                    return Err(MmError::InvalidElf);
                }
                let mut interp_data = alloc::vec![0u8; size];
                read_file_exact(file, offset, &mut interp_data)?;
                let end = interp_data
                    .iter()
                    .position(|&byte| byte == 0)
                    .unwrap_or(interp_data.len());
                let path =
                    core::str::from_utf8(&interp_data[..end]).map_err(|_| MmError::InvalidElf)?;
                if path.is_empty() {
                    return Err(MmError::InvalidElf);
                }
                interp_path = Some(String::from(path));
            }

            if ph_type != xmas_elf::program::Type::Load {
                continue;
            }

            let virtual_addr =
                usize::try_from(ph.virtual_addr()).map_err(|_| MmError::InvalidElf)?;
            let memory_size = usize::try_from(ph.mem_size()).map_err(|_| MmError::InvalidElf)?;
            let file_size = usize::try_from(ph.file_size()).map_err(|_| MmError::InvalidElf)?;
            let file_offset = usize::try_from(ph.offset()).map_err(|_| MmError::InvalidElf)?;
            if file_size > memory_size {
                return Err(MmError::InvalidElf);
            }
            if virtual_addr & (PAGE_SIZE - 1) != file_offset & (PAGE_SIZE - 1) {
                return Err(MmError::InvalidElf);
            }
            let segment_end = virtual_addr
                .checked_add(memory_size)
                .ok_or(MmError::InvalidElf)?;
            let start_va: VirtAddr = add_load_bias(virtual_addr, load_bias)?.into();
            let end_va: VirtAddr = add_load_bias(segment_end, load_bias)?.into();

            if memory_size == 0 {
                continue;
            }

            if phdr_load_vaddr.is_none() {
                let file_end = file_offset
                    .checked_add(file_size)
                    .ok_or(MmError::InvalidElf)?;
                if phdr_file_offset >= file_offset && phdr_file_offset < file_end {
                    let offset_in_segment = phdr_file_offset - file_offset;
                    phdr_load_vaddr = Some(add_load_bias(
                        virtual_addr
                            .checked_add(offset_in_segment)
                            .ok_or(MmError::InvalidElf)?,
                        load_bias,
                    )?);
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
            let start = usize::from(start_va);
            let end = usize::from(end_va);
            let file_end = start.checked_add(file_size).ok_or(MmError::InvalidElf)?;
            let first_page = start & !(PAGE_SIZE - 1);
            let full_file_start = (start + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            let full_file_end = file_end & !(PAGE_SIZE - 1);

            max_end_vpn = max_end_vpn.max(VirtAddr::from(end).ceil());

            // An unaligned first page, or a segment whose file part fits in
            // one page, cannot be represented by a page-aligned file VMA
            // without exposing bytes outside the PT_LOAD. Copy just this
            // boundary page into a zeroed private frame.
            let eager_first = if file_size == 0 {
                start & (PAGE_SIZE - 1) != 0
            } else {
                full_file_start >= full_file_end || start & (PAGE_SIZE - 1) != 0
            };
            if eager_first {
                map_elf_boundary_page(self.memory_set, first_page, map_perm)?;
                copy_elf_file_range(
                    self.memory_set,
                    file,
                    start,
                    file_offset,
                    file_size.min(PAGE_SIZE - (start & (PAGE_SIZE - 1))),
                )?;
            }

            // All pages in this interval are wholly covered by file bytes.
            // They can therefore be backed directly by the inode's page
            // cache and loaded only when code/data first touches them.
            if full_file_start < full_file_end {
                let full_start_offset = full_file_start
                    .checked_sub(start)
                    .ok_or(MmError::InvalidElf)?;
                let full_file_offset = file_offset
                    .checked_add(full_start_offset)
                    .ok_or(MmError::InvalidElf)?;
                let vma = Vma::new_file(
                    VirtAddr::from(full_file_start),
                    VirtAddr::from(full_file_end),
                    map_perm,
                    Arc::clone(&file_description),
                    full_file_offset / PAGE_SIZE,
                    false,
                );
                self.memory_set.insert_vma(vma, None)?;
            }

            // Copy the final partial file page. Its remaining bytes are
            // already zero, and the following complete BSS pages become
            // anonymous lazy mappings below.
            if file_size != 0 && file_end & (PAGE_SIZE - 1) != 0 {
                let tail_page = file_end & !(PAGE_SIZE - 1);
                if !eager_first || tail_page != first_page {
                    map_elf_boundary_page(self.memory_set, tail_page, map_perm)?;
                    let tail_offset = tail_page.checked_sub(start).ok_or(MmError::InvalidElf)?;
                    let tail_file_offset = file_offset
                        .checked_add(tail_offset)
                        .ok_or(MmError::InvalidElf)?;
                    copy_elf_file_range(
                        self.memory_set,
                        file,
                        tail_page,
                        tail_file_offset,
                        file_end - tail_page,
                    )?;
                }
            }

            // If the file part ends in the middle of a page, the partial
            // tail page above already includes its zero-filled BSS suffix.
            // Start anonymous lazy allocation at the next page boundary.
            let bss_start = (file_end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            let bss_end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
            if bss_start < bss_end {
                self.memory_set.insert_vma(
                    Vma::new_anonymous(
                        VirtAddr::from(bss_start),
                        VirtAddr::from(bss_end),
                        map_perm,
                        false,
                    ),
                    None,
                )?;
            }
        }

        let max_end_va: VirtAddr = max_end_vpn.into();
        let info = ElfLoadInfo {
            entry_point: add_load_bias(elf.header.pt2.entry_point() as usize, load_bias)?,
            phdr_vaddr: phdr_load_vaddr.unwrap_or(0),
            phent_size: elf.header.pt2.ph_entry_size() as usize,
            phnum: ph_count as usize,
            interp_path,
        };
        Ok(LoadedElf {
            info,
            image_end: usize::from(max_end_va),
        })
    }
}

fn is_et_dyn_without_interp(elf: &xmas_elf::ElfFile<'_>) -> Result<bool, MmError> {
    if elf.header.pt2.type_().as_type() != xmas_elf::header::Type::SharedObject {
        return Ok(false);
    }
    let has_interp = (0..elf.header.pt2.ph_count()).try_fold(false, |found, index| {
        let ph = elf.program_header(index).map_err(|_| MmError::InvalidElf)?;
        let is_interp =
            ph.get_type().map_err(|_| MmError::InvalidElf)? == xmas_elf::program::Type::Interp;
        Ok::<bool, MmError>(found || is_interp)
    })?;
    Ok(!has_interp)
}

/// Return whether this image uses CosmOS's legacy kernel-relocated static PIE
/// ABI. `ET_DYN` alone is not sufficient: a runtime linker such as glibc's
/// `ld.so` has no `PT_INTERP` either, but its entry code deliberately runs
/// before relocations and relocates the image itself. GNU linkers mark actual
/// PIE executables with `DF_1_PIE`, while runtime linkers/shared objects leave
/// that bit clear.
fn needs_kernel_static_pie_relocations(elf: &xmas_elf::ElfFile<'_>) -> Result<bool, MmError> {
    const ELF64_DYN_SIZE: usize = 16;
    const DT_NULL: u64 = 0;
    const DT_FLAGS_1: u64 = 0x6fff_fffb;

    if !is_et_dyn_without_interp(elf)? {
        return Ok(false);
    }

    for index in 0..elf.header.pt2.ph_count() {
        let ph = elf.program_header(index).map_err(|_| MmError::InvalidElf)?;
        if ph.get_type().map_err(|_| MmError::InvalidElf)? != xmas_elf::program::Type::Dynamic {
            continue;
        }

        let offset = usize::try_from(ph.offset()).map_err(|_| MmError::InvalidElf)?;
        let size = usize::try_from(ph.file_size()).map_err(|_| MmError::InvalidElf)?;
        let end = offset.checked_add(size).ok_or(MmError::InvalidElf)?;
        let bytes = elf.input.get(offset..end).ok_or(MmError::InvalidElf)?;
        if bytes.len() % ELF64_DYN_SIZE != 0 {
            return Err(MmError::InvalidElf);
        }

        for entry in bytes.chunks_exact(ELF64_DYN_SIZE) {
            let tag = u64::from_le_bytes(entry[..8].try_into().map_err(|_| MmError::InvalidElf)?);
            if tag == DT_NULL {
                break;
            }
            if tag == DT_FLAGS_1 {
                let flags =
                    u64::from_le_bytes(entry[8..].try_into().map_err(|_| MmError::InvalidElf)?);
                return Ok(flags & xmas_elf::dynamic::FLAG_1_PIE != 0);
            }
        }
    }
    Ok(false)
}

fn add_load_bias(addr: usize, load_bias: usize) -> Result<usize, MmError> {
    addr.checked_add(load_bias).ok_or(MmError::InvalidElf)
}

fn read_file_exact(file: &Arc<OSInode>, offset: usize, buf: &mut [u8]) -> Result<(), MmError> {
    let mut done = 0usize;
    while done < buf.len() {
        let read = file
            .read_bytes_at(
                offset.checked_add(done).ok_or(MmError::InvalidElf)?,
                &mut buf[done..],
            )
            .map_err(|_| MmError::InvalidElf)?;
        if read == 0 {
            return Err(MmError::InvalidElf);
        }
        done += read;
    }
    Ok(())
}

/// Read only the ELF header and program-header table. The returned buffer is
/// intentionally sparse/zero-filled between the ELF header and `e_phoff`;
/// xmas-elf only needs the bytes covering those two structures for the
/// transition loader.
fn read_elf_metadata(file: &Arc<OSInode>) -> Result<Vec<u8>, MmError> {
    let mut header = [0u8; 64];
    read_file_exact(file, 0, &mut header)?;
    if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 {
        return Err(MmError::InvalidElf);
    }

    let phoff = usize::try_from(u64::from_le_bytes(
        header[32..40].try_into().map_err(|_| MmError::InvalidElf)?,
    ))
    .map_err(|_| MmError::InvalidElf)?;
    let phentsize = usize::from(u16::from_le_bytes(
        header[54..56].try_into().map_err(|_| MmError::InvalidElf)?,
    ));
    let phnum = usize::from(u16::from_le_bytes(
        header[56..58].try_into().map_err(|_| MmError::InvalidElf)?,
    ));
    if phoff < header.len() || phentsize == 0 || phnum == 0 {
        return Err(MmError::InvalidElf);
    }
    let ph_size = phentsize.checked_mul(phnum).ok_or(MmError::InvalidElf)?;
    let ph_end = phoff.checked_add(ph_size).ok_or(MmError::InvalidElf)?;
    if ph_end > ELF_METADATA_MAX_SIZE {
        return Err(MmError::InvalidElf);
    }

    let mut metadata = alloc::vec![0u8; ph_end.max(header.len())];
    metadata[..header.len()].copy_from_slice(&header);
    read_file_exact(file, phoff, &mut metadata[phoff..ph_end])?;
    Ok(metadata)
}

/// Copy one ELF file range directly into already mapped user pages. Only a
/// small reusable buffer is allocated in kernel space, so a large debug ELF
/// no longer causes a large `Vec` allocation during exec.
fn copy_elf_file_range(
    memory_set: &mut MemorySet,
    file: &Arc<OSInode>,
    start_va: usize,
    file_offset: usize,
    file_size: usize,
) -> Result<(), MmError> {
    let mut buffer = alloc::vec![0u8; ELF_IO_CHUNK_SIZE];
    let mut copied = 0usize;
    while copied < file_size {
        let chunk_len = min(buffer.len(), file_size - copied);
        read_file_exact(
            file,
            file_offset.checked_add(copied).ok_or(MmError::InvalidElf)?,
            &mut buffer[..chunk_len],
        )?;

        let mut chunk_offset = 0usize;
        while chunk_offset < chunk_len {
            let va = start_va
                .checked_add(copied)
                .and_then(|value| value.checked_add(chunk_offset))
                .ok_or(MmError::InvalidElf)?;
            let vpn = VirtAddr::from(va).floor();
            let page_offset = va & (PAGE_SIZE - 1);
            let copy_len = min(chunk_len - chunk_offset, PAGE_SIZE - page_offset);
            let ppn = memory_set
                .page_table
                .translate(vpn)
                .ok_or(MmError::NoMapping)?
                .ppn();
            ppn.get_bytes_array()[page_offset..page_offset + copy_len]
                .copy_from_slice(&buffer[chunk_offset..chunk_offset + copy_len]);
            chunk_offset += copy_len;
        }
        copied += chunk_len;
    }
    Ok(())
}

/// Create the read-only open-file description retained by an ELF file VMA.
/// The VMA owns this description after `exec`, so the executable can be
/// faulted in even though the original userspace fd was never installed.
fn elf_file_description(file: &Arc<OSInode>) -> Arc<FileDescription> {
    let file_object: Arc<dyn File + Send + Sync> = file.clone();
    Arc::new(FileDescription::new(
        file_object,
        AccessMode::ReadOnly,
        FileStatusFlags::empty(),
        0,
    ))
}

/// Eagerly map one ELF boundary page. The caller copies only the bytes that
/// belong to the segment; newly allocated frames are already zeroed, which
/// also supplies the required padding/BSS bytes in this page.
fn map_elf_boundary_page(
    memory_set: &mut MemorySet,
    page_start: usize,
    map_perm: MapPermission,
) -> Result<(), MmError> {
    let page_end = page_start
        .checked_add(PAGE_SIZE)
        .ok_or(MmError::InvalidElf)?;
    memory_set.insert_vma(
        Vma::new_elf(
            VirtAddr::from(page_start),
            VirtAddr::from(page_end),
            map_perm,
        ),
        None,
    )
}

fn relocated_value(load_bias: usize, addend: i64) -> Result<usize, MmError> {
    if addend >= 0 {
        load_bias
            .checked_add(addend as usize)
            .ok_or(MmError::InvalidElf)
    } else {
        load_bias
            .checked_sub(addend.unsigned_abs() as usize)
            .ok_or(MmError::InvalidElf)
    }
}

fn write_user_usize(memory_set: &mut MemorySet, va: usize, value: usize) -> Result<(), MmError> {
    for (idx, byte) in value.to_le_bytes().iter().copied().enumerate() {
        let pa = memory_set
            .page_table
            .translate_va(VirtAddr(va + idx))
            .ok_or(MmError::NoMapping)?;
        *pa.get_mut::<u8>() = byte;
    }
    Ok(())
}

fn file_offset_for_vaddr(elf: &xmas_elf::ElfFile<'_>, vaddr: usize) -> Option<usize> {
    let ph_count = elf.header.pt2.ph_count();
    for i in 0..ph_count {
        let ph = elf.program_header(i).ok()?;
        if ph.get_type().ok()? != xmas_elf::program::Type::Load {
            continue;
        }
        let seg_start = ph.virtual_addr() as usize;
        let seg_size = ph.file_size() as usize;
        let seg_end = seg_start.checked_add(seg_size)?;
        if vaddr < seg_start || vaddr >= seg_end {
            continue;
        }
        let within_seg = vaddr.checked_sub(seg_start)?;
        return (ph.offset() as usize).checked_add(within_seg);
    }
    None
}

fn read_dynsym_value(
    elf: &xmas_elf::ElfFile<'_>,
    symtab_vaddr: usize,
    sym_ent: usize,
    sym_index: u32,
) -> Result<(usize, u16), MmError> {
    if sym_ent != 24 {
        return Err(MmError::InvalidElf);
    }
    let symtab_offset = file_offset_for_vaddr(elf, symtab_vaddr).ok_or(MmError::InvalidElf)?;
    let sym_offset = symtab_offset
        .checked_add(
            sym_ent
                .checked_mul(sym_index as usize)
                .ok_or(MmError::InvalidElf)?,
        )
        .ok_or(MmError::InvalidElf)?;
    let sym_end = sym_offset.checked_add(sym_ent).ok_or(MmError::InvalidElf)?;
    let sym = elf
        .input
        .get(sym_offset..sym_end)
        .ok_or(MmError::InvalidElf)?;
    let shndx = u16::from_le_bytes(sym[6..8].try_into().map_err(|_| MmError::InvalidElf)?);
    let value = usize::from_le_bytes(sym[8..16].try_into().map_err(|_| MmError::InvalidElf)?);
    Ok((value, shndx))
}

fn apply_static_pie_relocations(
    memory_set: &mut MemorySet,
    elf: &xmas_elf::ElfFile<'_>,
    load_bias: usize,
) -> Result<(), MmError> {
    let mut rela_vaddr: Option<usize> = None;
    let mut rela_size = 0usize;
    let mut rela_ent = 0usize;
    let mut symtab_vaddr: Option<usize> = None;
    let mut sym_ent = 0usize;
    let ph_count = elf.header.pt2.ph_count();

    for i in 0..ph_count {
        let ph = elf.program_header(i).map_err(|_| MmError::InvalidElf)?;
        if ph.get_type().map_err(|_| MmError::InvalidElf)? != xmas_elf::program::Type::Dynamic {
            continue;
        }
        let entries = ph.get_data(elf).map_err(|_| MmError::InvalidElf)?;
        let xmas_elf::program::SegmentData::Dynamic64(entries) = entries else {
            return Err(MmError::InvalidElf);
        };
        for entry in entries {
            match entry.get_tag().map_err(|_| MmError::InvalidElf)? {
                xmas_elf::dynamic::Tag::Rela => {
                    rela_vaddr = Some(entry.get_ptr().map_err(|_| MmError::InvalidElf)? as usize);
                }
                xmas_elf::dynamic::Tag::RelaSize => {
                    rela_size = entry.get_val().map_err(|_| MmError::InvalidElf)? as usize;
                }
                xmas_elf::dynamic::Tag::RelaEnt => {
                    rela_ent = entry.get_val().map_err(|_| MmError::InvalidElf)? as usize;
                }
                xmas_elf::dynamic::Tag::SymTab => {
                    symtab_vaddr = Some(entry.get_ptr().map_err(|_| MmError::InvalidElf)? as usize);
                }
                xmas_elf::dynamic::Tag::SymEnt => {
                    sym_ent = entry.get_val().map_err(|_| MmError::InvalidElf)? as usize;
                }
                xmas_elf::dynamic::Tag::Rel | xmas_elf::dynamic::Tag::JmpRel => {
                    return Err(MmError::InvalidElf);
                }
                _ => {}
            }
        }
    }

    if rela_size == 0 {
        return Ok(());
    }
    if rela_ent != 24 {
        return Err(MmError::InvalidElf);
    }

    let rela_vaddr = rela_vaddr.ok_or(MmError::InvalidElf)?;
    let rela_offset = file_offset_for_vaddr(elf, rela_vaddr).ok_or(MmError::InvalidElf)?;
    let rela_end = rela_offset
        .checked_add(rela_size)
        .ok_or(MmError::InvalidElf)?;
    let rela_bytes = elf
        .input
        .get(rela_offset..rela_end)
        .ok_or(MmError::InvalidElf)?;
    if rela_bytes.len() % rela_ent != 0 {
        return Err(MmError::InvalidElf);
    }

    for chunk in rela_bytes.chunks_exact(rela_ent) {
        let offset = usize::from_le_bytes(chunk[0..8].try_into().map_err(|_| MmError::InvalidElf)?);
        let info = u64::from_le_bytes(chunk[8..16].try_into().map_err(|_| MmError::InvalidElf)?);
        let addend = i64::from_le_bytes(chunk[16..24].try_into().map_err(|_| MmError::InvalidElf)?);
        let rel_type = info as u32;
        let sym_index = (info >> 32) as u32;
        let target = add_load_bias(offset, load_bias)?;
        let value = match rel_type {
            R_RISCV_RELATIVE => {
                if sym_index != 0 {
                    return Err(MmError::InvalidElf);
                }
                relocated_value(load_bias, addend)?
            }
            2 => {
                let symtab_vaddr = symtab_vaddr.ok_or(MmError::InvalidElf)?;
                let (sym_value, shndx) = read_dynsym_value(elf, symtab_vaddr, sym_ent, sym_index)?;
                if shndx == 0 {
                    return Err(MmError::InvalidElf);
                }
                let sym_addr = add_load_bias(sym_value, load_bias)?;
                relocated_value(sym_addr, addend)?
            }
            _ => return Err(MmError::InvalidElf),
        };
        write_user_usize(memory_set, target, value)?;
    }

    Ok(())
}
