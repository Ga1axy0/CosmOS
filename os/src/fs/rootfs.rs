//! Virtual in-memory root filesystem.
//!
//! [`VirtualDirNode`] is a lightweight synthetic directory that sits at the
//! top of the VFS namespace.  It supports two sources of children:
//!
//! 1. **`mounts`** – explicit name bindings inserted by [`do_mount`].  These
//!    have the highest priority and shadow the overlay at the same name.
//! 2. **`overlay`** – an optional real-filesystem directory node.  All
//!    lookups and mutations that are not covered by `mounts` are forwarded
//!    here transparently.
//!
//! # Mount design
//!
//! The global [`VIRT_ROOT`] is the kernel's true VFS root.  On startup
//! [`init_rootfs`](super::inode::init_rootfs) mounts the compiled-in
//! filesystem (ext4 / fat32 / easyfs) **at `"/"` as the overlay** of
//! `VIRT_ROOT`, which keeps all existing `/bin/…`, `/etc/…` etc. paths
//! working without any changes.
//!
//! Additional filesystems can later be mounted at sub-paths such as
//! `"/mnt/fat32"`.  [`do_mount`](super::inode::do_mount) creates virtual
//! intermediate directories as needed and binds the FS root there.
//!
//! # Future extension
//!
//! `do_mount` / `do_umount` are currently kernel-internal.  Adding
//! `sys_mount` / `sys_umount2` syscalls only requires thin wrappers in
//! `syscall/fs.rs` that translate user arguments and call these functions.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use fs::errno::FS_ERRNO;
use fs::remove_dentry;
use fs::vfs::{InodeTime, VfsFileType, VfsNode};
use lazy_static::*;

use crate::sync::{SpinNoIrqLock};

const MEMFS_ID: u64 = u64::MAX - 1;

// ---------------------------------------------------------------------------
// Inode-number allocator for virtual nodes
// ---------------------------------------------------------------------------

static NEXT_VIRT_INO: AtomicU64 = AtomicU64::new(1);

fn alloc_virt_ino() -> u64 {
    NEXT_VIRT_INO.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// VirtDirInner – the mutable state of a virtual directory
// ---------------------------------------------------------------------------

struct VirtDirInner {
    /// Optional real-FS directory that handles names not present in `mounts`.
    overlay: Option<Arc<dyn VfsNode>>,
    /// Explicit child bindings (mount points / virtual sub-dirs).
    mounts: BTreeMap<String, Arc<dyn VfsNode>>,
}

// ---------------------------------------------------------------------------
// VirtualDirNode
// ---------------------------------------------------------------------------

/// A synthetic in-memory directory node.
///
/// All VFS trait methods either consult `mounts` first and fall through to
/// `overlay`, or (for write operations) delegate directly to `overlay`.
pub struct VirtualDirNode {
    ino: u64,
    inner: SpinNoIrqLock<VirtDirInner>,
}

// SAFETY: SpinNoIrqLock now uses an atomic spinlock internally, so concurrent
// access from multiple harts is properly serialised.
unsafe impl Send for VirtualDirNode {}
unsafe impl Sync for VirtualDirNode {}

impl VirtualDirNode {
    /// Create a new, empty virtual directory.
    pub fn new() -> Arc<Self> {
        // SAFETY: single-processor guarantee documented above.
        let inner = unsafe {
            SpinNoIrqLock::new(VirtDirInner {
                overlay: None,
                mounts: BTreeMap::new(),
            })
        };
        Arc::new(Self {
            ino: alloc_virt_ino(),
            inner,
        })
    }

    /// Replace (or install) the overlay backing node.
    ///
    /// Typically called once at boot to set an ext4/fat32 root as the overlay
    /// of [`VIRT_ROOT`], making the whole on-disk tree visible at `/`.
    pub fn set_overlay(&self, node: Arc<dyn VfsNode>) {
        self.inner.lock().overlay = Some(node);
    }

    /// Bind `name` → `node` as an explicit child of this directory.
    ///
    /// Takes priority over the overlay at the same name.  Overwrites any
    /// previous binding.
    pub fn bind(&self, name: &str, node: Arc<dyn VfsNode>) {
        self.inner
            .lock()
            .mounts
            .insert(String::from(name), node);
        // Invalidate cached overlay dentry at this name.
        remove_dentry(u64::MAX, self.ino, name);
    }

    /// Remove the explicit binding for `name`.
    ///
    /// Returns `true` if the binding existed and was removed.
    pub fn unbind(&self, name: &str) -> bool {
        let existed = self.inner.lock().mounts.remove(name).is_some();
        if existed {
            // Invalidate cached mount dentry at this name.
            remove_dentry(u64::MAX, self.ino, name);
        }
        existed
    }

    /// If the overlay contains a *directory* child named `name`, return it.
    ///
    /// Used by `ensure_virtual_dir` to pre-populate the overlay of a newly
    /// created virtual sub-directory so that its contents remain reachable.
    pub(crate) fn overlay_child_dir(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let inner = self.inner.lock();
        inner
            .overlay
            .as_ref()
            .and_then(|ov| ov.find(name))
            .filter(|n| n.is_dir())
    }

    fn has_mount(&self, name: &str) -> bool {
        self.inner.lock().mounts.contains_key(name)
    }

    fn overlay_node(&self) -> Option<Arc<dyn VfsNode>> {
        self.inner.lock().overlay.clone()
    }
}

struct MemFileInner {
    data: Vec<u8>,
    mode: u32,
    atime: Option<InodeTime>,
    mtime: Option<InodeTime>,
    ctime: Option<InodeTime>,
}

/// Minimal in-memory regular file used by `/dev/shm`.
pub struct MemFileNode {
    ino: u64,
    inner: SpinNoIrqLock<MemFileInner>,
}

impl MemFileNode {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            ino: alloc_virt_ino(),
            inner: SpinNoIrqLock::new(MemFileInner {
                data: Vec::new(),
                mode: 0o100666,
                atime: None,
                mtime: None,
                ctime: None,
            }),
        })
    }
}

