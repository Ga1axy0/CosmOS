use crate::config::PAGE_SIZE;
use crate::mm::{
    translated_byte_buffer, MmError, PageFaultAccess, PageFaultHandled, PageTable, VirtAddr,
    USER_SPACE_END,
};
use crate::syscall::errno::{OrErrno, ERRNO};
use crate::task::{current_process, current_user_token, ProcessControlBlock};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use core::mem::{size_of, MaybeUninit};
use core::slice;

/// 标记“可按原始字节整体复制”的 ABI 数据结构。
///
/// 该 trait 为空 trait，需要由声明了 `#[repr(C)]` 的类型手动实现，
/// 以显式确认该类型可以安全地通过 `write_pod_to_user` 按字节写回用户空间。
pub trait Pod {}

// i32 是 Linux ABI 中常见的简单写回类型，如 tid/status。
impl Pod for i16 {}
impl Pod for i32 {}
impl Pod for i64 {}
// u32/u64/usize 是若干 syscall 写回标量结果时使用的基础 ABI 类型。
impl Pod for u32 {}
impl Pod for u64 {}
impl Pod for usize {}

/// 判断页表项是否允许指定用户态访问。
fn pte_allows_user_access(pte: crate::mm::PageTableEntry, access: PageFaultAccess) -> bool {
    if !pte.is_user() {
        return false;
    }
    match access {
        PageFaultAccess::Read => pte.readable(),
        PageFaultAccess::Write => pte.writable(),
        PageFaultAccess::Exec => pte.executable(),
    }
}

fn checked_user_buffer_end(ptr: *const u8, len: usize) -> Option<usize> {
    let start = ptr as usize;
    if len == 0 {
        return Some(start);
    }
    if start >= USER_SPACE_END {
        return None;
    }
    let end = start.checked_add(len)?;
    (end <= USER_SPACE_END).then_some(end)
}

fn translated_byte_buffer_fast(
    token: usize,
    ptr: *const u8,
    len: usize,
    access: PageFaultAccess,
) -> Option<Vec<&'static mut [u8]>> {
    if len == 0 {
        return Some(Vec::new());
    }

    let start = ptr as usize;
    let end = checked_user_buffer_end(ptr, len)?;
    let page_table = PageTable::from_token(token);
    let mut page_start = start & !(PAGE_SIZE - 1);
    let span = end - page_start;
    let page_count = span / PAGE_SIZE + usize::from(span % PAGE_SIZE != 0);
    // Avoid turning a large user-supplied length into an equally large eager
    // kernel allocation.  Common small buffers still get their capacity
    // reserved up front; larger ranges grow only as mapped pages are found.
    let mut buffers = Vec::with_capacity(page_count.min(16));

    while page_start < end {
        let vpn = VirtAddr::from(page_start).floor();
        let pte = page_table.translate(vpn)?;
        if !pte_allows_user_access(pte, access) {
            return None;
        }

        let page_end = page_start.checked_add(PAGE_SIZE)?;
        let chunk_start = start.max(page_start);
        let chunk_end = end.min(page_end);
        let page_offset = chunk_start - page_start;
        let chunk_len = chunk_end - chunk_start;
        let page = pte.ppn().get_bytes_array();
        buffers.push(&mut page[page_offset..page_offset + chunk_len]);
        page_start = page_end;
    }

    Some(buffers)
}

/// Translate a user buffer that is entirely contained in one page.
///
/// Most syscall metadata buffers are small and single-page, so keep this
/// common case to one page-table lookup and one slice construction.  Missing
/// mappings, insufficient permissions, and cross-page buffers deliberately
/// fall back to the existing fault-in/retry path.
fn translated_single_page_fast(
    token: usize,
    ptr: *const u8,
    len: usize,
    access: PageFaultAccess,
) -> Option<&'static mut [u8]> {
    if len == 0 {
        return None;
    }

    checked_user_buffer_end(ptr, len)?;
    let start = ptr as usize;
    let page_offset = start & (PAGE_SIZE - 1);
    if page_offset.checked_add(len)? > PAGE_SIZE {
        return None;
    }

    let page_table = PageTable::from_token(token);
    let vpn = VirtAddr::from(start & !(PAGE_SIZE - 1)).floor();
    let pte = page_table.translate(vpn)?;
    if !pte_allows_user_access(pte, access) {
        return None;
    }

    let ppn = pte.ppn();
    let page = ppn.get_bytes_array();
    Some(&mut page[page_offset..page_offset + len])
}

