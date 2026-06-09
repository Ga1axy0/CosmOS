//! Minimal in-memory tmpfs backend.
//!
//! This backend is mountable through the existing virtual-root mount layer and
//! supports regular files, directories, `rename`, `unlink`, `rmdir`, and
//! basic `statfs` reporting.

use alloc::collections::{BTreeMap, btree_map::Entry};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use fs::errno::FS_ERRNO;
use fs::remove_dentry;
use fs::vfs::{
    InodeTime, VfsAttrs, VfsFileType, VfsNode, STATFS_MAGIC_TMPFS, STATFS_NAMELEN_DEFAULT,
};

use crate::config::PAGE_SIZE;
use crate::fs::{empty_statfs, StatFs64};
use crate::mm::{frame_alloc, FrameTracker};
use crate::sync::SpinNoIrqLock;

const TMPFS_FS_ID: u64 = STATFS_MAGIC_TMPFS;
static NEXT_TMPFS_INO: AtomicU64 = AtomicU64::new(1);

fn alloc_tmpfs_ino() -> u64 {
    NEXT_TMPFS_INO.fetch_add(1, Ordering::Relaxed)
}

fn tmpfs_statfs() -> StatFs64 {
    empty_statfs(
        STATFS_MAGIC_TMPFS,
        crate::config::PAGE_SIZE as u64,
        TMPFS_FS_ID,
        STATFS_NAMELEN_DEFAULT,
    )
}

fn tmpfs_page_index(offset: usize) -> u64 {
    (offset / PAGE_SIZE) as u64
}

fn tmpfs_page_start(page_idx: u64) -> usize {
    page_idx as usize * PAGE_SIZE
}

#[derive(Clone, Copy)]
struct TmpfsMeta {
    mode: u32,
    uid: u32,
    gid: u32,
    atime: Option<InodeTime>,
    mtime: Option<InodeTime>,
    ctime: Option<InodeTime>,
}

impl TmpfsMeta {
    fn new(mode: u32) -> Self {
        Self {
            mode,
            uid: 0,
            gid: 0,
            atime: None,
            mtime: None,
            ctime: None,
        }
    }
}

struct TmpfsFileState {
    ino: u64,
    inner: SpinNoIrqLock<TmpfsFileInner>,
}

struct TmpfsFileInner {
    size: usize,
    pages: BTreeMap<u64, FrameTracker>,
    meta: TmpfsMeta,
}

/// Regular tmpfs file node backed by sparse, page-sized slots.
#[derive(Clone)]
pub struct TmpfsFileNode {
    state: Arc<TmpfsFileState>,
}

impl TmpfsFileNode {
    fn new() -> Self {
        Self {
            state: Arc::new(TmpfsFileState {
                ino: alloc_tmpfs_ino(),
                inner: SpinNoIrqLock::new(TmpfsFileInner {
                    size: 0,
                    pages: BTreeMap::new(),
                    meta: TmpfsMeta::new(0o100666),
                }),
            }),
        }
    }
}

struct TmpfsSymlinkState {
    ino: u64,
    inner: SpinNoIrqLock<TmpfsSymlinkInner>,
}

struct TmpfsSymlinkInner {
    target: String,
    meta: TmpfsMeta,
}

/// Symbolic link node backed by an in-memory target path.
#[derive(Clone)]
pub struct TmpfsSymlinkNode {
    state: Arc<TmpfsSymlinkState>,
}

impl TmpfsSymlinkNode {
    fn new(target: &str) -> Self {
        Self {
            state: Arc::new(TmpfsSymlinkState {
                ino: alloc_tmpfs_ino(),
                inner: SpinNoIrqLock::new(TmpfsSymlinkInner {
                    target: String::from(target),
                    meta: TmpfsMeta::new(0o120777),
                }),
            }),
        }
    }
}

impl fmt::Debug for TmpfsFileNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.state.inner.lock();
        f.debug_struct("TmpfsFileNode")
            .field("ino", &self.state.ino)
            .field("size", &inner.size)
            .field("resident_pages", &inner.pages.len())
            .field("mode", &inner.meta.mode)
            .field("uid", &inner.meta.uid)
            .field("gid", &inner.meta.gid)
            .field("atime", &inner.meta.atime)
            .field("mtime", &inner.meta.mtime)
            .field("ctime", &inner.meta.ctime)
            .finish()
    }
}

