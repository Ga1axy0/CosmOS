use crate::fs::{
    AT_EMPTY_PATH, AT_FDCWD, AT_REMOVEDIR, AT_SYMLINK_FOLLOW, AT_SYMLINK_NOFOLLOW, AccessMode, File,
    FileDescription, FileStatusFlags, InodeTime, OpenFlags, Stat, StatMode, canonicalize, do_umount,
    inode_stat, linkat_with_flags, lookup_inode_follow, make_pipe, mkdir_at, mount_device, truncate_inode,
    rename_at,
    open_file_at, symlinkat, unlinkat,
};
use crate::mm::{PageFaultAccess, UserBuffer, translated_byte_buffer, translated_refmut, translated_str};
use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall::times::Timespec;
use crate::syscall::{translated_byte_buffer_with_access, write_bytes_to_user, write_pod_to_user};
use crate::syscall_body;
use crate::poll::{self, PollWakeState};
use crate::task::{
    block_current_and_run_next, current_process, current_task, current_user_token, FdEntry,
    FdFlags, WaitReason, SIG_DFL, SIG_IGN,
};
use crate::timer::{get_realtime_ns, get_time_us};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::{mem::size_of, slice};
use crate::timer::{add_timer, add_timer_with_poll_tag, get_time_ms};
use crate::task::SignalFlags;
use crate::syscall::OldTimespec32;

/// `writev` 使用的用户态向量缓冲区描述符。
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(super) struct IoVec {
    /// 用户缓冲区起始地址。
    iov_base: usize,
    /// 用户缓冲区长度。
    iov_len: usize,
}

/// `ppoll(2)` 使用的用户态文件描述符数组元素。
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct PollFd {
    /// 被监视的文件描述符。
    fd: i32,
    /// 期望事件掩码（POLLIN/POLLOUT/...）。
    events: i16,
    /// 已经完成的事件（由内核回填）
    revents: i16,
}



const POLLIN: u16 = 0x001;  // readable
const POLLPRI: u16 = 0x002; // urgent data / exceptional condition
const POLLOUT: u16 = 0x004; // writable
const POLLERR: u16 = 0x008; // error
const POLLHUP: u16 = 0x010; // hung up
const POLLNVAL: u16 = 0x020;    // invalid fd
const SELECT_READ_REVENTS: u16 = POLLIN | POLLERR | POLLHUP;
const SELECT_WRITE_REVENTS: u16 = POLLOUT | POLLERR | POLLHUP;
const SELECT_EXCEPT_REVENTS: u16 = POLLPRI;
/// 事件注册表耗尽时，回退轮询的休眠步长（毫秒）。
const PPOLL_FALLBACK_POLL_MS: usize = 10;
/// 单次 `ppoll`/`poll` 调用允许的最大 fd 数量上限，用于防止恶意的大规模分配。
const MAX_POLL_NFDS: usize = 4096;
const FD_SET_BITS_PER_WORD: usize = usize::BITS as usize;

