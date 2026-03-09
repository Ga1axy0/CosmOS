use crate::fs::{
    canonicalize, linkat, lookup_inode, make_pipe, mkdir_at, open_file_at, unlinkat, OpenFlags,
    Stat,
};
use crate::mm::{translated_byte_buffer, translated_refmut, translated_str, UserBuffer};
use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall_body;
use crate::task::{current_process, current_task, current_user_token};
use alloc::sync::Arc;
use alloc::vec::Vec;


/// write syscall
pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_write",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    let process = current_process();
    syscall_body!({
        let inner = process.inner_exclusive_access();
        if fd >= inner.fd_table.len() {
            return Err(ERRNO::EBADF);
        }
        let file = inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.clone();
        if !file.writable() {
            return Err(ERRNO::EACCES);
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        Ok(file.write(UserBuffer::new(
            translated_byte_buffer(token, buf, len).or_errno(ERRNO::EFAULT)?,
        )) as isize)
    })
}

/// read syscall
pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_read",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let token = current_user_token();
    syscall_body!({
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

/// open sysall
pub fn sys_open(path: *const u8, flags: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_open",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let token = current_user_token();
    syscall_body!({
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        let cwd = process.inner_exclusive_access().cwd.clone();
        debug!(
            "sys_open: path = {}, flags = {}, cwd = {}",
            path, flags, cwd
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
pub fn sys_close(fd: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_close",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    inner.fd_table[fd].take();
    0
}

/// pipe syscall
pub fn sys_pipe(pipe: *mut usize) -> isize {
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
        *translated_refmut(token, pipe).or_errno(ERRNO::EFAULT)? = read_fd;
        *translated_refmut(token, unsafe { pipe.add(1) }).or_errno(ERRNO::EFAULT)? = write_fd;
        Ok(0)
    })
}

/// dup syscall
pub fn sys_dup(fd: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_dup",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -(ERRNO::EBADF as isize);
    }
    if inner.fd_table[fd].is_none() {
        return -(ERRNO::EBADF as isize);
    }
    let new_fd = inner.alloc_fd();
    inner.fd_table[new_fd] = Some(Arc::clone(inner.fd_table[fd].as_ref().unwrap()));
    new_fd as isize
}

/// fstat syscall
pub fn sys_fstat(fd: usize, st: *mut Stat) -> isize {
    trace!(
        "kernel:pid[{}] sys_fstat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    let process = current_process();
    syscall_body!({
        let inner = process.inner_exclusive_access();
        let file = inner
            .fd_table
            .get(fd)
            .and_then(|f| f.as_ref())
            .ok_or(ERRNO::EBADF)?
            .clone();
        drop(inner);
        let stat = file.stat();
        let stat_bytes = unsafe {
            core::slice::from_raw_parts(
                &stat as *const Stat as *const u8,
                core::mem::size_of::<Stat>(),
            )
        };
        let mut buffers =
            translated_byte_buffer(token, st as *const u8, core::mem::size_of::<Stat>())
                .or_errno(ERRNO::EFAULT)?;
        let mut copied = 0usize;
        for buffer in buffers.iter_mut() {
            let len = buffer.len();
            buffer.copy_from_slice(&stat_bytes[copied..copied + len]);
            copied += len;
        }
        Ok(0)
    })
}

/// linkat syscall
pub fn sys_linkat(old_name: *const u8, new_name: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_linkat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let old_path = translated_str(token, old_name).or_errno(ERRNO::EFAULT)?;
        let new_path = translated_str(token, new_name).or_errno(ERRNO::EFAULT)?;
        if old_path.is_empty() || new_path.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        if old_path == new_path {
            return Err(ERRNO::EINVAL);
        }
        let cwd = current_process().inner_exclusive_access().cwd.clone();
        linkat(cwd.as_str(), &old_path, &new_path)?;
        Ok(0)
    })
}

/// unlinkat syscall
pub fn sys_unlinkat(name: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_unlinkat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let name = translated_str(token, name).or_errno(ERRNO::EFAULT)?;
        if name.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        let cwd = current_process().inner_exclusive_access().cwd.clone();
        unlinkat(cwd.as_str(), &name)?;
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
        let token = current_user_token();
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
        let mut off = 0usize;
        for slice in translated_byte_buffer(token, buf as *const u8, total)
            .or_errno(ERRNO::EFAULT)?
            .iter_mut()
        {
            let len = slice.len();
            slice.copy_from_slice(&src[off..off + len]);
            off += len;
        }
        Ok(buf as isize)
    })
}

/// mkdirat – create a directory at `path` relative to the current working directory.
///
/// `mode` is accepted but not enforced.
/// Returns 0 on success, −errno on failure.
pub fn sys_mkdirat(path: *const u8, _mode: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_mkdirat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    let process = current_process();
    syscall_body!({
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        let cwd = process.inner_exclusive_access().cwd.clone();
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
pub fn sys_getdents64(fd: usize, buf: *mut u8, count: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_getdents64",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    let process = current_process();
    syscall_body!({
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
        // … then copy to user space page by page.
        let mut user_bufs = translated_byte_buffer(token, buf as *const u8, bytes)
            .or_errno(ERRNO::EFAULT)?;
        let mut src_off = 0usize;
        for slice in user_bufs.iter_mut() {
            let len = slice.len();
            slice.copy_from_slice(&tmp[src_off..src_off + len]);
            src_off += len;
        }
        Ok(bytes as isize)
    })
}
