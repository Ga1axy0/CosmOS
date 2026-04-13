use crate::{
    syscall::errno::ERRNO,
    syscall_body,
    config::{PAGE_SIZE, PAGE_SIZE_BITS, TRAP_CONTEXT_BASE},
    mm::{
        MapPermission,
        VirtAddr,
    },
    task::{
        current_process, current_task, current_user_token,
        mmap_current_process, munmap_current_process, mprotect_current_process,
    },
};

bitflags! {
    /// mmap syscall flags (the `flags` argument of `sys_mmap`).
    pub struct MMapFlags: usize {
        const MAP_SHARED = 0x1;
        const MAP_PRIVATE = 0x2;
        const MAP_ANONYMOUS = 0x20;
    }
    pub struct MMapProt: usize {
        const PROT_READ = 0x1;
        const PROT_WRITE = 0x2;
        const PROT_EXEC = 0x4;
        const PROT_GROWSDOWN = 0x01000000;
        const PROT_GROWSUP = 0x02000000;
    }
}



/// mmap syscall
pub fn sys_mmap(addr: usize, len: usize, prot: usize, flags: usize, fd: usize, offset: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_mmap",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let pid = current_task().unwrap().process.upgrade().unwrap().getpid();
        debug!(
            "[mmap] request: pid={} addr={:#x} len={} prot={:#x} flags={:#x} fd={} offset={:#x}",
            pid,
            addr,
            len,
            prot,
            flags,
            fd,
            offset
        );
        if addr & ((1 << PAGE_SIZE_BITS) - 1) != 0 {
            return Err(ERRNO::EINVAL); // addr not page-aligned
        }
        // PROT_* currently supports only R/W/X bits.
        if prot & !(MMapProt::PROT_READ.bits() | MMapProt::PROT_WRITE.bits() | MMapProt::PROT_EXEC.bits()) != 0 {
            return Err(ERRNO::EINVAL); // unknown permission bits
        }
        if prot & 0x7 == 0 {
            return Err(ERRNO::EINVAL); // no access at all is meaningless
        }
        if len == 0 {
            return Err(ERRNO::EINVAL);
        }
        let end = addr.checked_add(len).ok_or(ERRNO::EOVERFLOW)?;
        let native_compat = flags == 0 && fd == 0 && offset == 0;
        if !native_compat {
            if offset & ((1 << PAGE_SIZE_BITS) - 1) != 0 {
                return Err(ERRNO::EINVAL); // file offset must be page-aligned
            }
            let mmap_flags = MMapFlags::from_bits_truncate(flags);
            let shared = mmap_flags.contains(MMapFlags::MAP_SHARED);
            let private = mmap_flags.contains(MMapFlags::MAP_PRIVATE);
            if shared == private {
                // both set or both unset
                return Err(ERRNO::EINVAL);
            }
        }

        let mut perm = MapPermission::U;
        if prot & 0x1 != 0 {
            perm |= MapPermission::R;
        }
        if prot & 0x2 != 0 {
            perm |= MapPermission::W;
        }
        if prot & 0x4 != 0 {
            perm |= MapPermission::X;
        }


        //if user did not specify addr.
        let len_aligned = (len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let process = current_process();
        let native_compat = flags == 0 && fd == 0 && offset == 0;
        let mut file_desc = None;
        let mut is_shared = false;
        let is_anon;
        if !native_compat {
            let mmap_flags = MMapFlags::from_bits_truncate(flags);
            is_anon = mmap_flags.contains(MMapFlags::MAP_ANONYMOUS);
            is_shared = mmap_flags.contains(MMapFlags::MAP_SHARED);
            if !is_anon {
                let file = {
                    let inner = process.inner_exclusive_access();
                    inner
                        .fd_table
                        .get(fd)
                        .and_then(|entry| entry.as_ref())
                        .map(|entry| entry.desc.clone())
                        .ok_or(ERRNO::EBADF)?
                };
                if file.is_dir() {
                    return Err(ERRNO::EACCES);
                }
                if !file.readable() {
                    return Err(ERRNO::EACCES);
                }
                if is_shared && (prot & MMapProt::PROT_WRITE.bits()) != 0 && !file.writable() {
                    return Err(ERRNO::EACCES);
                }
                if file.backing_inode().is_none() {
                    // TODO：后续若要支持设备文件专用 mmap，这里需要分派到对应驱动。
                    return Err(ERRNO::ENODEV);
                }
                file_desc = Some(file);
            }
        }
        let map_addr = if addr == 0 {
            // Linux-style mmap(NULL, ...): choose a free user VA automatically.
            let step = len_aligned.max(PAGE_SIZE);
            let mut probe = process.mmap_base();
            let limit = TRAP_CONTEXT_BASE.saturating_sub(step);
            let mut chosen: Option<usize> = None;
            while probe <= limit {
                let probe_end = probe.checked_add(len).ok_or(ERRNO::EOVERFLOW)?;
                let mapped = if let Some(file) = file_desc.as_ref() {
                    process.mmap_file(
                        VirtAddr::from(probe),
                        VirtAddr::from(probe_end),
                        perm,
                        file.clone(),
                        offset / PAGE_SIZE,
                        is_shared,
                    )
                } else {
                    process.mmap(VirtAddr::from(probe), VirtAddr::from(probe_end), perm)
                };
                if mapped {
                    debug!(
                        "[mmap] auto-selected range: pid={} start={:#x} end={:#x} file_backed={} shared={} lazy={}",
                        pid,
                        probe,
                        probe_end,
                        file_desc.is_some(),
                        is_shared,
                        file_desc.is_some()
                    );
                    chosen = Some(probe);
                    break;
                }
                probe = probe.saturating_add(step);
            }
            chosen.ok_or(ERRNO::ENOMEM)?
        } else {
            let mapped = if let Some(file) = file_desc.as_ref() {
                process.mmap_file(
                    VirtAddr::from(addr),
                    VirtAddr::from(end),
                    perm,
                    file.clone(),
                    offset / PAGE_SIZE,
                    is_shared,
                )
            } else {
                process.mmap(VirtAddr::from(addr), VirtAddr::from(end), perm)
            };
            if !mapped {
                return Err(ERRNO::ENOMEM);
            }
            debug!(
                "[mmap] fixed range registered: pid={} start={:#x} end={:#x} file_backed={} shared={} lazy={}",
                pid,
                addr,
                end,
                file_desc.is_some(),
                is_shared,
                file_desc.is_some()
            );
            addr
        };

        debug!(
            "[mmap] complete: pid={} mapped_addr={:#x} len_aligned={} native_compat={}",
            pid,
            map_addr,
            len_aligned,
            native_compat
        );

        if native_compat {
            Ok(0)
        } else {
            Ok(map_addr as isize)
        }
    })
}