#[derive(Clone, Copy, Debug, Default)]
struct PselectFdMeta {
    read: bool,
    write: bool,
    except: bool,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct PselectSigmaskArg {
    sigmask: *const u8,
    sigsetsize: usize,
}

/// 从用户态复制 `pollfd` 数组，兼容跨页布局。
fn copy_user_pollfds(token: usize, ufds: *mut PollFd, nfds: usize) -> Result<Vec<PollFd>, ERRNO> {
    if nfds == 0 {
        return Ok(Vec::new());
    }
    // 防止用户态传入过大的 nfds 造成巨量内存分配或 panic。
    if nfds > MAX_POLL_NFDS {
        return Err(ERRNO::EINVAL);
    }
    let bytes_len = size_of::<PollFd>()
        .checked_mul(nfds)
        .ok_or(ERRNO::EINVAL)?;
    let bytes = translated_byte_buffer(token, ufds as *const u8, bytes_len)
        .or_errno(ERRNO::EFAULT)?;

    let mut pollfds: Vec<PollFd> = Vec::new();
    // 使用可失败分配，避免在内存不足时 panic。
    pollfds
        .try_reserve_exact(nfds)
        .map_err(|_| ERRNO::ENOMEM)?;
    let mut scratch = [0u8; size_of::<PollFd>()];
    let mut scratch_len = 0usize;
    for chunk in bytes {
        let mut off = 0usize;
        while off < chunk.len() {
            let copy_len = (size_of::<PollFd>() - scratch_len).min(chunk.len() - off);
            scratch[scratch_len..scratch_len + copy_len]
                .copy_from_slice(&chunk[off..off + copy_len]);
            scratch_len += copy_len;
            off += copy_len;
            if scratch_len == size_of::<PollFd>() {
                let pfd = unsafe { core::ptr::read_unaligned(scratch.as_ptr() as *const PollFd) };
                pollfds.push(pfd);
                scratch_len = 0;
                if pollfds.len() == nfds {
                    break;
                }
            }
        }
        if pollfds.len() == nfds {
            break;
        }
    }
    if scratch_len != 0 || pollfds.len() != nfds {
        return Err(ERRNO::EFAULT);
    }
    Ok(pollfds)
}

/// 将内核中的 `pollfd` 数组（主要是 `revents`）回写到用户态。
fn write_back_pollfds(token: usize, ufds: *mut PollFd, pollfds: &[PollFd]) -> Result<(), ERRNO> {
    if pollfds.is_empty() {
        return Ok(());
    }

    // 仅回写 `revents` 字段，保持用户态传入的 `fd` / `events` 不变，
    // 以符合 poll/ppoll 语义并避免覆盖并发更新。
    for (i, pfd) in pollfds.iter().enumerate() {
        let user_pfd_ptr = unsafe { ufds.add(i) };
        // 如果任意一个元素的用户态地址翻译失败，则返回 EFAULT。
        let user_pfd = translated_refmut(token, user_pfd_ptr).or_errno(ERRNO::EFAULT)?;
        user_pfd.revents = pfd.revents;
    }
    Ok(())
}

/// 扫描 fd 集，更新每个 `pollfd.revents` 并返回已就绪计数。
fn scan_pollfds(pollfds: &mut [PollFd]) -> usize {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let mut ready_cnt = 0usize;

    for pfd in pollfds.iter_mut() {
        pfd.revents = 0;
        if pfd.fd < 0 {
            continue;
        }
        let fd = pfd.fd as usize;
        let Some(file) = inner.fd_table.get(fd).and_then(|f| f.as_ref()) else {
            pfd.revents = POLLNVAL as i16;
            ready_cnt += 1;
            continue;
        };

        let mut revents = file.desc.poll(pfd.events as u16);
        if !file.desc.readable() && (pfd.events as u16 & POLLIN) != 0 {
            revents |= POLLERR;
        }
        if !file.desc.writable() && (pfd.events as u16 & POLLOUT) != 0 {
            revents |= POLLERR;
        }
        // pipe 实现可能设置 POLLHUP；普通文件默认走 POLLIN/POLLOUT。
        if (revents & POLLHUP) != 0 {
            pfd.revents = (pfd.revents as u16 | POLLHUP) as i16;
        }
        pfd.revents |= revents as i16;
        if pfd.revents != 0 {
            ready_cnt += 1;
        }
    }

    ready_cnt
}

fn has_unmasked_pending_signal() -> bool {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let pending = inner.pending_signals & !inner.signal_mask;

    for signum in 1..=crate::task::MAX_SIG {
        let Some(flag) = SignalFlags::from_bits(1u32 << signum) else {
            continue;
        };
        if !pending.contains(flag) {
            continue;
        }

        let action = inner.signal_actions.table[signum];
        if action.handler == SIG_IGN {
            inner.pending_signals &= !flag;
            continue;
        }
        if action.handler == SIG_DFL && flag.check_error().is_none() {
            inner.pending_signals &= !flag;
            continue;
        }
        return true;
    }

    false
}

fn parse_timeout_ms(token: usize, tmo_p: *const OldTimespec32) -> Result<Option<usize>, ERRNO> {
    if tmo_p.is_null() {
        return Ok(None);
    }
    let tmo = translated_refmut(token, tmo_p as *mut OldTimespec32).or_errno(ERRNO::EFAULT)?;
    if tmo.tv_sec < 0 || tmo.tv_nsec < 0 || tmo.tv_nsec >= 1_000_000_000 {
        return Err(ERRNO::EINVAL);
    }
    let sec_ms = (tmo.tv_sec as u64)
        .checked_mul(1_000)
        .ok_or(ERRNO::EINVAL)?;
    let nsec = tmo.tv_nsec as u64;
    let nsec_ms = nsec / 1_000_000;
    let timeout_ms = sec_ms
        .checked_add(nsec_ms)
        .ok_or(ERRNO::EINVAL)?;
    Ok(Some(timeout_ms as usize))
}

fn timeout_ms_to_deadline(timeout_ms: Option<usize>) -> Result<Option<usize>, ERRNO> {
    match timeout_ms {
        None => Ok(None),
        Some(ms) => get_time_ms().checked_add(ms).map(Some).ok_or(ERRNO::EINVAL),
    }
}

fn apply_temp_signal_mask(
    token: usize,
    sigmask: *const u8,
    sigsetsize: usize,
    syscall_name: &str,
) -> Result<Option<SignalFlags>, ERRNO> {
    if sigmask.is_null() {
        return Ok(None);
    }
    if sigsetsize < size_of::<u32>() {
        warn!(
            "{}: sigsetsize {} too small for u32 mask",
            syscall_name,
            sigsetsize
        );
        return Err(ERRNO::EINVAL);
    }
    let new_mask_bits = *translated_refmut(token, sigmask as *mut u32).or_errno(ERRNO::EFAULT)?;
    let new_mask = SignalFlags::from_bits(new_mask_bits).or_errno(ERRNO::EINVAL)?;
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let old = inner.signal_mask;
    inner.signal_mask = new_mask;
    Ok(Some(old))
}

fn restore_temp_signal_mask(old_mask: Option<SignalFlags>) {
    if let Some(old) = old_mask {
        current_process().inner_exclusive_access().signal_mask = old;
    }
}

fn poll_wait_loop_with_writeback<F>(
    pid: usize,
    pollfds: &mut [PollFd],
    deadline: Option<usize>,
    mut write_back: F,
) -> Result<isize, ERRNO>
where
    F: FnMut(&[PollFd]) -> Result<(), ERRNO>,
{
    loop {
        let ready = scan_pollfds(pollfds);
        write_back(pollfds)?;
        if ready > 0 {
            return Ok(ready as isize);
        }

        if has_unmasked_pending_signal() {
            return Err(ERRNO::EINTR);
        }

        let now_ms = get_time_ms();
        if let Some(dl) = deadline {
            if now_ms >= dl {
                return Ok(0);
            }
        }

        let task = current_task().unwrap();
        let interests = {
            let process = current_process();
            let inner = process.inner_exclusive_access();
            let mut rows = Vec::new();
            for pfd in pollfds.iter() {
                if pfd.fd < 0 {
                    continue;
                }
                let fd = pfd.fd as usize;
                if let Some(entry) = inner.fd_table.get(fd).and_then(|slot| slot.as_ref()) {
                    rows.push((fd, entry.desc.poll_source_id(), pfd.events as u16));
                }
            }
            rows
        };

        let handle = match poll::register_poll_wait(pid, &task, &interests) {
            Ok(handle) => handle,
            Err(ERRNO::ENOSPC) => {
                // 回退路径：全局 poll 键/行耗尽时，短周期睡眠后重新扫描 fd 集，
                // 避免直接失败，同时不引入忙等。
                let sleep_until = if let Some(dl) = deadline {
                    let remain = dl.saturating_sub(now_ms);
                    let step = PPOLL_FALLBACK_POLL_MS.min(remain);
                    now_ms.saturating_add(step)
                } else {
                    now_ms.saturating_add(PPOLL_FALLBACK_POLL_MS)
                };
                add_timer(sleep_until, Arc::clone(&task));
                block_current_and_run_next(WaitReason::Poll);
                continue;
            }
            Err(e) => return Err(e),
        };
        if let Some(dl) = deadline {
            add_timer_with_poll_tag(dl, Arc::clone(&task), Some(handle.timer_tag()));
        }
        poll::wait_poll_key(handle);

        let wake_state = poll::poll_wait_state(handle);
        poll::cleanup_poll_wait(handle);

        if matches!(wake_state, PollWakeState::TimedOut) {
            return Ok(0);
        }
    }
}

fn fd_set_word_count(nfds: usize) -> Result<usize, ERRNO> {
    nfds
        .checked_add(FD_SET_BITS_PER_WORD - 1)
        .ok_or(ERRNO::EINVAL)
        .map(|x| x / FD_SET_BITS_PER_WORD)
}

fn copy_user_fdset_words(
    token: usize,
    set: *const usize,
    nfds: usize,
) -> Result<Option<Vec<usize>>, ERRNO> {
    if set.is_null() {
        return Ok(None);
    }
    let words = fd_set_word_count(nfds)?;
    if words == 0 {
        return Ok(Some(Vec::new()));
    }
    let bytes_len = words
        .checked_mul(size_of::<usize>())
        .ok_or(ERRNO::EINVAL)?;
    let bytes = translated_byte_buffer(token, set as *const u8, bytes_len).or_errno(ERRNO::EFAULT)?;
    let mut raw = Vec::new();
    raw.try_reserve_exact(bytes_len).map_err(|_| ERRNO::ENOMEM)?;
    for chunk in bytes {
        raw.extend_from_slice(chunk);
    }
    if raw.len() != bytes_len {
        return Err(ERRNO::EFAULT);
    }

    let mut words_vec = Vec::new();
    words_vec.try_reserve_exact(words).map_err(|_| ERRNO::ENOMEM)?;
    for i in 0..words {
        let off = i * size_of::<usize>();
        let value = unsafe { core::ptr::read_unaligned(raw[off..].as_ptr() as *const usize) };
        words_vec.push(value);
    }

    if let Some(last) = words_vec.last_mut() {
        let valid_bits = nfds % FD_SET_BITS_PER_WORD;
        if valid_bits != 0 {
            *last &= (1usize << valid_bits) - 1;
        }
    }
    Ok(Some(words_vec))
}

fn write_user_fdset_words(set: *mut usize, words: &[usize]) -> Result<(), ERRNO> {
    if set.is_null() || words.is_empty() {
        return Ok(());
    }
    let bytes_len = words
        .len()
        .checked_mul(size_of::<usize>())
        .ok_or(ERRNO::EINVAL)?;
    let bytes = unsafe { slice::from_raw_parts(words.as_ptr() as *const u8, bytes_len) };
    write_bytes_to_user(set as *mut u8, bytes)
}

#[inline]
fn fdset_test_bit(words: &[usize], fd: usize) -> bool {
    let word = fd / FD_SET_BITS_PER_WORD;
    let bit = fd % FD_SET_BITS_PER_WORD;
    words
        .get(word)
        .map(|w| (w & (1usize << bit)) != 0)
        .unwrap_or(false)
}

#[inline]
fn fdset_set_bit(words: &mut [usize], fd: usize) {
    let word = fd / FD_SET_BITS_PER_WORD;
    let bit = fd % FD_SET_BITS_PER_WORD;
    if let Some(dst) = words.get_mut(word) {
        *dst |= 1usize << bit;
    }
}

fn build_pselect_pollfds(
    nfds: usize,
    read_set: Option<&[usize]>,
    write_set: Option<&[usize]>,
    except_set: Option<&[usize]>,
) -> Result<(Vec<PollFd>, Vec<PselectFdMeta>), ERRNO> {
    let mut pollfds = Vec::new();
    pollfds.try_reserve_exact(nfds).map_err(|_| ERRNO::ENOMEM)?;
    let mut metas = Vec::new();
    metas.try_reserve_exact(nfds).map_err(|_| ERRNO::ENOMEM)?;

    for fd in 0..nfds {
        let mut events: u16 = 0;
        let mut meta = PselectFdMeta::default();

        if let Some(read_words) = read_set {
            if fdset_test_bit(read_words, fd) {
                events |= POLLIN;
                meta.read = true;
            }
        }
        if let Some(write_words) = write_set {
            if fdset_test_bit(write_words, fd) {
                events |= POLLOUT;
                meta.write = true;
            }
        }
        if let Some(except_words) = except_set {
            if fdset_test_bit(except_words, fd) {
                events |= POLLPRI;
                meta.except = true;
            }
        }

        if events == 0 {
            continue;
        }
        pollfds.push(PollFd {
            fd: fd as i32,
            events: events as i16,
            revents: 0,
        });
        metas.push(meta);
    }
    Ok((pollfds, metas))
}

fn validate_pselect_fds(
    nfds: usize,
    read_set: Option<&[usize]>,
    write_set: Option<&[usize]>,
    except_set: Option<&[usize]>,
) -> Result<(), ERRNO> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    for fd in 0..nfds {
        let monitored = read_set.map(|s| fdset_test_bit(s, fd)).unwrap_or(false)
            || write_set.map(|s| fdset_test_bit(s, fd)).unwrap_or(false)
            || except_set.map(|s| fdset_test_bit(s, fd)).unwrap_or(false);
        if !monitored {
            continue;
        }
        if inner
            .fd_table
            .get(fd)
            .and_then(|entry| entry.as_ref())
            .is_none()
        {
            return Err(ERRNO::EBADF);
        }
    }
    Ok(())
}