/// 尝试为一段用户虚拟地址触发并完成缺页装入，使后续字节翻译可成功。
///
/// 同时检查最终 PTE 是否具备用户态访问权限，避免内核 copyin/copyout 绕过
/// 用户页保护语义。
pub(crate) fn prefault_user_pages(
    process: &Arc<ProcessControlBlock>,
    token: usize,
    ptr: *const u8,
    len: usize,
    access: PageFaultAccess,
) -> Result<(), ERRNO> {
    if len == 0 {
        return Ok(());
    }

    let start = ptr as usize;
    let end = start.checked_add(len).ok_or(ERRNO::EFAULT)?;
    let mut page_start = start & !(PAGE_SIZE - 1);

    while page_start < end {
        let vpn = VirtAddr::from(page_start).floor();
        let page_table = PageTable::from_token(token);
        match page_table.translate(vpn) {
            Some(pte) => {
                // 映射已存在但不可写：可能是 COW 或 MAP_SHARED 写通知页。
                if access == PageFaultAccess::Write && !pte.writable() {
                    if process
                        .handle_private_cow_fault(page_start)
                        .map_err(|err| match err {
                            MmError::OutOfMemory => ERRNO::ENOMEM,
                            _ => ERRNO::EFAULT,
                        })?
                        != PageFaultHandled::Handled
                    {
                        match process.handle_file_page_fault(page_start, PageFaultAccess::Write) {
                            Ok(PageFaultHandled::Handled) => {}
                            Ok(PageFaultHandled::NotHandled) => return Err(ERRNO::EFAULT),
                            Err(MmError::BeyondFileEnd) => return Err(ERRNO::EFAULT),
                            Err(MmError::OutOfMemory) => return Err(ERRNO::ENOMEM),
                            Err(_) => return Err(ERRNO::EFAULT),
                        }
                    }
                }
            }
            None => {
                if access == PageFaultAccess::Write
                    && process
                        .handle_private_cow_fault(page_start)
                        .map_err(|err| match err {
                            MmError::OutOfMemory => ERRNO::ENOMEM,
                            _ => ERRNO::EFAULT,
                        })?
                        == PageFaultHandled::Handled
                {
                    page_start = page_start.checked_add(PAGE_SIZE).ok_or(ERRNO::EFAULT)?;
                    continue;
                }
                if process
                    .handle_lazy_user_fault(page_start, access)
                    .map_err(|err| match err {
                        MmError::OutOfMemory => ERRNO::ENOMEM,
                        _ => ERRNO::EFAULT,
                    })?
                    == PageFaultHandled::Handled
                {
                    page_start = page_start.checked_add(PAGE_SIZE).ok_or(ERRNO::EFAULT)?;
                    continue;
                }
                match process.handle_file_page_fault(page_start, access) {
                    Ok(PageFaultHandled::Handled) => {}
                    Ok(PageFaultHandled::NotHandled) => return Err(ERRNO::EFAULT),
                    Err(MmError::BeyondFileEnd) => return Err(ERRNO::EFAULT),
                    Err(MmError::OutOfMemory) => return Err(ERRNO::ENOMEM),
                    Err(_) => return Err(ERRNO::EFAULT),
                }
            }
        }
        let page_table = PageTable::from_token(token);
        let pte = page_table.translate(vpn).ok_or(ERRNO::EFAULT)?;
        if !pte_allows_user_access(pte, access) {
            return Err(ERRNO::EFAULT);
        }

        page_start = page_start.checked_add(PAGE_SIZE).ok_or(ERRNO::EFAULT)?;
    }

    Ok(())
}

