//! System V shared memory support built on top of the existing file-backed
//! `MAP_SHARED` path.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use lazy_static::lazy_static;

use crate::fs::{
    canonicalize, open_file_at, unlinkat, AccessMode, FileDescription, FileStatusFlags, OpenFlags,
    OSInode,
};
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;

/// SysV IPC key type.
pub type ShmKey = i32;

/// `shmget` flag: create the segment if it does not already exist.
pub const IPC_CREAT: i32 = 0o1000;
/// `shmget` flag: fail if the segment already exists.
pub const IPC_EXCL: i32 = 0o2000;
/// `shmctl` command: remove the segment identifier.
pub const IPC_RMID: i32 = 0;

const SHM_HUGETLB_MASK: i32 = 0x7800;
const IPC_PRIVATE: ShmKey = 0;
const SHM_NAME_PREFIX: &str = "/.sysvshm.";

/// Per-segment kernel metadata.
pub struct ShmSegment {
    /// User-visible shared memory id.
    pub id: usize,
    /// Lookup key used by `shmget`.
    pub key: ShmKey,
    /// Segment size in bytes.
    pub size: usize,
    /// Creation flags snapshot (low permission bits preserved for future use).
    pub flags: i32,
    /// Hidden backing file description used by `MAP_SHARED`.
    pub desc: Arc<FileDescription>,
    /// Hidden backing file path, used for deferred unlink on `IPC_RMID`.
    pub path: String,
    /// Number of active process attachments.
    pub nattch: usize,
    /// Whether `IPC_RMID` has been requested.
    pub marked_for_removal: bool,
}

impl ShmSegment {
    fn new(id: usize, key: ShmKey, size: usize, flags: i32, desc: Arc<FileDescription>, path: String) -> Self {
        Self {
            id,
            key,
            size,
            flags,
            desc,
            path,
            nattch: 0,
            marked_for_removal: false,
        }
    }
}

struct ShmManager {
    next_id: usize,
    by_id: BTreeMap<usize, Arc<SpinNoIrqLock<ShmSegment>>>,
    by_key: BTreeMap<ShmKey, Arc<SpinNoIrqLock<ShmSegment>>>,
}

impl ShmManager {
    fn new() -> Self {
        Self {
            next_id: 1,
            by_id: BTreeMap::new(),
            by_key: BTreeMap::new(),
        }
    }

    fn alloc_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1).max(1);
        id
    }
}

lazy_static! {
    static ref SHM_MANAGER: SpinNoIrqLock<ShmManager> = SpinNoIrqLock::new(ShmManager::new());
}

fn build_segment_path(id: usize) -> String {
    format!("{}{}", SHM_NAME_PREFIX, id)
}

fn open_backing_file(path: &str, create: bool) -> Result<Arc<OSInode>, ERRNO> {
    let flags = if create {
        OpenFlags::CREATE | OpenFlags::EXCL | OpenFlags::RDWR
    } else {
        OpenFlags::RDWR
    };
    open_file_at("/", path, flags)
}

fn new_backing_desc(inode: Arc<OSInode>) -> Arc<FileDescription> {
    Arc::new(FileDescription::new(
        inode,
        AccessMode::ReadWrite,
        FileStatusFlags::empty(),
        0,
    ))
}

fn maybe_destroy_segment(segment: Arc<SpinNoIrqLock<ShmSegment>>) {
    let (should_destroy, key, id, path) = {
        let seg = segment.lock();
        (
            seg.marked_for_removal && seg.nattch == 0,
            seg.key,
            seg.id,
            seg.path.clone(),
        )
    };
    if !should_destroy {
        return;
    }
    {
        let mut manager = SHM_MANAGER.lock();
        manager.by_id.remove(&id);
        if key != IPC_PRIVATE {
            if let Some(existing) = manager.by_key.get(&key) {
                if Arc::ptr_eq(existing, &segment) {
                    manager.by_key.remove(&key);
                }
            }
        }
    }
    let _ = unlinkat("/", canonicalize("/", path.as_str()).as_str(), 0);
}

