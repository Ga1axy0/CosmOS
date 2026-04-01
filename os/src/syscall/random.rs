use crate::mm::translated_byte_buffer;
use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall_body;
use crate::task::current_user_token;
use crate::task::WaitReason;

const GRND_NONBLOCK: usize = 0x0001;
const GRND_RANDOM: usize = 0x0004; // supported but treated same as default

/// Minimal getrandom syscall: fill user buffer with CSPRNG bytes.
pub fn sys_getrandom(buf: *mut u8, len: usize, flags: usize) -> isize {
    trace!("kernel: sys_getrandom");
    let token = current_user_token();
    syscall_body!({
        // Translate user buffer (may be discontiguous across pages)
        let user_chunks = translated_byte_buffer(token, buf as *const u8, len).or_errno(ERRNO::EFAULT)?;

        // If not seeded yet, handle blocking/non-blocking semantics.
        if !crate::random::is_seeded() {
            if (flags & GRND_NONBLOCK) != 0 {
                return Err(ERRNO::EAGAIN);
            }
            // Block until seeded (woken by random::add_entropy)
            crate::random::wait_for_seed(true)?;
        }

        let mut total = 0usize;
        for chunk in user_chunks {
            if !chunk.is_empty() {
                crate::random::fill_bytes(chunk);
                total = total.checked_add(chunk.len()).ok_or(ERRNO::EINVAL)?;
            }
        }
        debug!("sys_getrandom: filled {} bytes", total);
        Ok(total as isize)
    })
}