fn write_back_pselect_fdsets(
    readfds: *mut usize,
    writefds: *mut usize,
    exceptfds: *mut usize,
    nfds: usize,
    pollfds: &[PollFd],
    metas: &[PselectFdMeta],
) -> Result<(), ERRNO> {
    let words = fd_set_word_count(nfds)?;
    let mut read_out = if readfds.is_null() {
        None
    } else {
        let mut out = Vec::new();
        out.try_reserve_exact(words).map_err(|_| ERRNO::ENOMEM)?;
        out.resize(words, 0);
        Some(out)
    };
    let mut write_out = if writefds.is_null() {
        None
    } else {
        let mut out = Vec::new();
        out.try_reserve_exact(words).map_err(|_| ERRNO::ENOMEM)?;
        out.resize(words, 0);
        Some(out)
    };
    let mut except_out = if exceptfds.is_null() {
        None
    } else {
        let mut out = Vec::new();
        out.try_reserve_exact(words).map_err(|_| ERRNO::ENOMEM)?;
        out.resize(words, 0);
        Some(out)
    };

    for (pfd, meta) in pollfds.iter().zip(metas.iter()) {
        if pfd.fd < 0 {
            continue;
        }
        let fd = pfd.fd as usize;
        if fd >= nfds {
            continue;
        }
        let revents = pfd.revents as u16;
        if meta.read && (revents & SELECT_READ_REVENTS) != 0 {
            if let Some(out) = read_out.as_mut() {
                fdset_set_bit(out, fd);
            }
        }
        if meta.write && (revents & SELECT_WRITE_REVENTS) != 0 {
            if let Some(out) = write_out.as_mut() {
                fdset_set_bit(out, fd);
            }
        }
        if meta.except && (revents & SELECT_EXCEPT_REVENTS) != 0 {
            if let Some(out) = except_out.as_mut() {
                fdset_set_bit(out, fd);
            }
        }
    }

    // 显式按参数顺序回写，保持与用户态入参一一对应。
    if let Some(out) = read_out.as_ref() {
        write_user_fdset_words(readfds, out)?;
    }
    if let Some(out) = write_out.as_ref() {
        write_user_fdset_words(writefds, out)?;
    }
    if let Some(out) = except_out.as_ref() {
        write_user_fdset_words(exceptfds, out)?;
    }
    Ok(())
}

fn parse_pselect_sigmask_arg(
    token: usize,
    sigmask_arg: *const u8,
) -> Result<(*const u8, usize), ERRNO> {
    if sigmask_arg.is_null() {
        return Ok((core::ptr::null(), 0));
    }
    let arg = translated_refmut(token, sigmask_arg as *mut PselectSigmaskArg).or_errno(ERRNO::EFAULT)?;
    Ok((arg.sigmask, arg.sigsetsize))
}

/// 打开路径后需要挂入 `FileDescription` 的打开参数。
struct OpenFileState {
    /// 打开时确定的访问模式。
    access_mode: AccessMode,
    /// 当前可变文件状态位。
    status_flags: FileStatusFlags,
    /// `F_GETFL` 需要保留返回的固定状态位。
    status_fixed_bits: i32,
    /// 传给底层 inode 打开的标志位。
    open_flags: OpenFlags,
}

/// 校验 fd 并返回打开文件描述。
fn get_file_description(fd: usize) -> Result<Arc<FileDescription>, ERRNO> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return Err(ERRNO::EBADF);
    }
    let desc = inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.desc.clone();
    drop(inner);
    Ok(desc)
}

/// 校验 fd 并返回可写打开文件描述。
fn get_writable_file(fd: usize) -> Result<Arc<FileDescription>, ERRNO> {
    let desc = get_file_description(fd)?;
    if !desc.writable() {
        return Err(ERRNO::EACCES);
    }
    Ok(desc)
}

/// 校验 fd 并返回可读打开文件描述。
fn get_readable_file(fd: usize) -> Result<Arc<FileDescription>, ERRNO> {
    let desc = get_file_description(fd)?;
    if !desc.readable() {
        return Err(ERRNO::EACCES);
    }
    Ok(desc)
}

/// 解析 truncate 系统调用传入的目标长度。
fn parse_truncate_len(len: isize) -> Result<usize, ERRNO> {
    if len < 0 {
        return Err(ERRNO::EINVAL);
    }
    Ok(len as usize)
}

/// 从用户态复制 `iovec` 数组，避免数组跨页时直接解引用失败。
pub fn copy_user_iovecs(token: usize, iov: *const IoVec, iovcnt: i32) -> Result<Vec<IoVec>, ERRNO> {
    if iovcnt < 0 {
        return Err(ERRNO::EINVAL);
    }
    if iovcnt == 0 {
        return Ok(Vec::new());
    }
    let iovcnt = iovcnt as usize;
    let iov_bytes_len: usize = size_of::<IoVec>()
        .checked_mul(iovcnt)
        .ok_or(ERRNO::EINVAL)?;
    let iov_bytes = translated_byte_buffer(token, iov as *const u8, iov_bytes_len)
        .or_errno(ERRNO::EFAULT)?;
    let mut iovecs = Vec::with_capacity(iovcnt);
    let mut scratch = [0u8; size_of::<IoVec>()];
    let mut scratch_len = 0usize;
    // 以IoVec为单位平接。可能出现一个u8分属两个不同IoVec，所以下面按照offset一点点拼凑每一个IoVec。
    for chunk in iov_bytes {
        let mut chunk_offset = 0usize;
        while chunk_offset < chunk.len() {
            let copy_len = (size_of::<IoVec>() - scratch_len).min(chunk.len() - chunk_offset);
            // 逐步拼出一个完整的 `IoVec`，以兼容结构体跨页的情况。
            scratch[scratch_len..scratch_len + copy_len]
                .copy_from_slice(&chunk[chunk_offset..chunk_offset + copy_len]);
            scratch_len += copy_len;
            chunk_offset += copy_len;
            if scratch_len == size_of::<IoVec>() {
                // 这里按 C ABI 逐项复制，避免直接依赖用户地址对齐。
                let iovec = unsafe { core::ptr::read_unaligned(scratch.as_ptr() as *const IoVec) };
                iovecs.push(iovec);
                scratch_len = 0;
                if iovecs.len() == iovcnt {
                    break;
                }
            }
        }
        if iovecs.len() == iovcnt {
            break;
        }
    }
    if scratch_len != 0 || iovecs.len() != iovcnt {
        // 正常情况下长度应严格对齐；若触发说明用户内存翻译结果异常。
        return Err(ERRNO::EFAULT);
    }
    Ok(iovecs)
}

/// 从用户态复制 `utimensat` 需要的两个 `timespec`，允许结构体跨页。
fn copy_user_timespec_pair(token: usize, times: *const Timespec) -> Result<[Timespec; 2], ERRNO> {
    let raw_len = 2 * size_of::<Timespec>();
    let raw_chunks = translated_byte_buffer(token, times as *const u8, raw_len)
        .or_errno(ERRNO::EFAULT)?;
    let mut raw = [0u8; 2 * size_of::<Timespec>()];
    let mut copied = 0usize;
    for chunk in raw_chunks {
        if copied >= raw.len() {
            break;
        }
        let take = (raw.len() - copied).min(chunk.len());
        raw[copied..copied + take].copy_from_slice(&chunk[..take]);
        copied += take;
    }
    if copied != raw.len() {
        return Err(ERRNO::EFAULT);
    }
    let atime = unsafe { core::ptr::read_unaligned(raw.as_ptr() as *const Timespec) };
    let mtime = unsafe {
        core::ptr::read_unaligned(raw[size_of::<Timespec>()..].as_ptr() as *const Timespec)
    };
    Ok([atime, mtime])
}

