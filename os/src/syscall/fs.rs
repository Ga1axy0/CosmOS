use crate::fs::{
    AT_EMPTY_PATH, AT_FDCWD, AT_REMOVEDIR, AT_SYMLINK_NOFOLLOW, OpenFlags, Stat,
    canonicalize, do_umount, inode_stat, linkat, lookup_inode, make_pipe, mkdir_at,
    mount_device, open_file_at, unlinkat,
};
use crate::mm::{translated_byte_buffer, translated_refmut, translated_str, UserBuffer};
use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall::{write_bytes_to_user, write_pod_to_user};
use crate::syscall_body;
use crate::task::{current_process, current_task, current_user_token};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;

/// `writev` 使用的用户态向量缓冲区描述符。
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(super) struct IoVec {
    /// 用户缓冲区起始地址。
    iov_base: usize,
    /// 用户缓冲区长度。
    iov_len: usize,
}

/// 校验 fd 并返回可写文件对象。
fn get_writable_file(fd: usize) -> Result<Arc<dyn crate::fs::File>, ERRNO> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return Err(ERRNO::EBADF);
    }
    let file = inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.clone();
    if !file.writable() {
        return Err(ERRNO::EACCES);
    }
    drop(inner);
    Ok(file)
}

/// 从用户态复制 `iovec` 数组，避免数组跨页时直接解引用失败。
fn copy_user_iovecs(token: usize, iov: *const IoVec, iovcnt: i32) -> Result<Vec<IoVec>, ERRNO> {
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
    let file = inner
        .fd_table
        .get(dirfd as usize)
        .and_then(|file| file.as_ref())
        .ok_or(ERRNO::EBADF)?
        .clone();
    drop(inner);
    if !file.is_dir() {
        return Err(ERRNO::ENOTDIR);
    }
    file.path().ok_or(ERRNO::ENOTDIR)
}


/// write syscall
pub fn sys_write(fd: u32, buf: *const u8, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_write",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let fd = fd as usize;
        let file = get_writable_file(fd)?;
        Ok(file.write(UserBuffer::new(
            translated_byte_buffer(token, buf, len).or_errno(ERRNO::EFAULT)?,
        )) as isize)
    })
}

/// writev syscall：按 `iovec` 顺序将多个用户缓冲区写入同一个 fd。
pub fn sys_writev(fd: u32, iov: *const IoVec, iovcnt: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_writev",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let fd = fd as usize;
        let file = get_writable_file(fd)?;
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
                translated_byte_buffer(token, iovec.iov_base as *const u8, iovec.iov_len)
                    .or_errno(ERRNO::EFAULT)?,
            );
            let written = file.write(user_buf);
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
        "kernel:pid[{}] sys_read",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let token = current_user_token();
    syscall_body!({
        let fd = fd as usize;
        let inner = process.inner_exclusive_access();
        if fd >= inner.fd_table.len() {
            return Err(ERRNO::EBADF);
        }
        let file = inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.clone();
        if !file.readable() {
            return Err(ERRNO::EACCES);
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        trace!("kernel: sys_read .. file.read");
        Ok(file.read(UserBuffer::new(
            translated_byte_buffer(token, buf, len).or_errno(ERRNO::EFAULT)?,
        )) as isize)
    })
}

/// ioctl 系统调用：校验 fd 后转发到具体文件对象。
pub fn sys_ioctl(fd: u32, req: usize, arg: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_ioctl",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    syscall_body!({
        let fd = fd as usize;
        let inner = process.inner_exclusive_access();
        let file = inner
            .fd_table
            .get(fd)
            .and_then(|f| f.as_ref())
            .ok_or(ERRNO::EBADF)?
            .clone();
        drop(inner);
        // 具体 request 语义由底层文件对象决定；当前大多数对象会返回 ENOTTY。
        // TODO: tty 实现 `TCGETS/TIOCGWINSZ` 后，这里会开始承载真实终端控制语义。
        debug!("sys_ioctl: fd = {}, req = {:#x}, arg = {:#x}", fd, req, arg);
        file.ioctl(req, arg)
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
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        if path.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
        debug!(
            "sys_open: dirfd = {}, path = {}, flags = {}, cwd = {}",
            dirfd, path, flags, cwd
        );
        let inode = open_file_at(
            cwd.as_str(),
            path.as_str(),
            OpenFlags::from_bits(flags).or_errno(ERRNO::EINVAL)?,
        )
        .or_errno(ERRNO::ENOENT)?;
        let mut inner = process.inner_exclusive_access();
        let fd = inner.alloc_fd();
        inner.fd_table[fd] = Some(inode);
        Ok(fd as isize)
    })
}

