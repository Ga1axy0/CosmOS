use crate::mm::PageFaultAccess;
use crate::random::wait_for_seed;
use crate::syscall::errno::ERRNO;
use crate::syscall::translated_byte_buffer_with_access;
use crate::syscall_body;

const GRND_NONBLOCK: usize = 0x0001;
const GRND_RANDOM: usize = 0x0004; // supported but treated same as default

/// Minimal getrandom syscall: fill user buffer with CSPRNG bytes.
pub fn sys_getrandom(buf: *mut u8, len: usize, flags: usize) -> isize {
    trace!("kernel: sys_getrandom");
    syscall_body!({
        if flags & !(GRND_NONBLOCK | GRND_RANDOM) != 0 {
            return Err(ERRNO::EINVAL);
        }
        // Translate user buffer (may be discontiguous across pages)
        let user_chunks =
            translated_byte_buffer_with_access(buf as *const u8, len, PageFaultAccess::Write)?;

        // If not seeded yet, handle blocking/non-blocking semantics.
        if !crate::random::is_seeded() {
            if (flags & GRND_NONBLOCK) != 0 {
                return Err(ERRNO::EAGAIN);
            }
            // Block until seeded (woken by random::add_entropy)
            wait_for_seed(true)?;
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