/// 解析 `utimensat` 的单个时间参数。
fn parse_utime_arg(ts: Timespec) -> Result<UtimeArg, ERRNO> {
    match ts.tv_nsec {
        UTIME_NOW => Ok(UtimeArg::Now),
        UTIME_OMIT => Ok(UtimeArg::Omit),
        nsec if nsec < 1_000_000_000 => Ok(UtimeArg::Set(InodeTime::new(ts.tv_sec as u64, nsec as u32))),
        _ => Err(ERRNO::EINVAL),
    }
}

/// Result of resolving a (dirfd, path, flags) target.
enum ResolvedAtTarget {
    /// A concrete filesystem inode identified by path or fd.
    Inode(Arc<fs::Inode>),
    /// An open file description (fd) that does not map to a filesystem inode.
    FileDesc(Arc<FileDescription>),
}

/// Resolve `dirfd + path + flags` into either a filesystem inode or an
/// open `FileDescription` when the fd does not correspond to an inode.
///
/// Semantics:
/// - If `path` is empty, `AT_EMPTY_PATH` must be set. If `dirfd == AT_FDCWD`,
///   resolve the current working directory's inode. If `dirfd` is an fd, try
///   to return the underlying inode (if the `FileDescription` wraps one);
///   otherwise return the `FileDescription` itself so callers can operate on
///   the open descriptor.
/// - If `path` is non-empty, resolve against `dirfd` (or CWD) and return the
///   corresponding inode or `ENOENT`.
fn resolve_at_target(dirfd: isize, path: &str, flags: i32) -> Result<ResolvedAtTarget, ERRNO> {
    if path.is_empty() {
        if flags & AT_EMPTY_PATH as i32 == 0 {
            return Err(ERRNO::ENOENT);
        }
        if dirfd == AT_FDCWD {
            let cwd = current_process().inner_exclusive_access().cwd.clone();
            return lookup_inode_follow("/", cwd.as_str(), true).map(ResolvedAtTarget::Inode);
        }
        if dirfd < 0 {
            return Err(ERRNO::EBADF);
        }
        let desc = get_file_description(dirfd as usize)?;
        // Prefer returning the underlying inode if available (e.g. OSInode).
        if let Some(inode) = desc.as_inode() {
            return Ok(ResolvedAtTarget::Inode(inode));
        }
        // Fall back to path-based lookup if the description recorded an open path.
        if let Some(target_path) = desc.path() {
            return lookup_inode_follow("/", target_path.as_str(), true).map(ResolvedAtTarget::Inode);
        }
        // Otherwise return the open description itself (e.g. pipe/tty).
        return Ok(ResolvedAtTarget::FileDesc(desc));
    }

    let cwd = resolve_dirfd_base(dirfd, path)?;
    let follow_final = flags & AT_SYMLINK_NOFOLLOW as i32 == 0;
    lookup_inode_follow(cwd.as_str(), path, follow_final).map(ResolvedAtTarget::Inode)
}

fn resolve_dirfd_base(dirfd: isize, path: &str) -> Result<String, ERRNO> {
    if path.starts_with('/') {
        return Ok(String::from("/"));
    }
    let process = current_process();
    if dirfd == AT_FDCWD {
        return Ok(process.inner_exclusive_access().cwd.clone());
    }
    if dirfd < 0 {
        return Err(ERRNO::EBADF);
    }
    let inner = process.inner_exclusive_access();
    let desc = inner
        .fd_table
        .get(dirfd as usize)
        .and_then(|entry| entry.as_ref())
        .map(|entry| entry.desc.clone())
        .ok_or(ERRNO::EBADF)?;
    drop(inner);
    if !desc.is_dir() {
        return Err(ERRNO::ENOTDIR);
    }
    desc.path().ok_or(ERRNO::ENOTDIR)
}

fn resolve_chown_ids(inode: &Arc<fs::Inode>, uid: u32, gid: u32) -> Result<(u32, u32), ERRNO> {
    if uid == UID_GID_NO_CHANGE && gid == UID_GID_NO_CHANGE {
        return Ok((uid, gid));
    }
    let attrs = inode.stat_attrs();
    let mut new_uid = uid;
    let mut new_gid = gid;
    if uid == UID_GID_NO_CHANGE {
        new_uid = attrs.uid.or_else(|| inode.uid()).ok_or(ERRNO::EOPNOTSUPP)?;
    }
    if gid == UID_GID_NO_CHANGE {
        new_gid = attrs.gid.or_else(|| inode.gid()).ok_or(ERRNO::EOPNOTSUPP)?;
    }
    Ok((new_uid, new_gid))
}

const F_DUPFD: i32 = 0;
const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const F_DUPFD_CLOEXEC: i32 = 1030;

const F_OK: i32 = 0;
const X_OK: i32 = 1;
const W_OK: i32 = 2;
const R_OK: i32 = 4;

const UTIME_NOW: usize = 0x3fff_ffff;
const UTIME_OMIT: usize = 0x3fff_fffe;
const UID_GID_NO_CHANGE: u32 = u32::MAX;

#[derive(Clone, Copy)]
enum UtimeArg {
    Now,
    Omit,
    Set(InodeTime),
}

/// `fcntl(F_GETFD/F_SETFD)` 可见的 fd 标志位。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
enum FcntlFdFlag {
    /// `exec` 成功后自动关闭该 fd。
    Cloexec = 0x1,
}

impl FcntlFdFlag {
    /// 汇总所有当前已识别的 fd 标志位掩码。
    const ALL_BITS: i32 = Self::Cloexec as i32;
}

/// 过滤并校验 `openat` 的路径打开语义位。
fn filter_open_flags(flags: i32) -> Result<OpenFileState, ERRNO> {
    const O_APPEND: i32 = FileStatusFlags::APPEND.bits();
    const O_NOCTTY: i32 = 0x100;
    const O_NONBLOCK: i32 = FileStatusFlags::NONBLOCK.bits();
    const O_LARGEFILE: i32 = 0x8000;
    const O_DIRECTORY: i32 = OpenFlags::DIRECTORY.bits();

    let access_mode = AccessMode::from_open_bits(flags)?;
    let ignored_flags = flags & O_LARGEFILE;
    let unsupported_flags = flags & O_NOCTTY;
    let status_flags = FileStatusFlags::from_bits_truncate(flags & (O_APPEND | O_NONBLOCK));
    let effective_flags = flags & !(ignored_flags | O_APPEND | O_NONBLOCK);

    if unsupported_flags != 0 {
        // TODO: 后续若补齐 tty 控制终端语义，应在进程/会话层实现真实行为。
        warn!(
            "sys_open: unsupported open flags {:#x}",
            unsupported_flags
        );
        return Err(ERRNO::EINVAL);
    }
    if ignored_flags != 0 {
        // TODO: 后续若补齐大文件兼容细节，可在这里补充更精细的位语义校验。
        warn!(
            "sys_open: ignore ignorable open flags {:#x}",
            ignored_flags
        );
    }
    let open_flags = OpenFlags::from_bits(effective_flags).ok_or(ERRNO::EINVAL)?;
    Ok(OpenFileState {
        access_mode,
        status_flags,
        status_fixed_bits: effective_flags & O_DIRECTORY,
        open_flags,
    })
}

/// 解析 `F_SETFL` 可修改的文件状态位。
fn parse_setfl_status(arg: usize) -> Result<FileStatusFlags, ERRNO> {
    let arg = i32::try_from(arg).map_err(|_| ERRNO::EINVAL)?;
    let mutable_mask = FileStatusFlags::APPEND.bits() | FileStatusFlags::NONBLOCK.bits();
    let ignored_mask =
        0x3 | OpenFlags::CREATE.bits() | OpenFlags::TRUNC.bits() | OpenFlags::DIRECTORY.bits() | OpenFlags::NOFOLLOW.bits() | 0x100 | 0x8000;
    if arg & !(mutable_mask | ignored_mask) != 0 {
        // TODO: 后续若补齐 `O_ASYNC/O_DIRECT/O_NOATIME` 等位，应在这里扩展掩码。
        warn!(
            "sys_fcntl: unsupported F_SETFL flags {:#x}",
            arg & !(mutable_mask | ignored_mask)
        );
        return Err(ERRNO::EINVAL);
    }
    Ok(FileStatusFlags::from_bits_truncate(arg))
}