impl fmt::Debug for TmpfsSymlinkNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.state.inner.lock();
        f.debug_struct("TmpfsSymlinkNode")
            .field("ino", &self.state.ino)
            .field("target", &inner.target)
            .field("mode", &inner.meta.mode)
            .field("uid", &inner.meta.uid)
            .field("gid", &inner.meta.gid)
            .field("atime", &inner.meta.atime)
            .field("mtime", &inner.meta.mtime)
            .field("ctime", &inner.meta.ctime)
            .finish()
    }
}

enum TmpfsNode {
    File(TmpfsFileNode),
    Dir(TmpfsDirNode),
    Symlink(TmpfsSymlinkNode),
}

impl TmpfsNode {
    fn as_vfs(self) -> Arc<dyn VfsNode> {
        match self {
            Self::File(file) => Arc::new(file) as Arc<dyn VfsNode>,
            Self::Dir(dir) => Arc::new(dir) as Arc<dyn VfsNode>,
            Self::Symlink(link) => Arc::new(link) as Arc<dyn VfsNode>,
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self, Self::Dir(_))
    }

    fn file_type(&self) -> VfsFileType {
        match self {
            Self::File(_) => VfsFileType::Regular,
            Self::Dir(_) => VfsFileType::Directory,
            Self::Symlink(_) => VfsFileType::Symlink,
        }
    }
}

struct TmpfsDirState {
    ino: u64,
    inner: SpinNoIrqLock<TmpfsDirInner>,
}

struct TmpfsDirInner {
    children: BTreeMap<String, TmpfsNode>,
    parent: Option<Weak<TmpfsDirState>>,
    meta: TmpfsMeta,
}

/// Directory tmpfs node that owns a map of child entries.
#[derive(Clone)]
pub struct TmpfsDirNode {
    state: Arc<TmpfsDirState>,
}

impl TmpfsDirNode {
    fn new_root() -> Self {
        Self {
            state: Arc::new(TmpfsDirState {
                ino: alloc_tmpfs_ino(),
                inner: SpinNoIrqLock::new(TmpfsDirInner {
                    children: BTreeMap::new(),
                    parent: None,
                    meta: TmpfsMeta::new(0o040777),
                }),
            }),
        }
    }

    fn new_child(parent: &Self) -> Self {
        Self {
            state: Arc::new(TmpfsDirState {
                ino: alloc_tmpfs_ino(),
                inner: SpinNoIrqLock::new(TmpfsDirInner {
                    children: BTreeMap::new(),
                    parent: Some(Arc::downgrade(&parent.state)),
                    meta: TmpfsMeta::new(0o040777),
                }),
            }),
        }
    }

    fn parent_dir(&self) -> Option<Self> {
        self.state
            .inner
            .lock()
            .parent
            .as_ref()
            .and_then(|parent| parent.upgrade())
            .map(|state| Self { state })
    }

    fn set_parent(&self, parent: Option<&Self>) {
        self.state.inner.lock().parent = parent.map(|dir| Arc::downgrade(&dir.state));
    }

    fn is_ancestor_of(&self, candidate: &Self) -> bool {
        let mut cur = Some(candidate.clone());
        while let Some(dir) = cur {
            if Arc::ptr_eq(&self.state, &dir.state) {
                return true;
            }
            cur = dir.parent_dir();
        }
        false
    }

    fn child_as_vfs(node: &TmpfsNode) -> Arc<dyn VfsNode> {
        match node {
            TmpfsNode::File(file) => Arc::new(file.clone()) as Arc<dyn VfsNode>,
            TmpfsNode::Dir(dir) => Arc::new(dir.clone()) as Arc<dyn VfsNode>,
            TmpfsNode::Symlink(link) => Arc::new(link.clone()) as Arc<dyn VfsNode>,
        }
    }
}