/// 将用户地址翻译为内核可写切片；若命中 lazy/COW 页，先在内核态完成补页再重试。
pub fn translated_byte_buffer_with_access(
    ptr: *const u8,
    len: usize,
    access: PageFaultAccess,
) -> Result<Vec<&'static mut [u8]>, ERRNO> {
    let token = current_user_token();
    if let Some(buffers) = translated_byte_buffer_fast(token, ptr, len, access) {
        return Ok(buffers);
    }
    let process = current_process();
    prefault_user_pages(&process, token, ptr, len, access)?;
    translated_byte_buffer(token, ptr, len).or_errno(ERRNO::EFAULT)
}

/// 将指定进程的用户地址翻译为内核可写切片，并按访问类型检查权限。
pub fn translated_process_byte_buffer_with_access(
    process: &Arc<ProcessControlBlock>,
    ptr: *const u8,
    len: usize,
    access: PageFaultAccess,
) -> Result<Vec<&'static mut [u8]>, ERRNO> {
    let token = process.inner_exclusive_access().memory_set.token();
    prefault_user_pages(process, token, ptr, len, access)?;
    translated_byte_buffer(token, ptr, len).or_errno(ERRNO::EFAULT)
}

/// 将一段字节序列写回到用户地址空间。
pub fn write_bytes_to_user(ptr: *mut u8, src: &[u8]) -> Result<(), ERRNO> {
    if !src.is_empty() {
        let token = current_user_token();
        if let Some(buffer) = translated_single_page_fast(
            token,
            ptr as *const u8,
            src.len(),
            PageFaultAccess::Write,
        ) {
            buffer.copy_from_slice(src);
            return Ok(());
        }
    }

    let mut buffers =
        translated_byte_buffer_with_access(ptr as *const u8, src.len(), PageFaultAccess::Write)?;
    let mut copied = 0usize;
    for buffer in buffers.iter_mut() {
        let len = buffer.len();
        buffer.copy_from_slice(&src[copied..copied + len]);
        copied += len;
    }
    Ok(())
}

/// 将一段字节序列写回到指定进程的用户地址空间。
pub fn write_bytes_to_process_user(
    process: &Arc<ProcessControlBlock>,
    ptr: *mut u8,
    src: &[u8],
) -> Result<(), ERRNO> {
    let mut buffers = translated_process_byte_buffer_with_access(
        process,
        ptr as *const u8,
        src.len(),
        PageFaultAccess::Write,
    )?;
    let mut copied = 0usize;
    for buffer in buffers.iter_mut() {
        let len = buffer.len();
        buffer.copy_from_slice(&src[copied..copied + len]);
        copied += len;
    }
    Ok(())
}

/// 将一个 POD 结构写回到用户地址空间。
pub fn write_pod_to_user<T: Pod>(ptr: *mut T, value: &T) -> Result<(), ERRNO> {
    let value_bytes =
        unsafe { slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) };
    write_bytes_to_user(ptr as *mut u8, value_bytes)
}

/// 将一个 POD 结构写回到指定进程的用户地址空间。
pub fn write_pod_to_process_user<T: Pod>(
    process: &Arc<ProcessControlBlock>,
    ptr: *mut T,
    value: &T,
) -> Result<(), ERRNO> {
    let value_bytes =
        unsafe { slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) };
    write_bytes_to_process_user(process, ptr as *mut u8, value_bytes)
}

/// 从指定进程的用户地址空间读取一段字节，允许跨越多个用户页。
pub fn read_bytes_from_process_user(
    process: &Arc<ProcessControlBlock>,
    ptr: *const u8,
    len: usize,
) -> Result<Vec<u8>, ERRNO> {
    let buffers =
        translated_process_byte_buffer_with_access(process, ptr, len, PageFaultAccess::Read)?;
    let mut bytes = Vec::with_capacity(len);
    for buffer in buffers.iter() {
        bytes.extend_from_slice(buffer);
    }
    Ok(bytes)
}

