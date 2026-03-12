use alloc::{string::String, sync::Arc, vec::Vec};
use core::any::Any;

use crate::errno::FS_ERRNO;
/// Common VFS node interface.
///
/// The kernel keeps `Arc<Inode>` handles and uses these methods for file operations.
/// Implementations can be backed by different on-disk filesystems (EasyFS, FAT32, ext4, ...).
pub trait VfsNode: Send + Sync + Any {
    fn as_any(&self) -> &dyn Any;
    fn ls(&self) -> Vec<String>;
    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>>;
    fn create(&self, name: &str) -> Option<Arc<dyn VfsNode>>;
    /// Create a sub-directory named `name` inside this directory.
    /// Returns the new directory inode, or `None` on failure.
    fn mkdir(&self, name: &str) -> Option<Arc<dyn VfsNode>>;
    /// Returns true if this node is a directory.
    fn is_dir(&self) -> bool;
    fn clear(&self);
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize;
    fn write_at(&self, offset: usize, buf: &[u8]) -> usize;
    /// Stable inode number for stat-like metadata.
    fn ino(&self) -> u64 {
        0
    }
    /// Hard-link count for stat-like metadata.
    fn nlink(&self) -> u32 {
        1
    }
    /// File size in bytes for stat-like metadata.
    fn size(&self) -> usize {
        0
    }

    /// Create a hard link in this directory.
    fn link(&self, _old_name: &str, _new_name: &str) -> Result<(), FS_ERRNO> {
        Err(FS_ERRNO::EACCES)
    }

    /// Create a hard link in this directory to an already-resolved inode.
    fn link_inode(&self, _child: &Arc<dyn VfsNode>, _new_name: &str) -> Result<(), FS_ERRNO> {
        Err(FS_ERRNO::EACCES)
    }

    /// Remove a name entry in this directory.
    fn unlink(&self, _name: &str) -> Result<(), FS_ERRNO> {
        Err(FS_ERRNO::EACCES)
    }

    /// Remove an empty sub-directory in this directory.
    fn rmdir(&self, _name: &str) -> Result<(), FS_ERRNO> {
        Err(FS_ERRNO::EACCES)
    }
}

pub struct Inode {
    inner: Arc<dyn VfsNode>,
}

impl Inode {
    pub fn new(inner: Arc<dyn VfsNode>) -> Self {
        Self { inner }
    }

    fn wrap(node: Arc<dyn VfsNode>) -> Arc<Inode> {
        Arc::new(Self::new(node))
    }

    pub fn ls(&self) -> Vec<String> {
        self.inner.ls()
    }

    pub fn find(&self, name: &str) -> Option<Arc<Inode>> {
        self.inner.find(name).map(Self::wrap)
    }

    pub fn create(&self, name: &str) -> Option<Arc<Inode>> {
        self.inner.create(name).map(Self::wrap)
    }

    pub fn mkdir(&self, name: &str) -> Option<Arc<Inode>> {
        self.inner.mkdir(name).map(Self::wrap)
    }

    pub fn is_dir(&self) -> bool {
        self.inner.is_dir()
    }

    pub fn clear(&self) {
        self.inner.clear()
    }

    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        self.inner.read_at(offset, buf)
    }

    pub fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        self.inner.write_at(offset, buf)
    }

    pub fn ino(&self) -> u64 {
        self.inner.ino()
    }

    pub fn nlink(&self) -> u32 {
        self.inner.nlink()
    }

    pub fn size(&self) -> usize {
        self.inner.size()
    }

    pub fn link(&self, old_name: &str, new_name: &str) -> Result<(), FS_ERRNO> {
        self.inner.link(old_name, new_name)
    }

    pub fn link_inode(&self, child: &Inode, new_name: &str) -> Result<(), FS_ERRNO> {
        self.inner.link_inode(&child.inner, new_name)
    }

    pub fn unlink(&self, name: &str) -> Result<(), FS_ERRNO> {
        self.inner.unlink(name)
    }

    pub fn rmdir(&self, name: &str) -> Result<(), FS_ERRNO> {
        self.inner.rmdir(name)
    }
}
