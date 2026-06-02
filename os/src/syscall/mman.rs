use crate::{
    config::{PAGE_SIZE, PAGE_SIZE_BITS},
    mm::{MapPermission, VirtAddr},
    syscall::errno::ERRNO,
    syscall::{write_bytes_to_user, write_pod_to_user},
    syscall_body,
    task::{
        current_process, current_task, current_trap_cx, mprotect_current_process,
        msync_current_process, munmap_current_process,
    },
};

use alloc::vec::Vec;
use core::mem::size_of;

const MPOL_DEFAULT: i32 = 0;
const MPOL_F_NODE: u32 = 1 << 0;
const MPOL_F_ADDR: u32 = 1 << 1;
const MPOL_F_MEMS_ALLOWED: u32 = 1 << 2;
const GET_MEMPOLICY_SUPPORTED_FLAGS: u32 = MPOL_F_NODE | MPOL_F_ADDR | MPOL_F_MEMS_ALLOWED;
const MCL_CURRENT: i32 = 1;
const MCL_FUTURE: i32 = 2;
const MCL_ONFAULT: i32 = 4;
const MLOCKALL_SUPPORTED_FLAGS: i32 = MCL_CURRENT | MCL_FUTURE | MCL_ONFAULT;

fn write_ulong_mask_to_user(mask_ptr: *mut u8, maxnode: usize, mask: usize) -> Result<(), ERRNO> {
    if mask_ptr.is_null() || maxnode == 0 {
        return Ok(());
    }
    let byte_len = maxnode.div_ceil(8);
    let mut mask_bytes = Vec::new();
    mask_bytes.resize(byte_len, 0);
    let usable_bytes = byte_len.min(size_of::<usize>());
    for (idx, slot) in mask_bytes.iter_mut().take(usable_bytes).enumerate() {
        *slot = ((mask >> (idx * 8)) & 0xff) as u8;
    }
    write_bytes_to_user(mask_ptr, mask_bytes.as_slice())
}

bitflags! {
    /// mmap syscall flags (the `flags` argument of `sys_mmap`).
    pub struct MMapFlags: usize {
        const MAP_SHARED = 0x1;
        const MAP_PRIVATE = 0x2;
        const MAP_FIXED = 0x10;
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

const MS_ASYNC: i32 = 1;
const MS_INVALIDATE: i32 = 2;
const MS_SYNC: i32 = 4;
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


        // if user did not specify addr.
        // 对齐前先检查溢出，避免超大长度把探测步长算错。
        let len_aligned = len
            .checked_add(PAGE_SIZE - 1)
            .ok_or(ERRNO::EOVERFLOW)?
            & !(PAGE_SIZE - 1);
        let process = current_process();
        let native_compat = flags == 0 && fd == 0 && offset == 0;
        let mut file_desc = None;
        let mut is_shared = false;
        if !native_compat {
            let mmap_flags = MMapFlags::from_bits_truncate(flags);
            let is_anon = mmap_flags.contains(MMapFlags::MAP_ANONYMOUS);
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
            let (chosen, chosen_end, hint) = {
                let mut inner = process.inner_exclusive_access();
                inner.ensure_address_space_capacity(len_aligned)?;
                let hint = inner.vm_layout.mmap_hint;
                let base = inner.vm_layout.mmap_base;
                let chosen = inner
                    .memory_set
                    .find_free_mmap_area(hint, base, len_aligned)
                    .ok_or(ERRNO::ENOMEM)?;
                let chosen_end = chosen.checked_add(len_aligned).ok_or(ERRNO::EOVERFLOW)?;
                let mapped = if let Some(file) = file_desc.as_ref() {
                    inner.memory_set.mmap_file(
                        VirtAddr::from(chosen),
                        VirtAddr::from(chosen_end),
                        perm,
                        file.clone(),
                        offset / PAGE_SIZE,
                        is_shared,
                    )
                } else {
                    inner.memory_set.mmap_anonymous(
                        VirtAddr::from(chosen),
                        VirtAddr::from(chosen_end),
                        perm,
                    )
                };
                mapped.map_err(|_| ERRNO::ENOMEM)?;
                inner.vm_layout.mmap_hint = chosen_end;
                (chosen, chosen_end, hint)
            };
            debug!(
                "[mmap] auto-selected range: pid={} start={:#x} end={:#x} hint_in={:#x} hint_out={:#x} file_backed={} shared={} lazy={}",
                pid,
                chosen,
                chosen_end,
                hint,
                chosen_end,
                file_desc.is_some(),
                is_shared,
                file_desc.is_some()
            );
            chosen
        } else {
            if MMapFlags::from_bits_truncate(flags).contains(MMapFlags::MAP_FIXED) {
                // glibc's dynamic loader maps a DSO, then MAP_FIXED-remaps
                // writable subranges over the first mapping.
                let _ = process.munmap(VirtAddr::from(addr), VirtAddr::from(end));
            }
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
            mapped?;
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

/// change data segment size
pub fn sys_brk(addr: usize) -> isize {
    let pid = current_task().unwrap().process.upgrade().unwrap().getpid();
    let cx = current_trap_cx();
    debug!(
        "sys_brk: pid={} addr={:#x} tp={:#x} sp={:#x} sepc={:#x}",
        pid,
        addr,
        cx.x[4],
        cx.x[2],
        cx.sepc
    );
    current_process().set_program_brk(addr) as isize
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

/// Linux `mlock(2)` compatibility stub.
///
/// `cyclictest` and similar RT benchmarks use this to reduce paging jitter.
/// xxOS currently has no swap or pageable user memory, so treating it as a
/// validated no-op is sufficient for userland compatibility.
pub fn sys_mlock(addr: usize, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_mlock",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let _ = addr.checked_add(len).ok_or(ERRNO::EOVERFLOW)?;
        Ok(0)
    })
}

/// Linux `munlock(2)` compatibility stub.
pub fn sys_munlock(addr: usize, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_munlock",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let _ = addr.checked_add(len).ok_or(ERRNO::EOVERFLOW)?;
        Ok(0)
    })
}

