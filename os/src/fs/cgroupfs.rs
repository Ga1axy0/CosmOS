//! Minimal cgroup v2 pseudo filesystem.
//!
//! This implements the small cgroup2 surface needed by clone3
//! `CLONE_INTO_CGROUP`: mountability, nested cgroup directories, and
//! `cgroup.procs` membership reporting.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use fs::errno::FS_ERRNO;
use fs::remove_dentry;
use fs::vfs::{VfsAttrs, VfsFileType, VfsNode, STATFS_NAMELEN_DEFAULT};

use crate::fs::{empty_statfs, StatFs64};
use crate::sync::SpinNoIrqLock;

const CGROUP2_SUPER_MAGIC: u64 = 0x6367_7270;
const CGROUPFS_FS_ID: u64 = CGROUP2_SUPER_MAGIC;

static NEXT_CGROUP_INO: AtomicU64 = AtomicU64::new(1);

fn alloc_cgroup_ino() -> u64 {
    NEXT_CGROUP_INO.fetch_add(1, Ordering::Relaxed)
}

fn cgroup_statfs() -> StatFs64 {
    empty_statfs(
        CGROUP2_SUPER_MAGIC,
        crate::config::PAGE_SIZE as u64,
        CGROUPFS_FS_ID,
        STATFS_NAMELEN_DEFAULT,
    )
}

/// Directory node in the minimal cgroup v2 hierarchy.
#[derive(Clone)]
pub struct CgroupDirNode {
    state: Arc<CgroupDirState>,
}

struct CgroupDirState {
    ino: u64,
    inner: SpinNoIrqLock<CgroupDirInner>,
}

struct CgroupDirInner {
    children: BTreeMap<String, CgroupDirNode>,
    parent: Option<Weak<CgroupDirState>>,
    procs: Vec<usize>,
    subtree_control: String,
}

#[derive(Clone, Copy)]
enum CgroupFileKind {
    Controllers,
    SubtreeControl,
    Procs,
}

#[derive(Clone)]
struct CgroupFileNode {
    ino: u64,
    dir: CgroupDirNode,
    kind: CgroupFileKind,
}

impl CgroupDirNode {
    fn new_root() -> Self {
        Self {
            state: Arc::new(CgroupDirState {
                ino: alloc_cgroup_ino(),
                inner: SpinNoIrqLock::new(CgroupDirInner {
                    children: BTreeMap::new(),
                    parent: None,
                    procs: Vec::new(),
                    subtree_control: String::new(),
                }),
            }),
        }
    }

    fn new_child(parent: &Self) -> Self {
        Self {
            state: Arc::new(CgroupDirState {
                ino: alloc_cgroup_ino(),
                inner: SpinNoIrqLock::new(CgroupDirInner {
                    children: BTreeMap::new(),
                    parent: Some(Arc::downgrade(&parent.state)),
                    procs: Vec::new(),
                    subtree_control: String::new(),
                }),
            }),
        }
    }

    fn file(&self, kind: CgroupFileKind) -> Arc<dyn VfsNode> {
        Arc::new(CgroupFileNode {
            ino: alloc_cgroup_ino(),
            dir: self.clone(),
            kind,
        }) as Arc<dyn VfsNode>
    }

    /// Record a process as a member of this cgroup.
    pub fn add_proc(&self, pid: usize) {
        let mut inner = self.state.inner.lock();
        if !inner.procs.contains(&pid) {
            inner.procs.push(pid);
        }
    }

    fn render_procs(&self) -> String {
        let inner = self.state.inner.lock();
        let mut out = String::new();
        for pid in &inner.procs {
            out.push_str(&pid.to_string());
            out.push('\n');
        }
        out
    }
}

impl fmt::Debug for CgroupDirNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.state.inner.lock();
        let child_names: Vec<String> = inner.children.keys().cloned().collect();
        f.debug_struct("CgroupDirNode")
            .field("ino", &self.state.ino)
            .field("children", &child_names)
            .field("procs", &inner.procs)
            .field("has_parent", &inner.parent.is_some())
            .finish()
    }
}

impl fmt::Debug for CgroupFileNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CgroupFileNode")
            .field("ino", &self.ino)
            .finish()
    }
}

