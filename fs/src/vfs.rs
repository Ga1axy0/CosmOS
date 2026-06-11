use alloc::{string::String, sync::Arc, vec::Vec};
use core::fmt::Debug;
use log::warn;
use core::any::Any;
use spin::Mutex;

use crate::dentry_cache::{insert_dentry, lookup_dentry, remove_dentry};
use crate::errno::FS_ERRNO;
use crate::inode_cache::{get_or_create_inode, remove_cached_inode, remove_cached_node};

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

/// Batched inode attribute snapshot — read in one call to avoid repeated
/// lock acquisitions in the backend.
#[derive(Clone, Debug)]
pub struct VfsAttrs {
    pub mode: Option<u32>,
    pub ino: u64,
    pub nlink: u32,
    pub size: usize,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub rdev: u64,
    pub atime: Option<InodeTime>,
    pub mtime: Option<InodeTime>,
    pub ctime: Option<InodeTime>,
}

/// Filesystem statistics snapshot shared by VFS backends.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VfsStatFs {
    pub f_type: u64,
    pub f_bsize: u64,
    pub f_blocks: u64,
    pub f_bfree: u64,
    pub f_bavail: u64,
    pub f_files: u64,
    pub f_ffree: u64,
    pub f_fsid: [i32; 2],
    pub f_namelen: u64,
    pub f_frsize: u64,
    pub f_flags: u64,
    pub f_spare: [u64; 4],
}

/// Linux magic numbers used by the in-tree filesystem backends.
pub const STATFS_MAGIC_EASYFS: u64 = 0x0041_4A53;
pub const STATFS_MAGIC_EXT4: u64 = 0x0000_EF53;
pub const STATFS_MAGIC_MSDOS: u64 = 0x0000_4D44;
pub const STATFS_MAGIC_PROC: u64 = 0x0000_9FA0;
pub const STATFS_MAGIC_TMPFS: u64 = 0x0102_1994;
pub const STATFS_MAGIC_PIPEFS: u64 = 0x5049_5045;

/// Generic filename-length defaults used by the in-tree filesystems.
pub const STATFS_NAMELEN_DEFAULT: u64 = 255;
pub const STATFS_NAMELEN_EASYFS: u64 = 27;

/// VFS-visible inode file type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VfsFileType {
    Regular,
    Directory,
    Symlink,
    Char,
    Block,
    Fifo,
    Socket,
    Unknown,
}

/// Common VFS node interface.
///
/// The kernel keeps `Arc<Inode>` handles and uses these methods for file operations.
/// Implementations can be backed by different on-disk filesystems (EasyFS, FAT32, ext4, ...).
pub trait VfsNode: Send + Sync + Any + Debug {
    fn as_any(&self) -> &dyn Any;
    /// List directory entries as `(name, file_type)` pairs.
    fn ls(&self) -> Vec<(String, VfsFileType)>;
    /// Fill `buf` with `linux_dirent64` records starting from the backend-defined
    /// directory position `offset`.
    ///
    /// The default implementation preserves the historical behavior used by the
    /// in-tree backends: `offset` is treated as an entry index and the method
    /// is implemented on top of `ls()`.
    fn getdents64(&self, offset: usize, buf: &mut [u8]) -> usize {
        let entries = self.ls();
        let mut written = 0usize;

        for (i, (name, file_type)) in entries.iter().enumerate().skip(offset) {
            let name_bytes = name.as_bytes();
            let reclen = (19 + name_bytes.len() + 1 + 7) & !7usize;
            if written + reclen > buf.len() {
                break;
            }

            buf[written..written + 8].copy_from_slice(&((i + 1) as u64).to_le_bytes());
            let next_off = (i + 1) as i64;
            buf[written + 8..written + 16].copy_from_slice(&next_off.to_le_bytes());
            buf[written + 16..written + 18].copy_from_slice(&(reclen as u16).to_le_bytes());
            buf[written + 18] = match file_type {
                VfsFileType::Directory => 4,
                VfsFileType::Symlink => 10,
                VfsFileType::Char => 2,
                VfsFileType::Block => 6,
                VfsFileType::Fifo => 1,
                VfsFileType::Socket => 12,
                VfsFileType::Regular => 8,
                VfsFileType::Unknown => 0,
            };
            buf[written + 19..written + 19 + name_bytes.len()].copy_from_slice(name_bytes);
            buf[written + 19 + name_bytes.len()] = 0;
            for b in &mut buf[written + 19 + name_bytes.len() + 1..written + reclen] {
                *b = 0;
            }
            written += reclen;
        }

        written
    }
    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>>;
    fn create(&self, name: &str) -> Option<Arc<dyn VfsNode>>;
    /// Create a sub-directory named `name` inside this directory.
    /// Returns the new directory inode, or `None` on failure.
    fn mkdir(&self, name: &str) -> Option<Arc<dyn VfsNode>>;
    /// Returns true if this node is a directory.
    fn file_type(&self) -> VfsFileType;
    fn is_dir(&self) -> bool {
        self.file_type() == VfsFileType::Directory
    }
    fn is_symlink(&self) -> bool {
        self.file_type() == VfsFileType::Symlink
    }
    fn read_link(&self) -> Result<String, FS_ERRNO> {
        Err(FS_ERRNO::EINVAL)
    }
    fn symlink(&self, _name: &str, _target: &str) -> Result<Arc<dyn VfsNode>, FS_ERRNO> {
        Err(FS_ERRNO::EOPNOTSUPP)
    }
    fn clear(&self);
    /// 调整常规文件逻辑长度。
    fn truncate(&self, _new_size: usize) -> Result<(), FS_ERRNO> {
        Err(FS_ERRNO::EOPNOTSUPP)
    }
    /// Reserve or deallocate file space without forcing eager data materialisation.
    fn fallocate(&self, _mode: i32, _offset: usize, _len: usize) -> Result<(), FS_ERRNO> {
        Err(FS_ERRNO::EOPNOTSUPP)
    }
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize;
    fn write_at(&self, offset: usize, buf: &[u8]) -> usize;
    /// 向固定偏移写入数据，并保留底层文件系统返回的真实错误。
    fn write_at_result(&self, offset: usize, buf: &[u8]) -> Result<usize, FS_ERRNO> {
        Ok(self.write_at(offset, buf))
    }
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