/// `fcntl` 系统调用：处理 fd 标志、文件状态位与描述复制。
pub fn sys_fcntl(fd: u32, cmd: i32, arg: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_fcntl",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    syscall_body!({
        let fd = fd as usize;
        let mut inner = process.inner_exclusive_access();
        if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
            return Err(ERRNO::EBADF);
        }
        match cmd {
            F_GETFD => {
                let mut flags = 0i32;
                let entry = inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?;
                if entry.flags.contains(FdFlags::CLOEXEC) {
                    flags |= FcntlFdFlag::Cloexec as i32;
                }
                Ok(flags as isize)
            }
            F_SETFD => {
                let entry = inner.fd_table[fd].as_mut().ok_or(ERRNO::EBADF)?;
                let arg = i32::try_from(arg).map_err(|_| ERRNO::EINVAL)?;
                if arg & !FcntlFdFlag::ALL_BITS != 0 {
                    // TODO: 后续若补齐额外 fd 标志位，应在这里扩展掩码并同步到 `FdFlags`。
                    warn!(
                        "sys_fcntl: unsupported F_SETFD flags {:#x}",
                        arg & !FcntlFdFlag::ALL_BITS
                    );
                    return Err(ERRNO::EINVAL);
                }
                if arg & (FcntlFdFlag::Cloexec as i32) != 0 {
                    entry.flags |= FdFlags::CLOEXEC;
                } else {
                    entry.flags.remove(FdFlags::CLOEXEC);
                }
                Ok(0)
            }
            F_DUPFD => {
                let min_fd = i32::try_from(arg).map_err(|_| ERRNO::EINVAL)?;
                if min_fd < 0 {
                    return Err(ERRNO::EINVAL);
                }
                let desc = Arc::clone(&inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.desc);
                let new_fd = inner.alloc_fd_from(min_fd as usize)?;
                inner.fd_table[new_fd] = Some(FdEntry {
                    desc,
                    flags: FdFlags::empty(),
                });
                Ok(new_fd as isize)
            }
            F_GETFL => {
                let entry = inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?;
                Ok(entry.desc.status_bits() as isize)
            }
            F_SETFL => {
                let desc = Arc::clone(&inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.desc);
                let status_flags = parse_setfl_status(arg)?;
                desc.set_status_flags(status_flags);
                Ok(0)
            }
            F_DUPFD_CLOEXEC => {
                let min_fd = i32::try_from(arg).map_err(|_| ERRNO::EINVAL)?;
                if min_fd < 0 {
                    return Err(ERRNO::EINVAL);
                }
                let desc = Arc::clone(&inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.desc);
                let new_fd = inner.alloc_fd_from(min_fd as usize)?;
                inner.fd_table[new_fd] = Some(FdEntry {
                    desc,
                    flags: FdFlags::CLOEXEC,
                });
                Ok(new_fd as isize)
            }
            _ => Err(ERRNO::EINVAL),
        }
    })
}

/// write syscall
pub fn sys_write(fd: u32, buf: *const u8, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_write",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_writable_file(fd)?;
        Ok(desc.write(UserBuffer::new(
            translated_byte_buffer_with_access(buf, len, PageFaultAccess::Read)?,
        )) as isize)
    })
}

/// readv syscall：按 `iovec` 顺序将多个用户缓冲区从同一个 fd 读出。
pub fn sys_readv(fd: usize, iov: *const IoVec, iovcnt: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_readv",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let fd = fd as usize;
        let desc = get_readable_file(fd)?;
        let iovecs = copy_user_iovecs(token, iov, iovcnt)?;
        let mut read_total = 0usize;
        for &iovec in &iovecs {
            if iovec.iov_len == 0 {
                continue;
            }
            let user_buf = UserBuffer::new(
                translated_byte_buffer_with_access(
                    iovec.iov_base as *const u8,
                    iovec.iov_len,
                    PageFaultAccess::Write,
                )?,
            );
            let read = desc.read(user_buf);
            read_total = read_total.checked_add(read).ok_or(ERRNO::EINVAL)?;
            if read < iovec.iov_len {
                break;
            }
        }
        Ok(read_total as isize)
    })
}

/// writev syscall：按 `iovec` 顺序将多个用户缓冲区写入同一个 fd。
pub fn sys_writev(fd: usize, iov: *const IoVec, iovcnt: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_writev",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let fd = fd as usize;
        let desc = get_writable_file(fd)?;
        let iovecs = copy_user_iovecs(token, iov, iovcnt)?;
        let mut written_total = 0usize;
        for &iovec in &iovecs {
            written_total = written_total
                .checked_add(iovec.iov_len)
                .ok_or(ERRNO::EINVAL)?;
            if written_total > isize::MAX as usize {
                return Err(ERRNO::EINVAL);
            }
        }
        let mut completed = 0usize;
        for &iovec in &iovecs {
            if iovec.iov_len == 0 {
                continue;
            }
            let user_buf = UserBuffer::new(
                translated_byte_buffer_with_access(
                    iovec.iov_base as *const u8,
                    iovec.iov_len,
                    PageFaultAccess::Read,
                )?,
            );
            let written = desc.write(user_buf);
            completed += written;
            // 发生短写时立即返回，保留与 `write` 一致的部分写入语义。
            if written < iovec.iov_len {
                return Ok(completed as isize);
            }
        }
        // TODO: 当前未限制 `iovcnt` 上限；若后续补齐 `IOV_MAX`，应在复制前返回 `EINVAL`。
        Ok(completed as isize)
    })
}

/// read syscall
pub fn sys_read(fd: u32, buf: *const u8, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_read, fd = {}",
        current_task().unwrap().process.upgrade().unwrap().getpid(), fd
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_readable_file(fd)?;
        trace!("kernel: sys_read .. desc.read");
        Ok(desc.read(UserBuffer::new(
            translated_byte_buffer_with_access(buf, len, PageFaultAccess::Write)?,
        )) as isize)
    })
}

pub fn sys_lseek(fd: u32, offset: usize, whence: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_lseek, offset={}, whence={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        offset,
        whence
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_file_description(fd)?;
        // Combine high/low into a 64-bit pattern and interpret as signed offset.
        Ok(desc.seek(offset as i64, whence as u8)? as isize)
    })
}

/// ioctl 系统调用：校验 fd 后转发到具体文件对象。
pub fn sys_ioctl(fd: u32, req: usize, arg: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_ioctl",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_file_description(fd)?;
        // 具体 request 语义由底层文件对象决定；当前大多数对象会返回 ENOTTY。
        // TODO: tty 实现 `TCGETS/TIOCGWINSZ` 后，这里会开始承载真实终端控制语义。
        debug!("sys_ioctl: fd = {}, req = {:#x}, arg = {:#x}", fd, req, arg);
        desc.ioctl(req, arg)
    })
}

/// open sysall
pub fn sys_open(dirfd: isize, path: *const u8, flags: i32, _mode: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_open",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let token = current_user_token();
    syscall_body!({
        // TODO: 目前只有O_CLOEXEC位会落入FD层处理。
        const O_CLOEXEC: i32 = 0x80000;
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        if path.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
        debug!(
            "sys_open: dirfd = {}, path = {}, flags = {}, cwd = {}",
            dirfd, path, flags, cwd
        );
        let fd_flags = if flags & O_CLOEXEC != 0 {
            FdFlags::CLOEXEC
        } else {
            FdFlags::empty()
        };
        let open_state = filter_open_flags(flags & !O_CLOEXEC)?;
        let inode = open_file_at(
            cwd.as_str(),
            path.as_str(),
            open_state.open_flags,
        )?;
        if open_state.open_flags.contains(OpenFlags::DIRECTORY) && !inode.is_dir() {
            return Err(ERRNO::ENOTDIR);
        }
        let mut inner = process.inner_exclusive_access();
        let fd = inner.alloc_fd()?;
        let desc = Arc::new(FileDescription::new(
            inode,
            open_state.access_mode,
            open_state.status_flags,
            open_state.status_fixed_bits,
        ));
        let mut entry = FdEntry::new(desc);
        entry.flags = fd_flags;
        inner.fd_table[fd] = Some(entry);
        Ok(fd as isize)
    })
}

/// `truncate(2)`：按路径调整常规文件长度。
pub fn sys_truncate(path: *const u8, len: isize) -> isize {
    trace!(
        "kernel:pid[{}] sys_truncate",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let new_size = parse_truncate_len(len)?;
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        if path.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        debug!("sys_truncate: path='{}', new_size={}", path, new_size);
        let target = resolve_at_target(AT_FDCWD, path.as_str(), 0)?;
        match target {
            ResolvedAtTarget::Inode(inode) => {
                debug!(
                    "sys_truncate: resolved inode fs_id={} ino={}",
                    inode.fs_id(),
                    inode.ino()
                );
                truncate_inode(&inode, new_size).map_err(ERRNO::from)?;
                Ok(0)
            }
            ResolvedAtTarget::FileDesc(_) => Err(ERRNO::EINVAL),
        }
    })
}

