use alloc::{string::String, sync::Arc, vec::Vec};

/// Common VFS node interface.
///
/// The kernel keeps `Arc<Inode>` handles and uses these methods for file operations.
/// Implementations can be backed by different on-disk filesystems (EasyFS, FAT32, ...).
pub trait VfsNode: Send + Sync {
    fn ls(&self) -> Vec<String>;
    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>>;
    fn create(&self, name: &str) -> Option<Arc<dyn VfsNode>>;
    fn clear(&self);
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize;
    fn write_at(&self, offset: usize, buf: &[u8]) -> usize;
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

    pub fn clear(&self) {
        self.inner.clear()
    }

    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        self.inner.read_at(offset, buf)
    }

    pub fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        self.inner.write_at(offset, buf)
    }
}