/// 从用户地址空间读取一段字节，允许跨越多个用户页。
pub fn read_bytes_from_user(ptr: *const u8, len: usize) -> Result<Vec<u8>, ERRNO> {
    let buffers = translated_byte_buffer_with_access(ptr, len, PageFaultAccess::Read)?;
    let mut bytes = Vec::with_capacity(len);
    for buffer in buffers.iter() {
        bytes.extend_from_slice(buffer);
    }
    Ok(bytes)
}

/// Read a NUL-terminated user string, faulting in lazy/file-backed pages as needed.
pub fn read_cstring_from_user(ptr: *const u8, max_len: usize) -> Result<String, ERRNO> {
    if ptr.is_null() {
        return Err(ERRNO::EFAULT);
    }
    if max_len == 0 {
        return Err(ERRNO::ENAMETOOLONG);
    }
    let mut out = String::new();
    let token = current_user_token();
    let mut offset = 0usize;
    while offset < max_len {
        // Translate one page-sized run at a time instead of one byte at a
        // time.  The old per-byte loop performed a full page-table walk and a
        // fresh `Vec` allocation for every character, which dominated path
        // string copyin for `open`/`stat`-style syscalls.
        let cur = unsafe { ptr.add(offset) };
        let page_remain = PAGE_SIZE - ((cur as usize) & (PAGE_SIZE - 1));
        let chunk_len = (max_len - offset).min(page_remain);
        if let Some(chunk) =
            translated_single_page_fast(token, cur, chunk_len, PageFaultAccess::Read)
        {
            if let Some(nul_idx) = chunk.iter().position(|&byte| byte == 0) {
                out.extend(chunk[..nul_idx].iter().copied().map(|byte| byte as char));
                return Ok(out);
            }
            out.extend(chunk.iter().copied().map(|byte| byte as char));
        } else {
            let buffers =
                translated_byte_buffer_with_access(cur, chunk_len, PageFaultAccess::Read)?;
            let mut nul_found = false;
            for chunk in buffers {
                if let Some(nul_idx) = chunk.iter().position(|&byte| byte == 0) {
                    out.extend(chunk[..nul_idx].iter().copied().map(|byte| byte as char));
                    nul_found = true;
                    break;
                }
                out.extend(chunk.iter().copied().map(|byte| byte as char));
            }
            if nul_found {
                return Ok(out);
            }
        }
        offset += chunk_len;
    }
    Err(ERRNO::ENAMETOOLONG)
}

/// 从用户地址空间读取一个 POD 结构，允许结构体跨越多个用户页。
pub fn read_pod_from_user<T: Pod>(ptr: *const T) -> Result<T, ERRNO> {
    let mut value = MaybeUninit::<T>::uninit();
    let value_bytes =
        unsafe { slice::from_raw_parts_mut(value.as_mut_ptr() as *mut u8, size_of::<T>()) };

    if size_of::<T>() != 0 {
        let token = current_user_token();
        if let Some(bytes) = translated_single_page_fast(
            token,
            ptr as *const u8,
            size_of::<T>(),
            PageFaultAccess::Read,
        ) {
            value_bytes.copy_from_slice(bytes);
            return Ok(unsafe { value.assume_init() });
        }
    }

    let bytes = read_bytes_from_user(ptr as *const u8, size_of::<T>())?;
    value_bytes.copy_from_slice(&bytes);
    Ok(unsafe { value.assume_init() })
}

/// 从指定进程的用户地址空间读取一个 POD 结构，允许结构体跨越多个用户页。
pub fn read_pod_from_process_user<T: Pod>(
    process: &Arc<ProcessControlBlock>,
    ptr: *const T,
) -> Result<T, ERRNO> {
    let bytes = read_bytes_from_process_user(process, ptr as *const u8, size_of::<T>())?;
    let mut value = MaybeUninit::<T>::uninit();
    let value_bytes =
        unsafe { slice::from_raw_parts_mut(value.as_mut_ptr() as *mut u8, size_of::<T>()) };
    value_bytes.copy_from_slice(&bytes);
    Ok(unsafe { value.assume_init() })
}