impl VfsNode for MemFileNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn ls(&self) -> Vec<(String, bool)> {
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

    fn is_dir(&self) -> bool {
        false
    }

    fn clear(&self) {
        self.inner.lock().data.clear();
    }

    fn truncate(&self, new_size: usize) -> Result<(), FS_ERRNO> {
        self.inner.lock().data.resize(new_size, 0);
        Ok(())
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let inner = self.inner.lock();
        if offset >= inner.data.len() || buf.is_empty() {
            return 0;
        }
        let len = core::cmp::min(buf.len(), inner.data.len() - offset);
        buf[..len].copy_from_slice(&inner.data[offset..offset + len]);
        len
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        let mut inner = self.inner.lock();
        let end = offset.saturating_add(buf.len());
        if end > inner.data.len() {
            inner.data.resize(end, 0);
        }
        inner.data[offset..end].copy_from_slice(buf);
        buf.len()
    }

    fn fs_id(&self) -> u64 {
        MEMFS_ID
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn nlink(&self) -> u32 {
        1
    }

    fn size(&self) -> usize {
        self.inner.lock().data.len()
    }

    fn mode(&self) -> Option<u32> {
        Some(self.inner.lock().mode)
    }

    fn set_mode(&self, mode: u32) -> Result<(), FS_ERRNO> {
        self.inner.lock().mode = mode;
        Ok(())
    }

    fn atime(&self) -> Option<InodeTime> {
        self.inner.lock().atime
    }

    fn mtime(&self) -> Option<InodeTime> {
        self.inner.lock().mtime
    }

    fn ctime(&self) -> Option<InodeTime> {
        self.inner.lock().ctime
    }

    fn set_times(
        &self,
        atime: Option<InodeTime>,
        mtime: Option<InodeTime>,
        ctime: Option<InodeTime>,
    ) -> Result<(), FS_ERRNO> {
        let mut inner = self.inner.lock();
        inner.atime = atime;
        inner.mtime = mtime;
        inner.ctime = ctime;
        Ok(())
    }
}

struct MemDirInner {
    children: BTreeMap<String, Arc<dyn VfsNode>>,
    mode: u32,
    atime: Option<InodeTime>,
    mtime: Option<InodeTime>,
    ctime: Option<InodeTime>,
}

/// Minimal in-memory directory used to back POSIX shared memory objects.
pub struct MemDirNode {
    ino: u64,
    inner: SpinNoIrqLock<MemDirInner>,
}

impl MemDirNode {
    /// Create a new empty in-memory directory for synthetic kernel-backed paths.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            ino: alloc_virt_ino(),
            inner: SpinNoIrqLock::new(MemDirInner {
                children: BTreeMap::new(),
                mode: 0o040777,
                atime: None,
                mtime: None,
                ctime: None,
            }),
        })
    }
}

