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
use fs::vfs::{InodeTime, VfsNode};
use lazy_static::*;

use crate::sync::{SpinNoIrqLock};

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
    }

    /// Remove the explicit binding for `name`.
    ///
    /// Returns `true` if the binding existed and was removed.
    pub fn unbind(&self, name: &str) -> bool {
        self.inner.lock().mounts.remove(name).is_some()
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

// ---------------------------------------------------------------------------
// VfsNode implementation
// ---------------------------------------------------------------------------

impl VfsNode for VirtualDirNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_dir(&self) -> bool {
        true
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

    fn ls(&self) -> Vec<(String, bool)> {
        // Phase 1: collect (name, is_dir) from overlay.
        let mut entries: Vec<(String, bool)> = {
            let inner = self.inner.lock();
            match inner.overlay.as_ref() {
                Some(ov) => ov.ls(),
                None => Vec::new(),
            }
        };

        // Phase 2: add explicit mount names that the overlay doesn't list.
        let mount_entries: Vec<(String, bool)> = {
            let inner = self.inner.lock();
            inner
                .mounts
                .iter()
                .map(|(name, node)| (name.clone(), node.is_dir()))
                .collect()
        };

        for (key, is_dir) in mount_entries {
            if !entries.iter().any(|(name, _)| name == &key) {
                entries.push((key, is_dir));
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
