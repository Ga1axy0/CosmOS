use crate::fs::{
    canonicalize, linkat, lookup_inode, make_pipe, mkdir_at, open_file_at, unlinkat,
    OpenFlags, Stat,
};
use crate::mm::{translated_byte_buffer, translated_refmut, translated_str, UserBuffer};
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
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if !file.writable() {
            return -1;
        }
        let file = file.clone();
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        file.write(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}
/// read syscall
pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_read",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if !file.readable() {
            return -1;
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        trace!("kernel: sys_read .. file.read");
        file.read(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}
/// open sys
pub fn sys_open(path: *const u8, flags: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_open",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let token = current_user_token();
    let path = translated_str(token, path);
    let cwd = process.inner_exclusive_access().cwd.clone();
    debug!("sys_open: path = {}, flags = {}, cwd = {}", path, flags, cwd);
    if let Some(inode) = open_file_at(cwd.as_str(), path.as_str(), OpenFlags::from_bits(flags).unwrap()) {
        let mut inner = process.inner_exclusive_access();
        let fd = inner.alloc_fd();
        inner.fd_table[fd] = Some(inode);
        fd as isize
    } else {
        -1
    }
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
    let mut inner = process.inner_exclusive_access();
    let (pipe_read, pipe_write) = make_pipe();
    let read_fd = inner.alloc_fd();
    inner.fd_table[read_fd] = Some(pipe_read);
    let write_fd = inner.alloc_fd();
    inner.fd_table[write_fd] = Some(pipe_write);
    *translated_refmut(token, pipe) = read_fd;
    *translated_refmut(token, unsafe { pipe.add(1) }) = write_fd;
    0
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
        return -1;
    }
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    let new_fd = inner.alloc_fd();
    inner.fd_table[new_fd] = Some(Arc::clone(inner.fd_table[fd].as_ref().unwrap()));
    new_fd as isize
}

/// YOUR JOB: Implement fstat.
pub fn sys_fstat(_fd: usize, _st: *mut Stat) -> isize {
    trace!(
        "kernel:pid[{}] sys_fstat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if _fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[_fd] {
        let file = file.clone();
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        let stat = file.stat();
        let stat_bytes = unsafe {
            core::slice::from_raw_parts(
                &stat as *const Stat as *const u8,
                core::mem::size_of::<Stat>(),
            )
        };
        let mut buffers = translated_byte_buffer(token, _st as *const u8, core::mem::size_of::<Stat>());
        let mut copied = 0usize;
        for buffer in buffers.iter_mut() {
            let len = buffer.len();
            buffer.copy_from_slice(&stat_bytes[copied..copied + len]);
            copied += len;
        }
        0
    } else {
        -1
    }

}

/// YOUR JOB: Implement linkat.
pub fn sys_linkat(_old_name: *const u8, _new_name: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_linkat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    let old_path = translated_str(token, _old_name);
    let new_path = translated_str(token, _new_name);
    if old_path.is_empty() || new_path.is_empty() {
        return -1;
    }
    if old_path.as_str() == new_path.as_str() {
        return -1;
    }
    let cwd = current_process().inner_exclusive_access().cwd.clone();
    if let Ok(()) = linkat(cwd.as_str(), old_path.as_str(), new_path.as_str()) {
        0
    } else {
        -1
    }
}

/// YOUR JOB: Implement unlinkat.
pub fn sys_unlinkat(_name: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_unlinkat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    let name = translated_str(token, _name);
    if name.is_empty() {
        return -1;
    }
    let cwd = current_process().inner_exclusive_access().cwd.clone();
    if unlinkat(cwd.as_str(), name.as_str()).is_ok() {
        0
    } else {
        -1
    }
}


/// getcwd – copy the current working directory into a user-space buffer.
///
/// Returns the buffer address as `isize` on success, 0 if `size` is too small.
pub fn sys_getcwd(buf: *mut u8, size: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_getcwd",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    let process = current_process();
    let cwd = process.inner_exclusive_access().cwd.clone();
    let cwd_bytes = cwd.as_bytes();
    let need = cwd_bytes.len() + 1; // include null terminator
    if size < need {
        return 0; // ERANGE
    }
    // Write the CWD bytes directly into user memory.
    for (i, &b) in cwd_bytes.iter().enumerate() {
        *translated_refmut(token, unsafe { (buf as usize + i) as *mut u8 }) = b;
    }
    *translated_refmut(token, unsafe { (buf as usize + cwd_bytes.len()) as *mut u8 }) = 0;
    buf as isize
}

/// mkdirat – create a directory at `path` relative to the current working directory.
///
/// `_dirfd` (args[0]) is accepted for ABI compatibility but ignored—the kernel-level
/// CWD is used instead.  `mode` (args[2]) is accepted but not enforced.
/// Returns 0 on success, −1 on failure.
pub fn sys_mkdirat(path: *const u8, _mode: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_mkdirat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    let process = current_process();
    let path = translated_str(token, path);
    let cwd = process.inner_exclusive_access().cwd.clone();
    if mkdir_at(cwd.as_str(), path.as_str()) {
        0
    } else {
        -1
    }
}

/// chdir – change the current working directory.
///
/// Resolves `path` relative to the current CWD, verifies that the result
/// exists and is a directory, then updates the process CWD.
/// Returns 0 on success, −1 on failure.
pub fn sys_chdir(path: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_chdir",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    let process = current_process();
    let path = translated_str(token, path);
    let cwd = process.inner_exclusive_access().cwd.clone();
    let new_abs = canonicalize(cwd.as_str(), path.as_str());
    // Verify the target exists and is a directory.
    match lookup_inode(new_abs.as_str()) {
        Some(inode) if inode.is_dir() => {
            process.inner_exclusive_access().cwd = new_abs;
            0
        }
        _ => -1,
    }
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
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        drop(inner);
        // Fill a kernel-side temporary buffer …
        let mut tmp: Vec<u8> = Vec::new();
        tmp.resize(count, 0u8);
        let bytes = file.getdents64(&mut tmp);
        if bytes == 0 {
            return 0;
        }
        // … then copy to user space page by page.
        let user_bufs = translated_byte_buffer(token, buf as *const u8, bytes);
        let mut src_off = 0usize;
        for slice in user_bufs {
            let len = slice.len();
            slice.copy_from_slice(&tmp[src_off..src_off + len]);
            src_off += len;
        }
        bytes as isize
    } else {
        -1
    }
}