/// `ftruncate(2)`：按已打开文件描述调整常规文件长度。
pub fn sys_ftruncate(fd: u32, len: isize) -> isize {
    trace!(
        "kernel:pid[{}] sys_ftruncate",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let new_size = parse_truncate_len(len)?;
        let desc = get_writable_file(fd as usize)?;
        debug!("sys_ftruncate: fd={}, new_size={}", fd, new_size);
        if let Some(inode) = desc.backing_inode() {
            debug!(
                "sys_ftruncate: backing inode fs_id={} ino={}",
                inode.fs_id(),
                inode.ino()
            );
        }
        desc.truncate(new_size)?;
        Ok(0)
    })
}

/// close syscall
pub fn sys_close(fd: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_close",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let closed_entry = {
        let mut inner = process.inner_exclusive_access();
        let mut closed_entry: Option<FdEntry> = None;
        let result = syscall_body!({
            let fd = fd as usize;
            if fd >= inner.fd_table.len() {
                return Err(ERRNO::EBADF);
            }
            if inner.fd_table[fd].is_none() {
                return Err(ERRNO::EBADF);
            }
            // 先摘表项，等离开 `process.inner` 后再真正 drop，避免自旋锁内阻塞。
            let entry = inner.take_fd(fd).ok_or(ERRNO::EBADF)?;
            closed_entry = Some(entry);
            Ok(0)
        });
        if result != 0 {
            return result;
        }
        closed_entry
    };
    drop(closed_entry);
    0
}

/// pipe syscall
pub fn sys_pipe2(pipefd: *mut i32, _flags: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_pipe",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let token = current_user_token();
    syscall_body!({
        let mut inner = process.inner_exclusive_access();
        inner.ensure_fd_capacity(2)?;
        let (pipe_read, pipe_write) = make_pipe();
        let read_fd = inner.alloc_fd()?;
        inner.fd_table[read_fd] = Some(FdEntry::new(Arc::new(FileDescription::new(
            pipe_read,
            AccessMode::ReadOnly,
            FileStatusFlags::empty(),
            0,
        ))));
        let write_fd = inner.alloc_fd()?;
        inner.fd_table[write_fd] = Some(FdEntry::new(Arc::new(FileDescription::new(
            pipe_write,
            AccessMode::WriteOnly,
            FileStatusFlags::empty(),
            0,
        ))));
        drop(inner);
        *translated_refmut(token, pipefd).or_errno(ERRNO::EFAULT)? = read_fd as i32;
        *translated_refmut(token, unsafe { pipefd.add(1) }).or_errno(ERRNO::EFAULT)? = write_fd as i32;
        debug!("sys_pipe: read_fd = {}, write_fd = {}", read_fd, write_fd);
        Ok(0)
    })
}

/// dup syscall
pub fn sys_dup(fd: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_dup",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    syscall_body!({
        let fd = fd as usize;
        if fd >= inner.fd_table.len() {
            return Err(ERRNO::EBADF);
        }
        if inner.fd_table[fd].is_none() {
            return Err(ERRNO::EBADF);
        }
        let new_fd = inner.alloc_fd()?;
        let desc = Arc::clone(&inner.fd_table[fd].as_ref().unwrap().desc);
        inner.fd_table[new_fd] = Some(FdEntry {
            desc,
            flags: FdFlags::empty(),
        });
        Ok(new_fd as isize)
    })
}

/// dup2 syscall
pub fn sys_dup2(oldfd: u32, newfd: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_dup2",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let (result, replaced_entry) = {
        let mut inner = process.inner_exclusive_access();
        let mut replaced_entry: Option<FdEntry> = None;
        let result = syscall_body!({
            let oldfd = oldfd as usize;
            let newfd = newfd as usize;
            if oldfd >= inner.fd_table.len() {
                return Err(ERRNO::EBADF);
            }
            if inner.fd_table[oldfd].is_none() {
                return Err(ERRNO::EBADF);
            }
            if oldfd == newfd {
                return Ok(newfd as isize);
            }
            let newfd_is_occupied = inner.fd_table.get(newfd).and_then(|slot| slot.as_ref()).is_some();
            if !newfd_is_occupied {
                let allocated = inner.alloc_fd_from(newfd)?;
                debug_assert_eq!(allocated, newfd);
            }
            // 先把旧 `newfd` 表项拿出来，等离开进程自旋锁后再 drop。
            replaced_entry = inner.take_fd(newfd);
            let desc = Arc::clone(&inner.fd_table[oldfd].as_ref().unwrap().desc);
            inner.fd_table[newfd] = Some(FdEntry {
                desc,
                flags: FdFlags::empty(),
            });
            Ok(newfd as isize)
        });
        (result, replaced_entry)
    };
    drop(replaced_entry);
    result
}

/// fstat syscall
pub fn sys_fstat(fd: u32, st: *mut Stat) -> isize {
    trace!(
        "kernel:pid[{}] sys_fstat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_file_description(fd)?;
        let stat = desc.stat();
        write_pod_to_user(st, &stat)?;
        Ok(0)
    })
}

/// `newfstatat` 系统调用：按目录 fd 与路径查询文件元数据。
pub fn sys_newfstatat(dirfd: isize, path: *const u8, st: *mut Stat, flags: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_newfstatat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        let flags = flags as u32;
        let supported_flags = AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH;
        if flags & !supported_flags != 0 {
            return Err(ERRNO::EINVAL);
        }
        if path.is_empty() {
            if flags & AT_EMPTY_PATH == 0 {
                return Err(ERRNO::ENOENT);
            }
            // Use the unified resolver which returns either an inode or an
            // open FileDescription (for non-inode descriptors like pipes).
            match resolve_at_target(dirfd, "", flags as i32)? {
                ResolvedAtTarget::Inode(inode) => {
                    let stat = inode_stat(&inode);
                    write_pod_to_user(st, &stat)?;
                    return Ok(0);
                }
                ResolvedAtTarget::FileDesc(desc) => {
                    let stat = desc.stat();
                    write_pod_to_user(st, &stat)?;
                    return Ok(0);
                }
            }
        }
let time1 = get_time_us();
        let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
let time2 = get_time_us();
        let inode = lookup_inode_follow(cwd.as_str(), path.as_str(), flags & AT_SYMLINK_NOFOLLOW == 0)?;
let time3 = get_time_us();
        let stat = inode_stat(&inode);
let time4 = get_time_us();
        write_pod_to_user(st, &stat)?;
let time5 = get_time_us();
        debug!("sys_newfstatat: resolve_dirfd_base & canonicalize = {}us, lookup_inode = {}us, inode_stat = {}us, write_pod_to_user = {}us",
            time2 - time1, time3 - time2, time4 - time3, time5 - time4);
        Ok(0)
    })
}

/// `faccessat` 系统调用：按目录 fd 与路径检查可访问性。
pub fn sys_faccessat(dirfd: isize, path: *const u8, mode: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_faccessat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        if mode & !(R_OK | W_OK | X_OK) != 0 {
            return Err(ERRNO::EINVAL);
        }

        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        if path.is_empty() {
            return Err(ERRNO::ENOENT);
        }

        let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
        let inode = lookup_inode_follow(cwd.as_str(), path.as_str(), true)?;
        if mode == F_OK {
            return Ok(0);
        }

        let process = current_process();
        let uid = process.getuid();
        let gid = process.getgid();
        if inode.check_access(uid, gid, mode as u32) {
            Ok(0)
        } else {
            Err(ERRNO::EACCES)
        }
    })
}

/// linkat syscall
pub fn sys_linkat(
    old_dirfd: isize,
    old_name: *const u8,
    new_dirfd: isize,
    new_name: *const u8,
    flags: u32,
) -> isize {
    trace!(
        "kernel:pid[{}] sys_linkat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        if flags & !AT_SYMLINK_FOLLOW != 0 {
            return Err(ERRNO::EINVAL);
        }
        let old_path = translated_str(token, old_name).or_errno(ERRNO::EFAULT)?;
        let new_path = translated_str(token, new_name).or_errno(ERRNO::EFAULT)?;
        if old_path.is_empty() || new_path.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        if old_path == new_path {
            return Err(ERRNO::EINVAL);
        }
        let old_cwd = resolve_dirfd_base(old_dirfd, old_path.as_str())?;
        let new_cwd = resolve_dirfd_base(new_dirfd, new_path.as_str())?;
        linkat_with_flags(old_cwd.as_str(), &old_path, new_cwd.as_str(), &new_path, flags)?;
        Ok(0)
    })
}

