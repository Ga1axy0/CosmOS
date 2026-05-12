use crate::config::PAGE_SIZE;
use crate::mm::{translated_byte_buffer, PageFaultAccess, PageTable, VirtAddr};
use crate::syscall::errno::{OrErrno, ERRNO};
use crate::task::{current_process, current_user_token};

use alloc::vec::Vec;

use core::mem::{size_of, MaybeUninit};
use core::slice;

/// 标记“可按原始字节整体复制”的 ABI 数据结构。
///
/// 该 trait 为空 trait，需要由声明了 `#[repr(C)]` 的类型手动实现，
/// 以显式确认该类型可以安全地通过 `write_pod_to_user` 按字节写回用户空间。
pub trait Pod {}

// i32 是 Linux ABI 中常见的简单写回类型，如 tid/status。
impl Pod for i32 {}

/// 尝试为一段用户虚拟地址触发并完成缺页装入，使后续字节翻译可成功。
///
/// 仅处理与当前进程地址空间相关的 lazy file-backed / COW 场景；若地址本身
/// 不合法或权限不允许，会返回对应错误。
fn prefault_user_pages(
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
    let page_table = PageTable::from_token(token);
    let process = current_process();
    let mut page_start = start & !(PAGE_SIZE - 1);

    while page_start < end {
        let vpn = VirtAddr::from(page_start).floor();
        match page_table.translate(vpn) {
            Some(pte) => {
                // 映射已存在但不可写：可能是 COW 或 MAP_SHARED 写通知页。
                if access == PageFaultAccess::Write && !pte.writable() {
                    if !process.handle_private_cow_fault(page_start) {
                        match process.handle_file_page_fault(page_start, PageFaultAccess::Write) {
                            Ok(()) => {}
                            // 用户态缺页此场景会被视为 SIGBUS，这里按 copyin/copyout 语义返回 EFAULT。
                            Err(ERRNO::ENXIO) => return Err(ERRNO::EFAULT),
                            Err(e) => return Err(e),
                        }
                    }
                }
            }
            None => {
                if access == PageFaultAccess::Write && process.handle_private_cow_fault(page_start)
                {
                    page_start = page_start.checked_add(PAGE_SIZE).ok_or(ERRNO::EFAULT)?;
                    continue;
                }
                if process.handle_lazy_heap_fault(page_start, access) {
                    page_start = page_start.checked_add(PAGE_SIZE).ok_or(ERRNO::EFAULT)?;
                    continue;
                }
                match process.handle_file_page_fault(page_start, access) {
                    Ok(()) => {}
                    // 用户态缺页此场景会被视为 SIGBUS，这里按 copyin/copyout 语义返回 EFAULT。
                    Err(ERRNO::ENXIO) => return Err(ERRNO::EFAULT),
                    Err(e) => return Err(e),
                }
            }
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
    if let Some(buffers) = translated_byte_buffer(token, ptr, len) {
        return Ok(buffers);
    }

    prefault_user_pages(token, ptr, len, access)?;
    translated_byte_buffer(token, ptr, len).or_errno(ERRNO::EFAULT)
}

/// 将一段字节序列写回到用户地址空间。
pub fn write_bytes_to_user(ptr: *mut u8, src: &[u8]) -> Result<(), ERRNO> {
    let mut buffers = translated_byte_buffer_with_access(
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

/// 从用户地址空间读取一段字节，允许跨越多个用户页。
pub fn read_bytes_from_user(ptr: *const u8, len: usize) -> Result<Vec<u8>, ERRNO> {
    let buffers = translated_byte_buffer_with_access(ptr, len, PageFaultAccess::Read)?;
    let mut bytes = Vec::with_capacity(len);
    for buffer in buffers.iter() {
        bytes.extend_from_slice(buffer);
    }
    Ok(bytes)
}

/// 从用户地址空间读取一个 POD 结构，允许结构体跨越多个用户页。
pub fn read_pod_from_user<T: Pod>(ptr: *const T) -> Result<T, ERRNO> {
    let bytes = read_bytes_from_user(ptr as *const u8, size_of::<T>())?;
    let mut value = MaybeUninit::<T>::uninit();
    let value_bytes =
    unsafe { slice::from_raw_parts_mut(value.as_mut_ptr() as *mut u8, size_of::<T>()) };
    value_bytes.copy_from_slice(&bytes);
    Ok(unsafe { value.assume_init() })
}