impl VfsNode for MemDirNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn ls(&self) -> Vec<(String, bool)> {
        self.inner
            .lock()
            .children
            .iter()
            .map(|(name, node)| (name.clone(), node.is_dir()))
            .collect()
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        self.inner.lock().children.get(name).cloned()
    }

    fn create(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let mut inner = self.inner.lock();
        if inner.children.contains_key(name) {
            return None;
        }
        let node: Arc<dyn VfsNode> = MemFileNode::new();
        inner.children.insert(String::from(name), Arc::clone(&node));
        Some(node)
    }

    fn mkdir(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let mut inner = self.inner.lock();
        if inner.children.contains_key(name) {
            return None;
        }
        let node: Arc<dyn VfsNode> = Self::new();
        inner.children.insert(String::from(name), Arc::clone(&node));
        Some(node)
    }

    fn is_dir(&self) -> bool {
        true
    }

    fn clear(&self) {}

    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        0
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn fs_id(&self) -> u64 {
        MEMFS_ID
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn nlink(&self) -> u32 {
        2
    }

    fn mode(&self) -> Option<u32> {
        Some(self.inner.lock().mode)
    }

    fn set_mode(&self, mode: u32) -> Result<(), FS_ERRNO> {
        self.inner.lock().mode = mode;
        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), FS_ERRNO> {
        let mut inner = self.inner.lock();
        match inner.children.get(name) {
            Some(node) if node.is_dir() => Err(FS_ERRNO::EISDIR),
            Some(_) => {
                inner.children.remove(name);
                remove_dentry(MEMFS_ID, self.ino, name);
                Ok(())
            }
            None => Err(FS_ERRNO::ENOENT),
        }
    }

    fn rmdir(&self, name: &str) -> Result<(), FS_ERRNO> {
        let mut inner = self.inner.lock();
        let node = inner.children.get(name).cloned().ok_or(FS_ERRNO::ENOENT)?;
        let child_dir = node
            .as_any()
            .downcast_ref::<Self>()
            .ok_or(FS_ERRNO::ENOTDIR)?;
        if !child_dir.inner.lock().children.is_empty() {
            return Err(FS_ERRNO::ENOTEMPTY);
        }
        inner.children.remove(name);
        remove_dentry(MEMFS_ID, self.ino, name);
        Ok(())
    }

    fn atime(&self) -> Option<InodeTime> {
        self.inner.lock().atime
    }

    fn mtime(&self) -> Option<InodeTime> {
        self.inner.lock().mtime
    }

    fn ctime(&self) -> Option<InodeTime> {
        self.inner.lock().ctime
    }

    fn set_times(
        &self,
        atime: Option<InodeTime>,
        mtime: Option<InodeTime>,
        ctime: Option<InodeTime>,
    ) -> Result<(), FS_ERRNO> {
        let mut inner = self.inner.lock();
        inner.atime = atime;
        inner.mtime = mtime;
        inner.ctime = ctime;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// VfsNode implementation
// ---------------------------------------------------------------------------

impl VfsNode for VirtualDirNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Directory
    }

    fn fs_id(&self) -> u64 {
        // Use a sentinel so the dentry cache covers virtual-directory lookups.
        u64::MAX
    }

    fn ino(&self) -> u64 {
        self.ino
    }

    fn nlink(&self) -> u32 {
        2
    }

    // -----------------------------------------------------------------------
    // Directory enumeration
    // -----------------------------------------------------------------------

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        // Phase 1: collect (name, file_type) from overlay.
        let mut entries: Vec<(String, VfsFileType)> = {
            let inner = self.inner.lock();
            match inner.overlay.as_ref() {
                Some(ov) => ov.ls(),
                None => Vec::new(),
            }
        };

        // Phase 2: add explicit mount names that the overlay doesn't list.
        let mount_entries: Vec<(String, VfsFileType)> = {
            let inner = self.inner.lock();
            inner
                .mounts
                .iter()
                .map(|(name, node)| (name.clone(), node.file_type()))
                .collect()
        };

        for (key, file_type) in mount_entries {
            if !entries.iter().any(|(name, _)| name == &key) {
                entries.push((key, file_type));
            }
        }
        entries
    }

    // -----------------------------------------------------------------------
    // Lookup
    // -----------------------------------------------------------------------

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        // Step 1: explicit mounts take priority.
        {
            let inner = self.inner.lock();
            if let Some(node) = inner.mounts.get(name) {
                return Some(Arc::clone(node));
            }
        }
        // Step 2: fall through to overlay.
        let overlay = {
            let inner = self.inner.lock();
            inner.overlay.clone()
        };
        overlay?.find(name)
    }

    // -----------------------------------------------------------------------
    // Create / mkdir
    // -----------------------------------------------------------------------

    fn create(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        // File creation is entirely delegated to the overlay.
        let overlay = {
            let inner = self.inner.lock();
            inner.overlay.clone()
        };
        overlay?.create(name)
    }

    fn mkdir(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        // Prefer the overlay so the directory ends up on-disk.
        let from_overlay: Option<Arc<dyn VfsNode>> = {
            let inner = self.inner.lock();
            inner.overlay.as_ref().and_then(|ov| ov.mkdir(name))
        };
        if let Some(new_node) = from_overlay {
            return Some(new_node);
        }
        // Fallback: create a virtual in-memory sub-directory.
        let new_dir = VirtualDirNode::new();
        self.bind(name, Arc::clone(&new_dir) as Arc<dyn VfsNode>);
        Some(new_dir as Arc<dyn VfsNode>)
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn VfsNode>, FS_ERRNO> {
        let overlay = {
            let inner = self.inner.lock();
            inner.overlay.clone()
        };
        overlay.ok_or(FS_ERRNO::EPERM)?.symlink(name, target)
    }

    // -----------------------------------------------------------------------
    // Data I/O – directories carry no file content
    // -----------------------------------------------------------------------

    fn clear(&self) {}

    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        0
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    // -----------------------------------------------------------------------
    // Hard-link / remove – delegate to overlay; protect virtual mounts
    // -----------------------------------------------------------------------

    fn link(&self, old_name: &str, new_name: &str) -> Result<(), FS_ERRNO> {
        let overlay = {
            let inner = self.inner.lock();
            inner.overlay.clone()
        };
        overlay.ok_or(FS_ERRNO::EPERM)?.link(old_name, new_name)
    }

    fn link_inode(&self, child: &Arc<dyn VfsNode>, new_name: &str) -> Result<(), FS_ERRNO> {
        let overlay = {
            let inner = self.inner.lock();
            inner.overlay.clone()
        };
        overlay.ok_or(FS_ERRNO::EPERM)?.link_inode(child, new_name)
    }

    fn unlink(&self, name: &str) -> Result<(), FS_ERRNO> {
        // Refuse to unlink a live virtual mount point.
        {
            let inner = self.inner.lock();
            if inner.mounts.contains_key(name) {
                return Err(FS_ERRNO::EBUSY);
            }
        }
        let overlay = {
            let inner = self.inner.lock();
            inner.overlay.clone()
        };
        overlay.ok_or(FS_ERRNO::EPERM)?.unlink(name)
    }

    fn rmdir(&self, name: &str) -> Result<(), FS_ERRNO> {
        // Refuse to remove a live virtual mount point directory.
        {
            let inner = self.inner.lock();
            if inner.mounts.contains_key(name) {
                return Err(FS_ERRNO::EBUSY);
            }
        }
        let overlay = {
            let inner = self.inner.lock();
            inner.overlay.clone()
        };
        overlay.ok_or(FS_ERRNO::EPERM)?.rmdir(name)
    }

    fn atime(&self) -> Option<InodeTime> {
        let overlay = {
            let inner = self.inner.lock();
            inner.overlay.clone()
        };
        overlay.and_then(|ov| ov.atime())
    }

    fn mtime(&self) -> Option<InodeTime> {
        let overlay = {
            let inner = self.inner.lock();
            inner.overlay.clone()
        };
        overlay.and_then(|ov| ov.mtime())
    }

    fn ctime(&self) -> Option<InodeTime> {
        let overlay = {
            let inner = self.inner.lock();
            inner.overlay.clone()
        };
        overlay.and_then(|ov| ov.ctime())
    }

    fn set_times(
        &self,
        atime: Option<InodeTime>,
        mtime: Option<InodeTime>,
        ctime: Option<InodeTime>,
    ) -> Result<(), FS_ERRNO> {
        let overlay = {
            let inner = self.inner.lock();
            inner.overlay.clone()
        };
        overlay
            .ok_or(FS_ERRNO::EPERM)?
            .set_times(atime, mtime, ctime)
    }

    fn rename_child(
        &self,
        old_name: &str,
        new_parent: &Arc<dyn VfsNode>,
        new_name: &str,
    ) -> Result<(), FS_ERRNO> {
        if self.has_mount(old_name) {
            return Err(FS_ERRNO::EBUSY);
        }

        let overlay = self.overlay_node().ok_or(FS_ERRNO::EPERM)?;

        if let Some(new_parent_vdir) = new_parent.as_any().downcast_ref::<Self>() {
            if new_parent_vdir.has_mount(new_name) {
                return Err(FS_ERRNO::EBUSY);
            }
            let new_overlay = new_parent_vdir.overlay_node().ok_or(FS_ERRNO::EPERM)?;
            overlay.rename_child(old_name, &new_overlay, new_name)
        } else {
            overlay.rename_child(old_name, new_parent, new_name)
        }
    }
}

// ---------------------------------------------------------------------------
// Global virtual root
// ---------------------------------------------------------------------------

lazy_static! {
    /// The kernel's virtual root directory.
    ///
    /// Initially empty; [`super::inode::init_rootfs`] installs a real
    /// filesystem as the overlay at boot time.
    pub static ref VIRT_ROOT: Arc<VirtualDirNode> = VirtualDirNode::new();
}