/// symlinkat syscall
pub fn sys_symlinkat(target: *const u8, new_dirfd: isize, linkpath: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_symlinkat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let target = translated_str(token, target).or_errno(ERRNO::EFAULT)?;
        let linkpath = translated_str(token, linkpath).or_errno(ERRNO::EFAULT)?;
        if target.is_empty() || linkpath.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        let cwd = resolve_dirfd_base(new_dirfd, linkpath.as_str())?;
        symlinkat(target.as_str(), cwd.as_str(), linkpath.as_str())?;
        Ok(0)
    })
}

/// readlinkat syscall
pub fn sys_readlinkat(dirfd: isize, path: *const u8, buf: *mut u8, bufsiz: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_readlinkat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        if bufsiz == 0 {
            return Err(ERRNO::EINVAL);
        }
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        if path.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
        let inode = lookup_inode_follow(cwd.as_str(), path.as_str(), false)?;
        if !inode.is_symlink() {
            return Err(ERRNO::EINVAL);
        }
        let target = inode.read_link()?;
        let bytes = target.as_bytes();
        let n = bytes.len().min(bufsiz);
        write_bytes_to_user(buf, &bytes[..n])?;
        Ok(n as isize)
    })
}

/// unlinkat syscall
pub fn sys_unlinkat(dirfd: isize, name: *const u8, flags: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_unlinkat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        if flags & !AT_REMOVEDIR != 0 {
            return Err(ERRNO::EINVAL);
        }
        let name = translated_str(token, name).or_errno(ERRNO::EFAULT)?;
        if name.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        let cwd = resolve_dirfd_base(dirfd, name.as_str())?;
        unlinkat(cwd.as_str(), &name, flags)?;
        Ok(0)
    })
}

/// getcwd – copy the current working directory into a user-space buffer.
///
/// Returns the buffer address as `isize` on success, −errno on failure.
pub fn sys_getcwd(buf: *mut u8, size: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_getcwd",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        if size == 0 || buf.is_null() {
            return Err(ERRNO::EINVAL);
        }
        let cwd = current_process().inner_exclusive_access().cwd.clone();
        let cwd_bytes = cwd.as_bytes();
        if size < cwd_bytes.len() + 1 {
            return Err(ERRNO::ERANGE);
        }
        // Write cwd + null terminator into the user buffer in one pass.
        let total = cwd_bytes.len() + 1;
        let src: Vec<u8> = cwd_bytes
            .iter()
            .copied()
            .chain(core::iter::once(0u8))
            .collect();
        debug_assert_eq!(src.len(), total);
        write_bytes_to_user(buf, &src)?;
        Ok(buf as isize)
    })
}

/// mkdirat – create a directory at `path` relative to the provided directory fd.
///
/// `mode` is accepted but not enforced.
/// Returns 0 on success, −errno on failure.
pub fn sys_mkdirat(dirfd: isize, path: *const u8, _mode: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_mkdirat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        if path.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
        mkdir_at(cwd.as_str(), path.as_str())?;
        Ok(0)
    })
}

/// chdir – change the current working directory.
///
/// Resolves `path` relative to the current CWD, verifies that the result
/// exists and is a directory, then updates the process CWD.
/// Returns 0 on success, −errno on failure.
pub fn sys_chdir(path: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_chdir",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    let process = current_process();
    syscall_body!({
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        let cwd = process.inner_exclusive_access().cwd.clone();
        let new_abs = canonicalize(cwd.as_str(), path.as_str());
        let inode = lookup_inode_follow("/", new_abs.as_str(), true)?;
        if !inode.is_dir() {
            return Err(ERRNO::ENOTDIR);
        }
        process.inner_exclusive_access().cwd = new_abs;
        Ok(0)
    })
}

/// getdents64 – read directory entries from an open directory file descriptor.
///
/// The caller provides a `buf` of `count` bytes.  The kernel writes as many
/// `linux_dirent64` records as fit, advancing the fd's internal entry-index.
/// Returns the number of bytes written, 0 when the directory is exhausted,
/// or −1 on error.
///
/// Each `linux_dirent64` record:
/// ```text
///   +0   d_ino    u64  (stable inode number, non-zero for valid entries)
///   +8   d_off    i64  (directory position of the *next* record)
///   +16  d_reclen u16  (total record length, multiple of 8)
///   +18  d_type   u8   (DT_DIR = 4, DT_REG = 8, DT_UNKNOWN = 0)
///   +19  d_name[] null-terminated name, zero-padded to meet alignment
/// ```
pub fn sys_getdents64(fd: u32, buf: *mut u8, count: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_getdents64",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_file_description(fd)?;
        // Fill a kernel-side temporary buffer …
        let mut tmp: Vec<u8> = Vec::with_capacity(count);
        tmp.extend(core::iter::repeat(0u8).take(count));
        let bytes = desc.getdents64(&mut tmp);
        if bytes == 0 {
            return Ok(0);
        }
        // … then copy to user space.
        write_bytes_to_user(buf, &tmp[..bytes])?;
        Ok(bytes as isize)
    })
}

pub fn sys_utimensat(dirfd: isize, path: *const u8, times: *const Timespec, flags: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_utimensat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );

    let token = current_user_token();
    syscall_body!({
        let supported_flags = (AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) as i32;
        if flags & !supported_flags != 0 {
            return Err(ERRNO::EINVAL);
        }
        let path = if path.is_null() && (flags & AT_EMPTY_PATH as i32 != 0) { 
            String::new()
        } else {
            translated_str(token, path).or_errno(ERRNO::EFAULT)?
        };
        debug!("sys_utimensat: dirfd = {}, path = {}, flags = {}", dirfd, path, flags);
        let target = resolve_at_target(dirfd, path.as_str(), flags)?;
        let inode = match target {
            ResolvedAtTarget::Inode(i) => i,
            ResolvedAtTarget::FileDesc(_) => return Err(ERRNO::EBADF),
        };

        let now_ns = get_realtime_ns();
        let now = InodeTime::new(now_ns / 1_000_000_000, (now_ns % 1_000_000_000) as u32);

        let (atime_req, mtime_req) = if times.is_null() {
            (UtimeArg::Now, UtimeArg::Now)
        } else {
            let pair = copy_user_timespec_pair(token, times)?;
            (parse_utime_arg(pair[0])?, parse_utime_arg(pair[1])?)
        };

        let atime = match atime_req {
            UtimeArg::Now => Some(now),
            UtimeArg::Omit => None,
            UtimeArg::Set(ts) => Some(ts),
        };
        let mtime = match mtime_req {
            UtimeArg::Now => Some(now),
            UtimeArg::Omit => None,
            UtimeArg::Set(ts) => Some(ts),
        };

        if atime.is_none() && mtime.is_none() {
            return Ok(0);
        }

        inode.set_times(atime, mtime, Some(now))?;
        Ok(0)
    })
}

/// fchown(fd, user, group) — change ownership of the file referred to by `fd`.
pub fn sys_fchown(fd: u32, user: u32, group: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_fchown",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let target = resolve_at_target(fd as isize, "", AT_EMPTY_PATH as i32)?;
        let inode = match target {
            ResolvedAtTarget::Inode(i) => i,
            ResolvedAtTarget::FileDesc(_) => return Err(ERRNO::EBADF),
        };

        if user == UID_GID_NO_CHANGE && group == UID_GID_NO_CHANGE {
            return Ok(0);
        }

        let (uid, gid) = resolve_chown_ids(&inode, user, group)?;
        inode.set_owner(uid, gid)?;
        Ok(0)
    })
}

/// fchownat(dirfd, pathname, user, group, flags) — change ownership of a path-relative target.
pub fn sys_fchownat(dirfd: isize, pathname: *const u8, user: u32, group: u32, flags: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_fchownat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let supported_flags = (AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) as i32;
        if flags & !supported_flags != 0 {
            return Err(ERRNO::EINVAL);
        }

        let path = if pathname.is_null() && (flags & AT_EMPTY_PATH as i32 != 0) {
            String::new()
        } else {
            translated_str(token, pathname).or_errno(ERRNO::EFAULT)?
        };

        let target = resolve_at_target(dirfd, path.as_str(), flags)?;
        let inode = match target {
            ResolvedAtTarget::Inode(i) => i,
            ResolvedAtTarget::FileDesc(_) => return Err(ERRNO::EBADF),
        };

        if user == UID_GID_NO_CHANGE && group == UID_GID_NO_CHANGE {
            return Ok(0);
        }

        let (uid, gid) = resolve_chown_ids(&inode, user, group)?;
        inode.set_owner(uid, gid)?;
        Ok(0)
    })
}