    /// Device number (major/minor) for char/block device nodes.
    fn rdev(&self) -> u64 {
        0
    }

    /// File owner uid for stat-like metadata.
    fn uid(&self) -> Option<u32> {
        None
    }

    /// File owner gid for stat-like metadata.
    fn gid(&self) -> Option<u32> {
        None
    }

    /// Set file mode bits. This is used by `chmod` and `mkdir` syscalls to set permissions and type bits.
    fn set_mode(&self, _mode: u32) -> Result<(), FS_ERRNO> {
        Err(FS_ERRNO::EOPNOTSUPP)
    }

    /// Set file owner uid/gid.
    fn set_owner(&self, _uid: u32, _gid: u32) -> Result<(), FS_ERRNO> {
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

    /// Read all stat-relevant attributes in one shot.
    ///
    /// The default implementation calls individual getters; backends should
    /// override this to batch the reads under a single lock acquisition.
    fn stat_attrs(&self) -> VfsAttrs {
        VfsAttrs {
            mode: self.mode(),
            ino: self.ino(),
            nlink: self.nlink(),
            size: self.size(),
            uid: self.uid(),
            gid: self.gid(),
            rdev: self.rdev(),
            atime: self.atime(),
            mtime: self.mtime(),
            ctime: self.ctime(),
        }
    }

    /// Read filesystem-wide statistics.
    fn statfs(&self) -> Result<VfsStatFs, FS_ERRNO> {
        Err(FS_ERRNO::ENOSYS)
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
    /// 最近一次读取到的 stat 元数据快照。
    stat_attrs: Option<VfsAttrs>,
}

impl Inode {
    /// 创建一个未进入 inode cache 的临时内存 inode。
    fn new(inner: Arc<dyn VfsNode>) -> Self {
        Self {
            inner,
            state: Mutex::new(InodeState {
                page_cache: None,
                stat_attrs: None,
            }),
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

    pub fn ls(&self) -> Vec<(String, VfsFileType)> {
        self.inner.ls()
    }

    pub fn getdents64(&self, offset: usize, buf: &mut [u8]) -> usize {
        self.inner.getdents64(offset, buf)
    }

    pub fn find(&self, name: &str) -> Option<Arc<Inode>> {
        let fs_id = self.fs_id();
        if fs_id != 0 {
            if let Some(child) = lookup_dentry(fs_id, self.ino(), name) {
                return Some(child);
            }
        }
        let child = self.inner.find(name).map(Self::wrap)?;
        if fs_id != 0 {
            insert_dentry(fs_id, self.ino(), name, &child);
        }
        Some(child)
    }

    pub fn create(&self, name: &str) -> Option<Arc<Inode>> {
        let child = self.inner.create(name).map(|i| {
            if let Some(cur_mode) = i.mode() {
                let perms_mask: u32 = 0x0fff; // lower 12 bits
                let new_mode = (cur_mode & !perms_mask) | (0o644u32 & perms_mask);
                let _ = i.set_mode(new_mode);
            }
            remove_cached_node(i.as_ref());
            Self::wrap(i)
        })?;
        let fs_id = self.fs_id();
        if fs_id != 0 {
            insert_dentry(fs_id, self.ino(), name, &child);
        }
        Some(child)
    }

    pub fn mkdir(&self, name: &str) -> Option<Arc<Inode>> {
        let child = self.inner.mkdir(name).map(|i|{
            if let Some(cur_mode) = i.mode() {
                let perms_mask: u32 = 0x0fff; // lower 12 bits
                let new_mode = (cur_mode & !perms_mask) | (0o755u32 & perms_mask);
                let _ = i.set_mode(new_mode);
            }
            remove_cached_node(i.as_ref());
            Self::wrap(i)
        })?;
        let fs_id = self.fs_id();
        if fs_id != 0 {
            insert_dentry(fs_id, self.ino(), name, &child);
        }
        Some(child)
    }

    pub fn is_dir(&self) -> bool {
        self.inner.is_dir()
    }

    pub fn file_type(&self) -> VfsFileType {
        self.inner.file_type()
    }

    pub fn is_symlink(&self) -> bool {
        self.inner.is_symlink()
    }

    pub fn read_link(&self) -> Result<String, FS_ERRNO> {
        self.inner.read_link()
    }

    pub fn symlink(&self, name: &str, target: &str) -> Result<Arc<Inode>, FS_ERRNO> {
        let node = self.inner.symlink(name, target)?;
        remove_cached_node(node.as_ref());
        let child = Self::wrap(node);
        let fs_id = self.fs_id();
        if fs_id != 0 {
            insert_dentry(fs_id, self.ino(), name, &child);
        }
        Ok(child)
    }

    pub fn clear(&self) {
        self.inner.clear()
    }

    /// 调整 inode 对应常规文件的逻辑长度。
    pub fn truncate(&self, new_size: usize) -> Result<(), FS_ERRNO> {
        self.inner.truncate(new_size)?;
        self.invalidate_stat_cache();
        Ok(())
    }

    /// Reserve or deallocate file space.
    pub fn fallocate(&self, mode: i32, offset: usize, len: usize) -> Result<(), FS_ERRNO> {
        self.inner.fallocate(mode, offset, len)?;
        self.invalidate_stat_cache();
        Ok(())
    }

    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        self.inner.read_at(offset, buf)
    }

    pub fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let written = self.inner.write_at(offset, buf);
        if written != 0 {
            self.invalidate_stat_cache();
        }
        written
    }

    /// 向固定偏移写入数据，并把底层错误传给调用方。
    pub fn write_at_result(&self, offset: usize, buf: &[u8]) -> Result<usize, FS_ERRNO> {
        let written = self.inner.write_at_result(offset, buf).map_err(|err| {
            log::error!(
                "[vfs] write_at_result failed: ino={} offset={} len={} errno={}",
                self.ino(),
                offset,
                buf.len(),
                err as i32
            );
            err
        })?;
        if written != 0 {
            self.invalidate_stat_cache();
        }
        Ok(written)
    }

    pub fn ino(&self) -> u64 {
        self.inner.ino()
    }

    /// 返回底层文件系统实例标识。
    pub fn fs_id(&self) -> u64 {
        self.inner.fs_id()
    }

    /// Read all stat-relevant attributes in one call.
    pub fn stat_attrs(&self) -> VfsAttrs {
        if let Some(attrs) = self.state.lock().stat_attrs.clone() {
            return attrs;
        }
        let attrs = self.inner.stat_attrs();
        self.state.lock().stat_attrs = Some(attrs.clone());
        attrs
    }

    /// Read filesystem-wide statistics.
    pub fn statfs(&self) -> Result<VfsStatFs, FS_ERRNO> {
        self.inner.statfs()
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

    pub fn uid(&self) -> Option<u32> {
        self.inner.uid()
    }

    pub fn gid(&self) -> Option<u32> {
        self.inner.gid()
    }

    /// Set file mode bits on the underlying node.
    pub fn set_mode(&self, mode: u32) -> Result<(), FS_ERRNO> {
        self.inner.set_mode(mode)?;
        self.invalidate_stat_cache();
        Ok(())
    }

    /// Set file owner uid/gid on the underlying node.
    pub fn set_owner(&self, uid: u32, gid: u32) -> Result<(), FS_ERRNO> {
        self.inner.set_owner(uid, gid)?;
        self.invalidate_stat_cache();
        Ok(())
    }

    pub fn check_access(&self, uid: u32, gid: u32, mode: u32) -> bool {
        self.inner.check_access(uid, gid, mode)
    }

    pub fn link(&self, old_name: &str, new_name: &str) -> Result<(), FS_ERRNO> {
        self.inner.link(old_name, new_name)
    }

    pub fn link_inode(&self, child: &Arc<Inode>, new_name: &str) -> Result<(), FS_ERRNO> {
        self.inner.link_inode(&child.inner, new_name)?;
        self.invalidate_stat_cache();
        child.invalidate_stat_cache();
        let fs_id = self.fs_id();
        if fs_id != 0 {
            insert_dentry(fs_id, self.ino(), new_name, child);
        }
        Ok(())
    }

    pub fn unlink(&self, name: &str) -> Result<(), FS_ERRNO> {
        let child_to_drop = self
            .find(name)
            .filter(|child| child.nlink() <= 1)
            .map(|child| (child.fs_id(), child.ino()));
        let fs_id = self.fs_id();
        if let Err(err) = self.inner.unlink(name) {
            if matches!(err, FS_ERRNO::ENOENT) && fs_id != 0 {
                remove_dentry(fs_id, self.ino(), name);
            }
            return Err(err);
        }
        if fs_id != 0 {
            remove_dentry(fs_id, self.ino(), name);
        }
        if let Some((child_fs, child_ino)) = child_to_drop {
            remove_cached_inode(child_fs, child_ino);
        }
        self.invalidate_stat_cache();
        Ok(())
    }

    pub fn rmdir(&self, name: &str) -> Result<(), FS_ERRNO> {
        let child_to_drop = self.find(name).map(|child| (child.fs_id(), child.ino()));
        let fs_id = self.fs_id();
        if let Err(err) = self.inner.rmdir(name) {
            if matches!(err, FS_ERRNO::ENOENT) && fs_id != 0 {
                remove_dentry(fs_id, self.ino(), name);
            }
            return Err(err);
        }
        if fs_id != 0 {
            remove_dentry(fs_id, self.ino(), name);
        }
        if let Some((child_fs, child_ino)) = child_to_drop {
            remove_cached_inode(child_fs, child_ino);
        }
        self.invalidate_stat_cache();
        Ok(())
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
        self.inner.set_times(atime, mtime, ctime)?;
        self.invalidate_stat_cache();
        Ok(())
    }

    /// Update `atime`/`mtime`/`ctime` to the same timestamp.
    pub fn set_times_now(&self, now: InodeTime) -> Result<(), FS_ERRNO> {
        self.inner.set_times_now(now)?;
        self.invalidate_stat_cache();
        Ok(())
    }

    pub fn rename_child(&self, old_name: &str, new_parent: &Inode, new_name: &str) -> Result<(), FS_ERRNO> {
        let old_child = self.find(old_name).map(|child| (child.fs_id(), child.ino()));
        let replaced_child = new_parent
            .find(new_name)
            .filter(|child| {
                let child_key = (child.fs_id(), child.ino());
                Some(child_key) != old_child && (child.is_dir() || child.nlink() <= 1)
            })
            .map(|child| (child.fs_id(), child.ino()));
        self.inner.rename_child(old_name, &new_parent.inner, new_name)?;
        let old_fs = self.fs_id();
        if old_fs != 0 {
            remove_dentry(old_fs, self.ino(), old_name);
        }
        let new_fs = new_parent.fs_id();
        if new_fs != 0 {
            remove_dentry(new_fs, new_parent.ino(), new_name);
        }
        if let Some((child_fs, child_ino)) = replaced_child {
            remove_cached_inode(child_fs, child_ino);
        }
        self.invalidate_stat_cache();
        new_parent.invalidate_stat_cache();
        Ok(())
    }

    /// Drop the cached stat snapshot for this inode.
    pub fn invalidate_stat_cache(&self) {
        self.state.lock().stat_attrs = None;
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