/// munmap syscall
pub fn sys_munmap(start: usize, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_munmap",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let pid = current_task().unwrap().process.upgrade().unwrap().getpid();
        debug!(
            "[munmap] request: pid={} start={:#x} len={}",
            pid,
            start,
            len
        );
        if start & ((1 << PAGE_SIZE_BITS) - 1) != 0 {
            return Err(ERRNO::EINVAL); // start not page-aligned
        }
        if len == 0 {
            return Err(ERRNO::EINVAL);
        }
        let end = start.checked_add(len).ok_or(ERRNO::EOVERFLOW)?;
        if munmap_current_process(VirtAddr::from(start), VirtAddr::from(end)) {
            debug!(
                "[munmap] complete: pid={} start={:#x} end={:#x}",
                pid,
                start,
                end
            );
            Ok(0)
        } else {
            // Unmapping an invalid/unmapped range is treated as ENOMEM.
            Err(ERRNO::ENOMEM)
        }
    })
}

/// mprotect syscall
pub fn sys_mprotect(start: usize, len: usize, prot: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_mprotect",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        if start & ((1 << PAGE_SIZE_BITS) - 1) != 0 {
            return Err(ERRNO::EINVAL); // start not page-aligned
        }
        if len == 0 {
            return Ok(0); // POSIX/Linux: zero-length mprotect is a successful no-op
        }
        let end = start.checked_add(len).ok_or(ERRNO::EOVERFLOW)?;
        // PROT_* currently supports only R/W/X bits.
        if prot & !(MMapProt::PROT_READ.bits() | MMapProt::PROT_WRITE.bits() | MMapProt::PROT_EXEC.bits()) != 0 {
            return Err(ERRNO::EINVAL); // unknown permission bits
        }

        // Translate user PROT_* flags into internal MapPermission.
        // If no R/W/X bits are set (e.g., PROT_NONE), treat it as a valid
        // "no access" mapping by using an empty MapPermission, matching
        // Linux semantics used for guard pages.
        let mut perm = if prot & (MMapProt::PROT_READ.bits()
            | MMapProt::PROT_WRITE.bits()
            | MMapProt::PROT_EXEC.bits())
            == 0
        {
            MapPermission::empty()
        } else {
            let mut p = MapPermission::U;
            if prot & MMapProt::PROT_READ.bits() != 0 {
                p |= MapPermission::R;
            }
            if prot & MMapProt::PROT_WRITE.bits() != 0 {
                p |= MapPermission::W;
            }
            if prot & MMapProt::PROT_EXEC.bits() != 0 {
                p |= MapPermission::X;
            }
            p
        };

        if mprotect_current_process(VirtAddr::from(start), VirtAddr::from(end), perm) {
            Ok(0)
        } else {
            Err(ERRNO::ENOMEM)
        }
    })
}