/// fchmod(fd, mode) — change permissions of the file referred to by `fd`.
pub fn sys_fchmod(fd: u32, mode: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_fchmod",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let target = resolve_at_target(fd as isize, "", AT_EMPTY_PATH as i32)?;
        let inode = match target {
            ResolvedAtTarget::Inode(i) => i,
            ResolvedAtTarget::FileDesc(_) => return Err(ERRNO::EBADF),
        };

        let old_mode = inode_stat(&inode).mode.bits();
        let new_mode = (old_mode & StatMode::TYPE_MASK.bits()) | (mode & StatMode::PERM_MASK.bits());
        inode.set_mode(new_mode)?;
        Ok(0)
    })
}

/// fchmodat(dirfd, pathname, mode, flags) — change permissions of a path-relative target.
pub fn sys_fchmodat(dirfd: isize, pathname: *const u8, mode: u32, flags: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_fchmodat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let supported_flags = (AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) as i32;
        if flags & !supported_flags != 0 {
            return Err(ERRNO::EINVAL);
        }

        let path = if pathname.is_null() && (flags & AT_EMPTY_PATH as i32 != 0) {
            String::new()
        } else {
            translated_str(token, pathname).or_errno(ERRNO::EFAULT)?
        };

        let target = resolve_at_target(dirfd, path.as_str(), flags)?;
        let inode = match target {
            ResolvedAtTarget::Inode(i) => i,
            ResolvedAtTarget::FileDesc(_) => return Err(ERRNO::EBADF),
        };

        let old_mode = inode_stat(&inode).mode.bits();
        let new_mode = (old_mode & StatMode::TYPE_MASK.bits()) | (mode & StatMode::PERM_MASK.bits());
        inode.set_mode(new_mode)?;
        Ok(0)
    })
}


pub fn sys_mount(
    dev_name: *const u8,
    dir_name: *const u8,
    fs_type: *const u8,
    _flags: usize,
    data: *const u8,
) -> isize {
    trace!(
        "kernel:pid[{}] sys_mount",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let dev_name = translated_str(token, dev_name).or_errno(ERRNO::EFAULT)?;
        let dir_name = translated_str(token, dir_name).or_errno(ERRNO::EFAULT)?;
        let fs_type  = translated_str(token, fs_type).or_errno(ERRNO::EFAULT)?;
        // `data` is typically NULL (e.g. mount(…, NULL)); skip translation if so.
        let _data: String = if data.is_null() {
            String::new()
        } else {
            translated_str(token, data).or_errno(ERRNO::EFAULT)?
        };

        let cwd     = current_process().inner_exclusive_access().cwd.clone();
        let abs_mnt = canonicalize(&cwd, &dir_name);
        debug!(
            "sys_mount: dev_name = {}, dir_name = {}, abs_mnt = {}, fs_type = {}, data = {}",
            dev_name,
            dir_name,
            abs_mnt,
            fs_type,
            _data
        );
        mount_device(&dev_name, &abs_mnt, &fs_type)?;
        Ok(0)
    })
}

pub fn sys_umount(name: *const u8, _flags: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_umount",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let name = translated_str(token, name).or_errno(ERRNO::EFAULT)?;
        let cwd  = current_process().inner_exclusive_access().cwd.clone();
        let abs  = canonicalize(&cwd, &name);
        do_umount(&abs)?;
        Ok(0)
    })
}

/// `ppoll_time32(2)`：在 fd 集上等待事件，支持 32 位 timespec 与临时信号掩码。
/// sigmask 目前转为*mut u32，因为当前的信号实现中掩码就是一个 u32 位域；未来若扩展为更复杂结构体再调整类型。
pub fn sys_ppoll_time32(
    ufds: *mut PollFd,
    nfds: u32,  // length of ufds
    tmo_p: *const OldTimespec32,    // timeout, NULL for infinite
    sigmask: *const u8,
    sigsetsize: usize,  // length of sigmasks
) -> isize {
    trace!(
        "kernel:pid[{}] sys_ppoll_time32",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let mut pollfds = copy_user_pollfds(token, ufds, nfds as usize)?;
        let timeout_ms = parse_timeout_ms(token, tmo_p)?;
        let deadline = timeout_ms_to_deadline(timeout_ms)?;
        let pid = current_task().unwrap().process.upgrade().unwrap().getpid();

        let old_mask = apply_temp_signal_mask(token, sigmask, sigsetsize, "sys_ppoll_time32")?;
        let ret = poll_wait_loop_with_writeback(pid, &mut pollfds, deadline, |polled| {
            write_back_pollfds(token, ufds, polled)
        });
        restore_temp_signal_mask(old_mask);
        ret
    })
}

/// `pselect6_time32(2)`：在 `fd_set` 上等待事件，支持 32 位 timespec 与临时信号掩码。
///
/// 第 6 个参数遵循 Linux `pselect6` 约定：
/// `struct { const sigset_t *ss; size_t ss_len; }`。
pub fn sys_pselect6_time32(
    nfds: i32,
    readfds: *mut usize,
    writefds: *mut usize,
    exceptfds: *mut usize,
    tmo_p: *const OldTimespec32,
    sigmask: *const u8,
) -> isize {
    trace!(
        "kernel:pid[{}] sys_pselect6_time32",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        if nfds < 0 {
            return Err(ERRNO::EINVAL);
        }
        let nfds = nfds as usize;
        if nfds > MAX_POLL_NFDS {
            return Err(ERRNO::EINVAL);
        }

        let read_set = copy_user_fdset_words(token, readfds as *const usize, nfds)?;
        let write_set = copy_user_fdset_words(token, writefds as *const usize, nfds)?;
        let except_set = copy_user_fdset_words(token, exceptfds as *const usize, nfds)?;

        
        validate_pselect_fds(
            nfds,
            read_set.as_deref(),
            write_set.as_deref(),
            except_set.as_deref(),
        )?;
        
        let (mut pollfds, metas) = build_pselect_pollfds(
            nfds,
            read_set.as_deref(),
            write_set.as_deref(),
            except_set.as_deref(),
        )?;
        
        let timeout_ms = parse_timeout_ms(token, tmo_p)?;
        // debug!(
        //     "sys_pselect6_time32: nfds={}, read_set={:?}, write_set={:?}, except_set={:?}, timeout_ms={:?}, sigmask={:p}",
        //     nfds, read_set, write_set, except_set, timeout_ms, sigmask
        // );
        let deadline = timeout_ms_to_deadline(timeout_ms)?;
        let pid = current_task().unwrap().process.upgrade().unwrap().getpid();
        let (sigmask_ptr, sigsetsize) = parse_pselect_sigmask_arg(token, sigmask)?;
        let old_mask = apply_temp_signal_mask(token, sigmask_ptr, sigsetsize, "sys_pselect6_time32")?;

        let ret = poll_wait_loop_with_writeback(pid, &mut pollfds, deadline, |polled| {
            write_back_pselect_fdsets(
                readfds,
                writefds,
                exceptfds,
                nfds,
                polled,
                &metas,
            )
        });
        restore_temp_signal_mask(old_mask);
        ret
    })
}

pub fn sys_renameat2(
    old_dirfd: isize,
    old_name: *const u8,
    new_dirfd: isize,
    new_name: *const u8,
    flags: u32,
) -> isize {
    trace!(
        "kernel:pid[{}] sys_renameat2(flags={})",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        flags
    );
    if flags != 0 {
        return -(ERRNO::EINVAL as isize);
    }
    let token = current_user_token();
    syscall_body!({
        let old_name = translated_str(token, old_name).or_errno(ERRNO::EFAULT)?;
        let new_name = translated_str(token, new_name).or_errno(ERRNO::EFAULT)?;
        if old_name.is_empty() || new_name.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        let old_cwd = resolve_dirfd_base(old_dirfd, old_name.as_str())?;
        let new_cwd = resolve_dirfd_base(new_dirfd, new_name.as_str())?;
        rename_at(old_cwd.as_str(), &old_name, new_cwd.as_str(), &new_name)?;
        Ok(0)
    })
}
