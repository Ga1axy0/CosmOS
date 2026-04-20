use alloc::{string::String, sync::Arc, vec::Vec};
use core::any::Any;
use spin::Mutex;

use crate::errno::FS_ERRNO;
use crate::inode_cache::get_or_create_inode;

/// Linux-style inode timestamp snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InodeTime {
    /// Seconds since Unix epoch.
    pub sec: u64,
    /// Nanoseconds within the second.
    pub nsec: u32,
}

impl InodeTime {
    /// Build an inode timestamp from second + nanosecond parts.
    pub const fn new(sec: u64, nsec: u32) -> Self {
        Self { sec, nsec }
    }
}

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
    /// 调整常规文件逻辑长度。
    fn truncate(&self, _new_size: usize) -> Result<(), FS_ERRNO> {
        Err(FS_ERRNO::EOPNOTSUPP)
    }
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize;
    fn write_at(&self, offset: usize, buf: &[u8]) -> usize;
    /// Stable inode number for stat-like metadata.
    fn ino(&self) -> u64 {
        0
    }
    /// 底层文件系统实例的稳定标识；返回 0 表示当前节点不参与 inode cache 复用。
    fn fs_id(&self) -> u64 {
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

    /// File mode bits for stat-like metadata, including permission and type bits.
    fn mode(&self) -> Option<u32> {
        None
    }

    /// Set file mode bits. This is used by `chmod` and `mkdir` syscalls to set permissions and type bits.
    fn set_mode(&self, _mode: u32) -> Result<(), FS_ERRNO> {
        Err(FS_ERRNO::EOPNOTSUPP)
    }

    /// Check whether `(uid, gid)` can access this inode with `mode` (`F_OK/R_OK/W_OK/X_OK`).
    ///
    /// Default implementation is permissive for backends that have not implemented
    /// Unix permission checks yet.
    fn check_access(&self, _uid: u32, _gid: u32, _mode: u32) -> bool {
        true
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
    /// Last access timestamp.
    fn atime(&self) -> Option<InodeTime> {
        None
    }

    /// Last modification timestamp.
    fn mtime(&self) -> Option<InodeTime> {
        None
    }

    /// Last metadata-change timestamp.
    fn ctime(&self) -> Option<InodeTime> {
        None
    }

    /// Update inode timestamps.
    fn set_times(
        &self,
        _atime: Option<InodeTime>,
        _mtime: Option<InodeTime>,
        _ctime: Option<InodeTime>,
    ) -> Result<(), FS_ERRNO> {
        Err(FS_ERRNO::EOPNOTSUPP)
    }

    /// Update `atime`/`mtime`/`ctime` to the same timestamp.
    fn set_times_now(&self, now: InodeTime) -> Result<(), FS_ERRNO> {
        self.set_times(Some(now), Some(now), Some(now))
    }

    /// Rename or move a child entry from this directory to `new_parent/new_name`.
    fn rename_child(
        &self,
        _old_name: &str,
        _new_parent: &Arc<dyn VfsNode>,
        _new_name: &str,
    ) -> Result<(), FS_ERRNO> {
        Err(FS_ERRNO::EOPNOTSUPP)
    }
}

pub struct Inode {
    inner: Arc<dyn VfsNode>,
    state: Mutex<InodeState>,
}

/// 稳定内存 inode 的可变运行时状态。
struct InodeState {
    /// 由 OS 层按需挂载的 page cache 宿主对象。
    page_cache: Option<Arc<dyn Any + Send + Sync>>,
}

impl Inode {
    /// 创建一个未进入 inode cache 的临时内存 inode。
    fn new(inner: Arc<dyn VfsNode>) -> Self {
        Self {
            inner,
            state: Mutex::new(InodeState { page_cache: None }),
        }
    }

    /// 创建一个不参与全局去重的 `Arc<Inode>`。
    pub(crate) fn new_uncached(inner: Arc<dyn VfsNode>) -> Arc<Self> {
        Arc::new(Self::new(inner))
    }

    /// 从底层 VFS 节点构造稳定内存 inode，对外统一走 inode cache。
    pub fn from_vfs_node(inner: Arc<dyn VfsNode>) -> Arc<Self> {
        get_or_create_inode(inner)
    }

    /// 将底层 VFS 节点包装为稳定内存 inode。
    fn wrap(node: Arc<dyn VfsNode>) -> Arc<Inode> {
        Self::from_vfs_node(node)
    }

    pub fn ls(&self) -> Vec<String> {
        self.inner.ls()
    }

    pub fn find(&self, name: &str) -> Option<Arc<Inode>> {
        self.inner.find(name).map(Self::wrap)
    }

    pub fn create(&self, name: &str) -> Option<Arc<Inode>> {
        self.inner.create(name).map(|i| {
            if let Some(cur_mode) = i.mode() {
                let perms_mask: u32 = 0x0fff; // lower 12 bits
                let new_mode = (cur_mode & !perms_mask) | (0o644u32 & perms_mask);
                let _ = i.set_mode(new_mode);
            }
            Self::wrap(i)
        })
    }

    pub fn mkdir(&self, name: &str) -> Option<Arc<Inode>> {
        self.inner.mkdir(name).map(|i|{
            if let Some(cur_mode) = i.mode() {
                let perms_mask: u32 = 0x0fff; // lower 12 bits
                let new_mode = (cur_mode & !perms_mask) | (0o755u32 & perms_mask);
                let _ = i.set_mode(new_mode);
            }
            Self::wrap(i)
        })
    }

    pub fn is_dir(&self) -> bool {
        self.inner.is_dir()
    }

    pub fn clear(&self) {
        self.inner.clear()
    }

    /// 调整 inode 对应常规文件的逻辑长度。
    pub fn truncate(&self, new_size: usize) -> Result<(), FS_ERRNO> {
        self.inner.truncate(new_size)
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

    /// 返回底层文件系统实例标识。
    pub fn fs_id(&self) -> u64 {
        self.inner.fs_id()
    }

    pub fn nlink(&self) -> u32 {
        self.inner.nlink()
    }

    pub fn size(&self) -> usize {
        self.inner.size()
    }

    pub fn mode(&self) -> Option<u32> {
        self.inner.mode()
    }

    /// Set file mode bits on the underlying node.
    pub fn set_mode(&self, mode: u32) -> Result<(), FS_ERRNO> {
        self.inner.set_mode(mode)
    }

    pub fn check_access(&self, uid: u32, gid: u32, mode: u32) -> bool {
        self.inner.check_access(uid, gid, mode)
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

    pub fn atime(&self) -> Option<InodeTime> {
        self.inner.atime()
    }

    pub fn mtime(&self) -> Option<InodeTime> {
        self.inner.mtime()
    }

    pub fn ctime(&self) -> Option<InodeTime> {
        self.inner.ctime()
    }

    pub fn set_times(
        &self,
        atime: Option<InodeTime>,
        mtime: Option<InodeTime>,
        ctime: Option<InodeTime>,
    ) -> Result<(), FS_ERRNO> {
        self.inner.set_times(atime, mtime, ctime)
    }

    /// Update `atime`/`mtime`/`ctime` to the same timestamp.
    pub fn set_times_now(&self, now: InodeTime) -> Result<(), FS_ERRNO> {
        self.inner.set_times_now(now)
    }

    pub fn rename_child(&self, old_name: &str, new_parent: &Inode, new_name: &str) -> Result<(), FS_ERRNO> {
        self.inner.rename_child(old_name, &new_parent.inner, new_name)
    }

    /// 获取当前 inode 挂载的 page cache 宿主对象。
    pub fn page_cache_state<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.state
            .lock()
            .page_cache
            .as_ref()
            .and_then(|state| Arc::clone(state).downcast::<T>().ok())
    }

    /// 为当前 inode 安装 page cache 宿主对象。
    pub fn set_page_cache_state<T: Any + Send + Sync>(&self, state: Arc<T>) {
        self.state.lock().page_cache = Some(state);
    }

    /// 原子地获取或安装当前 inode 挂载的 page cache 宿主对象。
    pub fn get_or_insert_page_cache_state<T, F>(&self, init: F) -> (Arc<T>, bool)
    where
        T: Any + Send + Sync,
        F: FnOnce() -> Arc<T>,
    {
        let mut state_guard = self.state.lock();
        if let Some(existing) = state_guard
            .page_cache
            .as_ref()
            .and_then(|state| Arc::clone(state).downcast::<T>().ok())
        {
            return (existing, false);
        }
        let state = init();
        let erased: Arc<dyn Any + Send + Sync> = state.clone();
        state_guard.page_cache = Some(erased);
        (state, true)
    }

    /// 移除当前 inode 挂载的 page cache 宿主对象。
    pub fn take_page_cache_state<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.state
            .lock()
            .page_cache
            .take()
            .and_then(|state| state.downcast::<T>().ok())
    }

    /// Returns a clone of the raw [`VfsNode`] backing this inode.
    ///
    /// Used by the kernel's virtual-rootfs layer to obtain the concrete node
    /// (e.g. an ext4 root) so it can be stored as a mount-point overlay.
    pub fn vfs_node(&self) -> Arc<dyn VfsNode> {
        Arc::clone(&self.inner)
    }
}
