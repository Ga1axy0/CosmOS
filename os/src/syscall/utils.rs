use crate::mm::translated_byte_buffer;
use crate::syscall::errno::{OrErrno, ERRNO};
use crate::task::current_user_token;

use core::mem::size_of;
use core::slice;

/// 将一段字节序列写回到用户地址空间。
pub fn write_bytes_to_user(ptr: *mut u8, src: &[u8]) -> Result<(), ERRNO> {
    let mut buffers = translated_byte_buffer(current_user_token(), ptr as *const u8, src.len())
        .or_errno(ERRNO::EFAULT)?;
    let mut copied = 0usize;
    for buffer in buffers.iter_mut() {
        let len = buffer.len();
        buffer.copy_from_slice(&src[copied..copied + len]);
        copied += len;
    }
    Ok(())
}

/// 将一个 POD 结构写回到用户地址空间。
pub fn write_pod_to_user<T>(ptr: *mut T, value: &T) -> Result<(), ERRNO> {
    let value_bytes =
        unsafe { slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) };
    write_bytes_to_user(ptr as *mut u8, value_bytes)
}