/// Create or look up a SysV shared memory segment and return its `shmid`.
pub fn shmget(key: ShmKey, size: usize, flags: i32) -> Result<usize, ERRNO> {
    if size == 0 {
        return Err(ERRNO::EINVAL);
    }
    if flags & SHM_HUGETLB_MASK != 0 {
        return Err(ERRNO::ENOSYS);
    }

    if key != IPC_PRIVATE {
        let mut manager = SHM_MANAGER.lock();
        if let Some(segment) = manager.by_key.get(&key).cloned() {
            let seg = segment.lock();
            if seg.marked_for_removal {
                return Err(ERRNO::EIDRM);
            }
            if (flags & IPC_CREAT != 0) && (flags & IPC_EXCL != 0) {
                return Err(ERRNO::EEXIST);
            }
            if size > seg.size {
                return Err(ERRNO::EINVAL);
            }
            return Ok(seg.id);
        }
        if flags & IPC_CREAT == 0 {
            return Err(ERRNO::ENOENT);
        }

        let id = manager.alloc_id();
        let path = build_segment_path(id);
        let inode = open_backing_file(path.as_str(), true)?;
        let desc = new_backing_desc(inode);
        desc.truncate(size)?;
        let segment = Arc::new(SpinNoIrqLock::new(ShmSegment::new(id, key, size, flags, desc, path)));
        manager.by_id.insert(id, Arc::clone(&segment));
        manager.by_key.insert(key, segment);
        return Ok(id);
    }

    let mut manager = SHM_MANAGER.lock();
    let id = manager.alloc_id();
    let path = build_segment_path(id);
    let inode = open_backing_file(path.as_str(), true)?;
    let desc = new_backing_desc(inode);
    desc.truncate(size)?;
    let segment = Arc::new(SpinNoIrqLock::new(ShmSegment::new(id, key, size, flags, desc, path)));
    manager.by_id.insert(id, segment);
    Ok(id)
}

/// Return a segment by `shmid`.
pub fn shm_by_id(id: usize) -> Result<Arc<SpinNoIrqLock<ShmSegment>>, ERRNO> {
    let manager = SHM_MANAGER.lock();
    let Some(segment) = manager.by_id.get(&id).cloned() else {
        return Err(ERRNO::EINVAL);
    };
    if segment.lock().marked_for_removal {
        return Err(ERRNO::EIDRM);
    }
    Ok(segment)
}

/// Increase the active attachment count of a segment.
pub fn attach_segment(id: usize) -> Result<Arc<SpinNoIrqLock<ShmSegment>>, ERRNO> {
    let segment = shm_by_id(id)?;
    {
        let mut seg = segment.lock();
        seg.nattch = seg.nattch.saturating_add(1);
    }
    Ok(segment)
}

/// Increase the active attachment count when the caller already knows the
/// segment exists, such as after `fork`.
pub fn retain_attached_segment(id: usize) {
    let segment = {
        let manager = SHM_MANAGER.lock();
        manager.by_id.get(&id).cloned()
    };
    if let Some(segment) = segment {
        let mut seg = segment.lock();
        seg.nattch = seg.nattch.saturating_add(1);
    }
}

/// Drop one active attachment for a segment.
pub fn detach_segment(id: usize) {
    let segment = {
        let manager = SHM_MANAGER.lock();
        manager.by_id.get(&id).cloned()
    };
    let Some(segment) = segment else {
        return;
    };
    {
        let mut seg = segment.lock();
        if seg.nattch > 0 {
            seg.nattch -= 1;
        }
    }
    maybe_destroy_segment(segment);
}

/// Mark a segment for removal; actual destruction is deferred until the last
/// attachment disappears.
pub fn mark_segment_for_removal(id: usize) -> Result<(), ERRNO> {
    let segment = {
        let manager = SHM_MANAGER.lock();
        manager.by_id.get(&id).cloned().ok_or(ERRNO::EINVAL)?
    };
    {
        let mut seg = segment.lock();
        seg.marked_for_removal = true;
        if seg.key != IPC_PRIVATE {
            SHM_MANAGER.lock().by_key.remove(&seg.key);
        }
    }
    maybe_destroy_segment(segment);
    Ok(())
}