/// Linux `mlockall(2)` compatibility stub.
pub fn sys_mlockall(flags: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_mlockall",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        if flags & !MLOCKALL_SUPPORTED_FLAGS != 0 {
            return Err(ERRNO::EINVAL);
        }
        Ok(0)
    })
}

/// Linux `munlockall(2)` compatibility stub.
pub fn sys_munlockall() -> isize {
    trace!(
        "kernel:pid[{}] sys_munlockall",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    0
}

/// msync syscall
pub fn sys_msync(addr: usize, len: usize, flags: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_msync",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        if addr & ((1 << PAGE_SIZE_BITS) - 1) != 0 {
            return Err(ERRNO::EINVAL);
        }
        if len == 0 {
            return Ok(0);
        }
        if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0 {
            return Err(ERRNO::EINVAL);
        }
        if (flags & MS_ASYNC != 0) && (flags & MS_SYNC != 0) {
            return Err(ERRNO::EINVAL);
        }
        let end = addr.checked_add(len).ok_or(ERRNO::EOVERFLOW)?;
        msync_current_process(VirtAddr::from(addr), VirtAddr::from(end))?;
        Ok(0)
    })
}

pub fn sys_get_mempolicy(
    mode: *mut i32,
    nodemask: *mut u8,
    maxnode: usize,
    addr: usize,
    flags: u32,
) -> isize {
    syscall_body!({
        if flags & !GET_MEMPOLICY_SUPPORTED_FLAGS != 0 {
            return Err(ERRNO::EINVAL);
        }
        if flags & MPOL_F_MEMS_ALLOWED != 0 {
            if flags & (MPOL_F_NODE | MPOL_F_ADDR) != 0 {
                return Err(ERRNO::EINVAL);
            }
            if !mode.is_null() {
                return Err(ERRNO::EINVAL);
            }
        } else if flags & MPOL_F_ADDR == 0 && addr != 0 {
            return Err(ERRNO::EINVAL);
        }

        if flags & MPOL_F_MEMS_ALLOWED != 0 {
            write_ulong_mask_to_user(nodemask, maxnode, 1)?;
            return Ok(0);
        }

        if flags & MPOL_F_NODE != 0 {
            if mode.is_null() {
                return Err(ERRNO::EINVAL);
            }
            write_pod_to_user(mode, &0i32)?;
            if !nodemask.is_null() {
                write_ulong_mask_to_user(nodemask, maxnode, 1)?;
            }
            return Ok(0);
        }

        if !mode.is_null() {
            write_pod_to_user(mode, &MPOL_DEFAULT)?;
        }
        if !nodemask.is_null() {
            write_ulong_mask_to_user(nodemask, maxnode, 1)?;
        }
        Ok(0)
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
        // If no R/W/X bits are set (e.g., PROT_NONE), keep the U bit so the
        // region remains a user VMA, but deny all actual accesses.
        let perm = if prot & (MMapProt::PROT_READ.bits()
            | MMapProt::PROT_WRITE.bits()
            | MMapProt::PROT_EXEC.bits())
            == 0
        {
            MapPermission::U
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

pub fn sys_madvise(start: usize, len: usize, advice: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_madvise",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    // TODO take madvice
    syscall_body!({
        if start & ((1 << PAGE_SIZE_BITS) - 1) != 0 {
            return Err(ERRNO::EINVAL); // start not page-aligned
        }
        if len == 0 {
            return Ok(0); // POSIX/Linux: zero-length madvise is a successful no-op
        }
        warn!(
            "madvise(pid={} addr={:#x} len={} advice={}) is not implemented",
            current_task().unwrap().process.upgrade().unwrap().getpid(),
            start,
            len,
            advice
        );
        Ok(0)
    }
)
}