impl VfsNode for CgroupDirNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        let inner = self.state.inner.lock();
        let mut entries = Vec::new();
        entries.push((String::from("cgroup.controllers"), VfsFileType::Regular));
        entries.push((String::from("cgroup.subtree_control"), VfsFileType::Regular));
        entries.push((String::from("cgroup.procs"), VfsFileType::Regular));
        entries.extend(
            inner
                .children
                .keys()
                .cloned()
                .map(|name| (name, VfsFileType::Directory)),
        );
        entries
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        match name {
            "cgroup.controllers" => Some(self.file(CgroupFileKind::Controllers)),
            "cgroup.subtree_control" => Some(self.file(CgroupFileKind::SubtreeControl)),
            "cgroup.procs" => Some(self.file(CgroupFileKind::Procs)),
            _ => self
                .state
                .inner
                .lock()
                .children
                .get(name)
                .cloned()
                .map(|dir| Arc::new(dir) as Arc<dyn VfsNode>),
        }
    }

    fn create(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn mkdir(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let mut inner = self.state.inner.lock();
        if inner.children.contains_key(name) {
            return None;
        }
        let dir = CgroupDirNode::new_child(self);
        inner.children.insert(String::from(name), dir.clone());
        Some(Arc::new(dir) as Arc<dyn VfsNode>)
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

    fn rmdir(&self, name: &str) -> Result<(), FS_ERRNO> {
        let mut inner = self.state.inner.lock();
        let child = inner.children.get(name).ok_or(FS_ERRNO::ENOENT)?;
        let child_inner = child.state.inner.lock();
        if !child_inner.children.is_empty() {
            return Err(FS_ERRNO::ENOTEMPTY);
        }
        drop(child_inner);
        inner.children.remove(name);
        remove_dentry(CGROUPFS_FS_ID, self.state.ino, name);
        Ok(())
    }

    fn fs_id(&self) -> u64 {
        CGROUPFS_FS_ID
    }

    fn ino(&self) -> u64 {
        self.state.ino
    }

    fn nlink(&self) -> u32 {
        2
    }

    fn mode(&self) -> Option<u32> {
        Some(0o040755)
    }

    fn set_mode(&self, _mode: u32) -> Result<(), FS_ERRNO> {
        Ok(())
    }

    fn set_owner(&self, _uid: u32, _gid: u32) -> Result<(), FS_ERRNO> {
        Ok(())
    }

    fn stat_attrs(&self) -> VfsAttrs {
        VfsAttrs {
            mode: self.mode(),
            ino: self.state.ino,
            nlink: 2,
            size: 0,
            uid: Some(0),
            gid: Some(0),
            rdev: 0,
            atime: None,
            mtime: None,
            ctime: None,
        }
    }

    fn statfs(&self) -> Result<StatFs64, FS_ERRNO> {
        Ok(cgroup_statfs())
    }
}

impl VfsNode for CgroupFileNode {
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

    fn clear(&self) {}

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let content = match self.kind {
            CgroupFileKind::Controllers => "base\n".to_string(),
            CgroupFileKind::SubtreeControl => self.dir.state.inner.lock().subtree_control.clone(),
            CgroupFileKind::Procs => self.dir.render_procs(),
        };
        if offset >= content.len() {
            return 0;
        }
        let bytes = content.as_bytes();
        let len = core::cmp::min(buf.len(), bytes.len() - offset);
        buf[..len].copy_from_slice(&bytes[offset..offset + len]);
        len
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> usize {
        if matches!(self.kind, CgroupFileKind::SubtreeControl) {
            let value = core::str::from_utf8(buf).unwrap_or("").trim();
            self.dir.state.inner.lock().subtree_control = value.to_string();
        }
        buf.len()
    }

    fn fs_id(&self) -> u64 {
        CGROUPFS_FS_ID
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn size(&self) -> usize {
        match self.kind {
            CgroupFileKind::Controllers => "base\n".len(),
            CgroupFileKind::SubtreeControl => self.dir.state.inner.lock().subtree_control.len(),
            CgroupFileKind::Procs => self.dir.render_procs().len(),
        }
    }

    fn mode(&self) -> Option<u32> {
        Some(0o100644)
    }

    fn set_mode(&self, _mode: u32) -> Result<(), FS_ERRNO> {
        Ok(())
    }

    fn set_owner(&self, _uid: u32, _gid: u32) -> Result<(), FS_ERRNO> {
        Ok(())
    }

    fn stat_attrs(&self) -> VfsAttrs {
        VfsAttrs {
            mode: self.mode(),
            ino: self.ino,
            nlink: 1,
            size: self.size(),
            uid: Some(0),
            gid: Some(0),
            rdev: 0,
            atime: None,
            mtime: None,
            ctime: None,
        }
    }

    fn statfs(&self) -> Result<StatFs64, FS_ERRNO> {
        Ok(cgroup_statfs())
    }
}

/// Create a fresh cgroup v2 root node for a mount instance.
pub fn new_cgroup2_root() -> Arc<dyn VfsNode> {
    Arc::new(CgroupDirNode::new_root()) as Arc<dyn VfsNode>
}