impl fmt::Debug for TmpfsDirNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.state.inner.lock();
        let child_names: Vec<String> = inner.children.keys().cloned().collect();
        f.debug_struct("TmpfsDirNode")
            .field("ino", &self.state.ino)
            .field("child_count", &child_names.len())
            .field("children", &child_names)
            .field("has_parent", &inner.parent.is_some())
            .field("mode", &inner.meta.mode)
            .field("uid", &inner.meta.uid)
            .field("gid", &inner.meta.gid)
            .field("atime", &inner.meta.atime)
            .field("mtime", &inner.meta.mtime)
            .field("ctime", &inner.meta.ctime)
            .finish()
    }
}

impl VfsNode for TmpfsFileNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        Vec::new()
    }

    fn find(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn create(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn mkdir(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn clear(&self) {
        let mut inner = self.state.inner.lock();
        inner.size = 0;
        inner.pages.clear();
    }

    fn truncate(&self, new_size: usize) -> Result<(), FS_ERRNO> {
        let mut inner = self.state.inner.lock();
        let old_size = inner.size;
        if new_size >= old_size {
            inner.size = new_size;
            return Ok(());
        }

        if new_size == 0 {
            inner.size = 0;
            inner.pages.clear();
            return Ok(());
        }

        let keep_tail_idx = tmpfs_page_index(new_size.saturating_sub(1));
        let keep_tail_valid = new_size - tmpfs_page_start(keep_tail_idx);
        if let Some(page) = inner.pages.get(&keep_tail_idx) {
            page.ppn.get_bytes_array()[keep_tail_valid..].fill(0);
        }

        let first_removed_idx = keep_tail_idx.saturating_add(1);
        let removed_indices: Vec<_> = inner
            .pages
            .range(first_removed_idx..)
            .map(|(&idx, _)| idx)
            .collect();
        for page_idx in removed_indices {
            inner.pages.remove(&page_idx);
        }
        inner.size = new_size;
        Ok(())
    }

    fn fallocate(&self, mode: i32, offset: usize, len: usize) -> Result<(), FS_ERRNO> {
        if mode != 0 {
            return Err(FS_ERRNO::EOPNOTSUPP);
        }
        let new_size = offset.checked_add(len).ok_or(FS_ERRNO::EINVAL)?;
        let mut inner = self.state.inner.lock();
        inner.size = inner.size.max(new_size);
        Ok(())
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let inner = self.state.inner.lock();
        if offset >= inner.size || buf.is_empty() {
            return 0;
        }
        let len = core::cmp::min(buf.len(), inner.size - offset);
        let out = &mut buf[..len];
        out.fill(0);

        let end = offset + len;
        let first_page = tmpfs_page_index(offset);
        let last_page = tmpfs_page_index(end.saturating_sub(1));
        for page_idx in first_page..=last_page {
            let Some(page) = inner.pages.get(&page_idx) else {
                continue;
            };
            let page_start = tmpfs_page_start(page_idx);
            let copy_start = core::cmp::max(offset, page_start);
            let copy_end = core::cmp::min(end, page_start + PAGE_SIZE);
            let src = &page.ppn.get_bytes_array()[copy_start - page_start..copy_end - page_start];
            out[copy_start - offset..copy_end - offset].copy_from_slice(src);
        }
        len
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        let mut inner = self.state.inner.lock();
        let end = offset.saturating_add(buf.len());
        inner.size = inner.size.max(end);

        let mut written = 0usize;
        while written < buf.len() {
            let file_off = offset + written;
            let page_idx = tmpfs_page_index(file_off);
            let page_off = file_off % PAGE_SIZE;
            let copy_len = core::cmp::min(PAGE_SIZE - page_off, buf.len() - written);
            let page = match inner.pages.entry(page_idx) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => entry.insert(
                    frame_alloc().expect("tmpfs page allocation failed while writing file"),
                ),
            };
            let bytes = page.ppn.get_bytes_array();
            bytes[page_off..page_off + copy_len]
                .copy_from_slice(&buf[written..written + copy_len]);
            written += copy_len;
        }
        buf.len()
    }

    fn fs_id(&self) -> u64 {
        TMPFS_FS_ID
    }

    fn ino(&self) -> u64 {
        self.state.ino
    }

    fn nlink(&self) -> u32 {
        1
    }

    fn size(&self) -> usize {
        self.state.inner.lock().size
    }

    fn mode(&self) -> Option<u32> {
        Some(self.state.inner.lock().meta.mode)
    }

    fn set_mode(&self, mode: u32) -> Result<(), FS_ERRNO> {
        self.state.inner.lock().meta.mode = mode;
        Ok(())
    }

    fn uid(&self) -> Option<u32> {
        Some(self.state.inner.lock().meta.uid)
    }

    fn gid(&self) -> Option<u32> {
        Some(self.state.inner.lock().meta.gid)
    }

    fn set_owner(&self, uid: u32, gid: u32) -> Result<(), FS_ERRNO> {
        let mut inner = self.state.inner.lock();
        inner.meta.uid = uid;
        inner.meta.gid = gid;
        Ok(())
    }

    fn atime(&self) -> Option<InodeTime> {
        self.state.inner.lock().meta.atime
    }

    fn mtime(&self) -> Option<InodeTime> {
        self.state.inner.lock().meta.mtime
    }

    fn ctime(&self) -> Option<InodeTime> {
        self.state.inner.lock().meta.ctime
    }

    fn set_times(
        &self,
        atime: Option<InodeTime>,
        mtime: Option<InodeTime>,
        ctime: Option<InodeTime>,
    ) -> Result<(), FS_ERRNO> {
        let mut inner = self.state.inner.lock();
        inner.meta.atime = atime;
        inner.meta.mtime = mtime;
        inner.meta.ctime = ctime;
        Ok(())
    }

    fn stat_attrs(&self) -> VfsAttrs {
        let inner = self.state.inner.lock();
        VfsAttrs {
            mode: Some(inner.meta.mode),
            ino: self.state.ino,
            nlink: 1,
            size: inner.size,
            uid: Some(inner.meta.uid),
            gid: Some(inner.meta.gid),
            rdev: 0,
            atime: inner.meta.atime,
            mtime: inner.meta.mtime,
            ctime: inner.meta.ctime,
        }
    }

    fn statfs(&self) -> Result<StatFs64, FS_ERRNO> {
        Ok(tmpfs_statfs())
    }
}

impl VfsNode for TmpfsSymlinkNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        Vec::new()
    }

    fn find(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn create(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn mkdir(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Symlink
    }

    fn read_link(&self) -> Result<String, FS_ERRNO> {
        Ok(self.state.inner.lock().target.clone())
    }

    fn clear(&self) {}

    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        0
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn fs_id(&self) -> u64 {
        TMPFS_FS_ID
    }

    fn ino(&self) -> u64 {
        self.state.ino
    }

    fn nlink(&self) -> u32 {
        1
    }

    fn size(&self) -> usize {
        self.state.inner.lock().target.len()
    }

    fn mode(&self) -> Option<u32> {
        Some(self.state.inner.lock().meta.mode)
    }

    fn set_mode(&self, mode: u32) -> Result<(), FS_ERRNO> {
        self.state.inner.lock().meta.mode = mode;
        Ok(())
    }

    fn uid(&self) -> Option<u32> {
        Some(self.state.inner.lock().meta.uid)
    }

    fn gid(&self) -> Option<u32> {
        Some(self.state.inner.lock().meta.gid)
    }

    fn set_owner(&self, uid: u32, gid: u32) -> Result<(), FS_ERRNO> {
        let mut inner = self.state.inner.lock();
        inner.meta.uid = uid;
        inner.meta.gid = gid;
        Ok(())
    }

    fn atime(&self) -> Option<InodeTime> {
        self.state.inner.lock().meta.atime
    }

    fn mtime(&self) -> Option<InodeTime> {
        self.state.inner.lock().meta.mtime
    }

    fn ctime(&self) -> Option<InodeTime> {
        self.state.inner.lock().meta.ctime
    }

    fn set_times(
        &self,
        atime: Option<InodeTime>,
        mtime: Option<InodeTime>,
        ctime: Option<InodeTime>,
    ) -> Result<(), FS_ERRNO> {
        let mut inner = self.state.inner.lock();
        inner.meta.atime = atime;
        inner.meta.mtime = mtime;
        inner.meta.ctime = ctime;
        Ok(())
    }

    fn stat_attrs(&self) -> VfsAttrs {
        let inner = self.state.inner.lock();
        VfsAttrs {
            mode: Some(inner.meta.mode),
            ino: self.state.ino,
            nlink: 1,
            size: inner.target.len(),
            uid: Some(inner.meta.uid),
            gid: Some(inner.meta.gid),
            rdev: 0,
            atime: inner.meta.atime,
            mtime: inner.meta.mtime,
            ctime: inner.meta.ctime,
        }
    }

    fn statfs(&self) -> Result<StatFs64, FS_ERRNO> {
        Ok(tmpfs_statfs())
    }
}

impl VfsNode for TmpfsDirNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        self.state
            .inner
            .lock()
            .children
            .iter()
            .map(|(name, node)| (name.clone(), node.file_type()))
            .collect()
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let inner = self.state.inner.lock();
        inner.children.get(name).map(Self::child_as_vfs)
    }

    fn create(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let mut inner = self.state.inner.lock();
        if inner.children.contains_key(name) {
            return None;
        }
        let file = TmpfsFileNode::new();
        let node = TmpfsNode::File(file.clone());
        inner.children.insert(String::from(name), node);
        Some(Arc::new(file) as Arc<dyn VfsNode>)
    }

    fn mkdir(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let mut inner = self.state.inner.lock();
        if inner.children.contains_key(name) {
            return None;
        }
        let dir = TmpfsDirNode::new_child(self);
        let node = TmpfsNode::Dir(dir.clone());
        inner.children.insert(String::from(name), node);
        Some(Arc::new(dir) as Arc<dyn VfsNode>)
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn VfsNode>, FS_ERRNO> {
        let mut inner = self.state.inner.lock();
        if inner.children.contains_key(name) {
            return Err(FS_ERRNO::EEXIST);
        }
        let link = TmpfsSymlinkNode::new(target);
        let node = TmpfsNode::Symlink(link.clone());
        inner.children.insert(String::from(name), node);
        Ok(Arc::new(link) as Arc<dyn VfsNode>)
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Directory
    }

    fn clear(&self) {}

    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        0
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn fs_id(&self) -> u64 {
        TMPFS_FS_ID
    }

    fn ino(&self) -> u64 {
        self.state.ino
    }

    fn nlink(&self) -> u32 {
        2
    }

    fn mode(&self) -> Option<u32> {
        Some(self.state.inner.lock().meta.mode)
    }

    fn set_mode(&self, mode: u32) -> Result<(), FS_ERRNO> {
        self.state.inner.lock().meta.mode = mode;
        Ok(())
    }

    fn uid(&self) -> Option<u32> {
        Some(self.state.inner.lock().meta.uid)
    }

    fn gid(&self) -> Option<u32> {
        Some(self.state.inner.lock().meta.gid)
    }

    fn set_owner(&self, uid: u32, gid: u32) -> Result<(), FS_ERRNO> {
        let mut inner = self.state.inner.lock();
        inner.meta.uid = uid;
        inner.meta.gid = gid;
        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), FS_ERRNO> {
        let mut inner = self.state.inner.lock();
        match inner.children.get(name) {
            Some(node) if node.is_dir() => Err(FS_ERRNO::EISDIR),
            Some(_) => {
                inner.children.remove(name);
                remove_dentry(TMPFS_FS_ID, self.state.ino, name);
                Ok(())
            }
            None => Err(FS_ERRNO::ENOENT),
        }
    }

    fn rmdir(&self, name: &str) -> Result<(), FS_ERRNO> {
        let mut inner = self.state.inner.lock();
        let node = inner.children.get(name).ok_or(FS_ERRNO::ENOENT)?;
        let TmpfsNode::Dir(child_dir) = node else {
            return Err(FS_ERRNO::ENOTDIR);
        };
        if !child_dir.state.inner.lock().children.is_empty() {
            return Err(FS_ERRNO::ENOTEMPTY);
        }
        inner.children.remove(name);
        remove_dentry(TMPFS_FS_ID, self.state.ino, name);
        Ok(())
    }

    fn atime(&self) -> Option<InodeTime> {
        self.state.inner.lock().meta.atime
    }

    fn mtime(&self) -> Option<InodeTime> {
        self.state.inner.lock().meta.mtime
    }

    fn ctime(&self) -> Option<InodeTime> {
        self.state.inner.lock().meta.ctime
    }

    fn set_times(
        &self,
        atime: Option<InodeTime>,
        mtime: Option<InodeTime>,
        ctime: Option<InodeTime>,
    ) -> Result<(), FS_ERRNO> {
        let mut inner = self.state.inner.lock();
        inner.meta.atime = atime;
        inner.meta.mtime = mtime;
        inner.meta.ctime = ctime;
        Ok(())
    }

    fn stat_attrs(&self) -> VfsAttrs {
        let inner = self.state.inner.lock();
        VfsAttrs {
            mode: Some(inner.meta.mode),
            ino: self.state.ino,
            nlink: 2,
            size: 0,
            uid: Some(inner.meta.uid),
            gid: Some(inner.meta.gid),
            rdev: 0,
            atime: inner.meta.atime,
            mtime: inner.meta.mtime,
            ctime: inner.meta.ctime,
        }
    }

    fn rename_child(
        &self,
        old_name: &str,
        new_parent: &Arc<dyn VfsNode>,
        new_name: &str,
    ) -> Result<(), FS_ERRNO> {
        let dst_dir = new_parent
            .as_any()
            .downcast_ref::<TmpfsDirNode>()
            .ok_or(FS_ERRNO::EXDEV)?;

        if old_name == "." || old_name == ".." || new_name == "." || new_name == ".." {
            return Err(FS_ERRNO::EINVAL);
        }

        let mut src_inner = self.state.inner.lock();
        let node = src_inner.children.remove(old_name).ok_or(FS_ERRNO::ENOENT)?;
        drop(src_inner);

        if let TmpfsNode::Dir(node_dir) = &node {
            if node_dir.is_ancestor_of(dst_dir) {
                let mut src_inner = self.state.inner.lock();
                src_inner.children.insert(String::from(old_name), node);
                return Err(FS_ERRNO::EINVAL);
            }
        }

        let mut dst_inner = dst_dir.state.inner.lock();
        if let Some(existing) = dst_inner.children.get(new_name) {
            if existing.is_dir() != node.is_dir() {
                let mut src_inner = self.state.inner.lock();
                src_inner.children.insert(String::from(old_name), node);
                return if existing.is_dir() {
                    Err(FS_ERRNO::EISDIR)
                } else {
                    Err(FS_ERRNO::ENOTDIR)
                };
            }
            if let TmpfsNode::Dir(existing_dir) = existing {
                if !existing_dir.state.inner.lock().children.is_empty() {
                    let mut src_inner = self.state.inner.lock();
                    src_inner.children.insert(String::from(old_name), node);
                    return Err(FS_ERRNO::ENOTEMPTY);
                }
            }
        }
        if dst_inner.children.remove(new_name).is_some() {
            remove_dentry(TMPFS_FS_ID, dst_dir.state.ino, new_name);
        }
        if let TmpfsNode::Dir(dir) = &node {
            dir.set_parent(Some(dst_dir));
        }
        dst_inner.children.insert(String::from(new_name), node);
        remove_dentry(TMPFS_FS_ID, self.state.ino, old_name);
        Ok(())
    }

    fn statfs(&self) -> Result<StatFs64, FS_ERRNO> {
        Ok(tmpfs_statfs())
    }
}

/// Create a fresh tmpfs root directory suitable for mounting.
pub fn new_tmpfs_root() -> Arc<dyn VfsNode> {
    TmpfsNode::Dir(TmpfsDirNode::new_root()).as_vfs()
}