/// close syscall
pub fn sys_close(fd: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_close",
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
        inner.fd_table[fd].take();
        Ok(0)
    })
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
        let (pipe_read, pipe_write) = make_pipe();
        let read_fd = inner.alloc_fd();
        inner.fd_table[read_fd] = Some(pipe_read);
        let write_fd = inner.alloc_fd();
        inner.fd_table[write_fd] = Some(pipe_write);
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
        let new_fd = inner.alloc_fd();
        inner.fd_table[new_fd] = Some(Arc::clone(inner.fd_table[fd].as_ref().unwrap()));
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
    let mut inner = process.inner_exclusive_access();
    syscall_body!({
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
        if newfd >= inner.fd_table.len() {
            inner.fd_table.resize(newfd + 1, None);
        }
        // If newfd is already open, close it first.
        inner.fd_table[newfd].take();
        inner.fd_table[newfd] = Some(Arc::clone(inner.fd_table[oldfd].as_ref().unwrap()));
        Ok(newfd as isize)
    })
}

/// fstat syscall
pub fn sys_fstat(fd: u32, st: *mut Stat) -> isize {
    trace!(
        "kernel:pid[{}] sys_fstat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    syscall_body!({
        let fd = fd as usize;
        let inner = process.inner_exclusive_access();
        let file = inner
            .fd_table
            .get(fd)
            .and_then(|f| f.as_ref())
            .ok_or(ERRNO::EBADF)?
            .clone();
        drop(inner);
        let stat = file.stat();
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
        if flags & AT_SYMLINK_NOFOLLOW != 0 {
            // TODO: 当前 VFS 尚未实现 symlink，暂时按普通路径 stat 处理。
            warn!(
                "sys_newfstatat: AT_SYMLINK_NOFOLLOW is not implemented, fallback to stat target path"
            );
        }
        if path.is_empty() {
            if flags & AT_EMPTY_PATH == 0 {
                return Err(ERRNO::ENOENT);
            }
            if dirfd == AT_FDCWD {
                let cwd = current_process().inner_exclusive_access().cwd.clone();
                let inode = lookup_inode(cwd.as_str()).ok_or(ERRNO::ENOENT)?;
                let stat = inode_stat(&inode);
                write_pod_to_user(st, &stat)?;
                return Ok(0);
            }
            if dirfd < 0 {
                return Err(ERRNO::EBADF);
            }
            let process = current_process();
            let inner = process.inner_exclusive_access();
            let file = inner
                .fd_table
                .get(dirfd as usize)
                .and_then(|file| file.as_ref())
                .ok_or(ERRNO::EBADF)?
                .clone();
            drop(inner);
            let stat = file.stat();
            write_pod_to_user(st, &stat)?;
            return Ok(0);
        }
        let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
        let abs_path = canonicalize(cwd.as_str(), path.as_str());
        let inode = lookup_inode(abs_path.as_str()).ok_or(ERRNO::ENOENT)?;
        let stat = inode_stat(&inode);
        write_pod_to_user(st, &stat)?;
        Ok(0)
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
        if flags != 0 {
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
        linkat(old_cwd.as_str(), &old_path, new_cwd.as_str(), &new_path)?;
        Ok(0)
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
        let inode = lookup_inode(new_abs.as_str()).or_errno(ERRNO::ENOENT)?;
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
///   +0   d_ino    u64  (entry index used as synthetic inode number)
///   +8   d_off    i64  (entry index of the *next* record)
///   +16  d_reclen u16  (total record length, multiple of 8)
///   +18  d_type   u8   (DT_DIR = 4, DT_REG = 8, DT_UNKNOWN = 0)
///   +19  d_name[] null-terminated name, zero-padded to meet alignment
/// ```
pub fn sys_getdents64(fd: u32, buf: *mut u8, count: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_getdents64",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    syscall_body!({
        let fd = fd as usize;
        let inner = process.inner_exclusive_access();
        let file = inner
            .fd_table
            .get(fd)
            .and_then(|f| f.as_ref())
            .ok_or(ERRNO::EBADF)?
            .clone();
        drop(inner);
        // Fill a kernel-side temporary buffer …
        let mut tmp: Vec<u8> = Vec::new();
        tmp.resize(count, 0u8);
        let bytes = file.getdents64(&mut tmp);
        if bytes == 0 {
            return Ok(0);
        }
        // … then copy to user space.
        write_bytes_to_user(buf, &tmp[..bytes])?;
        Ok(bytes as isize)
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
