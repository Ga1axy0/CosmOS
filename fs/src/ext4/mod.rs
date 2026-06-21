use alloc::{string::String, sync::Arc, vec, vec::Vec};
use core::any::Any;
use core::cmp::min;
use core::fmt;
#[cfg(feature = "io_perf_counters")]
use core::fmt::Write;
#[cfg(feature = "io_perf_counters")]
use core::sync::atomic::{AtomicUsize, Ordering};
use log::{debug, info};

use crate::block_cache::{
    get_block_cache, overwrite_block_cache_range, overwrite_block_cache_ranges,
};
use crate::block_dev::{BlockDevice as OsBlockDevice, BlockWrite as OsBlockWrite};
use crate::dentry_cache::insert_dentry;
use crate::errno::FS_ERRNO;
use crate::sleep_mutex::SleepMutex as Mutex;
use crate::{STATFS_MAGIC_EXT4, STATFS_NAMELEN_DEFAULT, VfsStatFs};
use crate::vfs::{Inode, InodeTime, VfsAttrs, VfsFileType, VfsNode};
use crate::BLOCK_SZ;

use ext4_rs::{
    BlockDevice as Ext4BlockDevice, BlockWrite as Ext4BlockWrite, Ext4, InodeFileType, BLOCK_SIZE,
};

/// Adapts the OS block-id based device into ext4_rs offset-based IO.
struct Ext4BlockDeviceAdapter {
    inner: Arc<dyn OsBlockDevice>,
}

#[cfg(feature = "io_perf_counters")]
static WRITE_OFFSETS_MANY_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_OFFSETS_MANY_ITEMS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_OFFSETS_MANY_SINGLE_ITEM_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_OFFSETS_MANY_ALIGNED_ITEMS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_OFFSETS_MANY_UNALIGNED_ITEMS: AtomicUsize = AtomicUsize::new(0);

const EXT4_ROOT_INODE: u32 = 2;

#[inline]
fn decode_ext4_time(sec_lo: u32, extra: u32) -> InodeTime {
    let sec_hi = (extra & 0x3) as u64;
    let nsec = extra >> 2;
    InodeTime::new((sec_lo as u64) | (sec_hi << 32), nsec)
}

#[inline]
fn encode_ext4_time(ts: InodeTime) -> (u32, u32) {
    let sec_lo = ts.sec as u32;
    let sec_hi = ((ts.sec >> 32) as u32) & 0x3;
    let nsec = ts.nsec & 0x3fff_ffff;
    (sec_lo, (nsec << 2) | sec_hi)
}

impl Ext4BlockDeviceAdapter {
    fn new(inner: Arc<dyn OsBlockDevice>) -> Self {
        Self { inner }
    }
}

impl Ext4BlockDevice for Ext4BlockDeviceAdapter {
    fn read_offset(&self, offset: usize) -> Vec<u8> {
        let len = ext4_rs::BLOCK_SIZE;
        let mut out = vec![0u8; len];

        let start_block = offset / BLOCK_SZ;
        let end_block = (offset + len).div_ceil(BLOCK_SZ);

        for block_id in start_block..end_block {
            let block_start = block_id * BLOCK_SZ;
            let src_start = offset.saturating_sub(block_start);
            let src_end = BLOCK_SZ.min(offset + len - block_start);
            if src_start >= src_end {
                continue;
            }

            let dst_start = block_start + src_start - offset;
            let copy_len = src_end - src_start;
            get_block_cache(block_id, Arc::clone(&self.inner))
                .lock()
                .read_bytes(src_start, &mut out[dst_start..dst_start + copy_len]);
        }

        out
    }

    fn write_offset(&self, offset: usize, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let mut written = 0usize;

        if offset % BLOCK_SZ != 0 {
            let block_id = offset / BLOCK_SZ;
            let dst_start = offset % BLOCK_SZ;
            let len = (BLOCK_SZ - dst_start).min(data.len());
            get_block_cache(block_id, Arc::clone(&self.inner))
                .lock()
                .write_bytes(dst_start, &data[..len]);
            written += len;
        }

        let aligned_len = (data.len() - written) / BLOCK_SZ * BLOCK_SZ;
        if aligned_len != 0 {
            let start_block = (offset + written) / BLOCK_SZ;
            overwrite_block_cache_range(
                start_block,
                Arc::clone(&self.inner),
                &data[written..written + aligned_len],
            );
            written += aligned_len;
        }

        if written < data.len() {
            let block_id = (offset + written) / BLOCK_SZ;
            get_block_cache(block_id, Arc::clone(&self.inner))
                .lock()
                .write_bytes(0, &data[written..]);
        }
    }

    fn write_offsets_many(&self, writes: &[Ext4BlockWrite<'_>]) {
        #[cfg(feature = "io_perf_counters")]
        {
            let non_empty = writes.iter().filter(|write| !write.data.is_empty()).count();
            if non_empty != 0 {
                WRITE_OFFSETS_MANY_CALLS.fetch_add(1, Ordering::Relaxed);
                WRITE_OFFSETS_MANY_ITEMS.fetch_add(non_empty, Ordering::Relaxed);
                if non_empty == 1 {
                    WRITE_OFFSETS_MANY_SINGLE_ITEM_CALLS.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        let mut pending: Vec<OsBlockWrite<'_>> = Vec::new();
        for write in writes {
            if write.data.is_empty() {
                continue;
            }
            if offset_and_len_are_block_aligned(write.offset, write.data.len()) {
                #[cfg(feature = "io_perf_counters")]
                WRITE_OFFSETS_MANY_ALIGNED_ITEMS.fetch_add(1, Ordering::Relaxed);
                pending.push(OsBlockWrite {
                    start_block: write.offset / BLOCK_SZ,
                    data: write.data,
                });
            } else {
                #[cfg(feature = "io_perf_counters")]
                WRITE_OFFSETS_MANY_UNALIGNED_ITEMS.fetch_add(1, Ordering::Relaxed);
                if !pending.is_empty() {
                    overwrite_block_cache_ranges(Arc::clone(&self.inner), &pending);
                    pending.clear();
                }
                self.write_offset(write.offset, write.data);
            }
        }
        if !pending.is_empty() {
            overwrite_block_cache_ranges(Arc::clone(&self.inner), &pending);
        }
    }
}

#[inline]
fn offset_and_len_are_block_aligned(offset: usize, len: usize) -> bool {
    offset % BLOCK_SZ == 0 && len % BLOCK_SZ == 0
}

#[cfg(feature = "io_perf_counters")]
fn perf_load(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::Relaxed)
}

#[cfg(feature = "io_perf_counters")]
pub fn reset_perf_counters() {
    WRITE_OFFSETS_MANY_CALLS.store(0, Ordering::Relaxed);
    WRITE_OFFSETS_MANY_ITEMS.store(0, Ordering::Relaxed);
    WRITE_OFFSETS_MANY_SINGLE_ITEM_CALLS.store(0, Ordering::Relaxed);
    WRITE_OFFSETS_MANY_ALIGNED_ITEMS.store(0, Ordering::Relaxed);
    WRITE_OFFSETS_MANY_UNALIGNED_ITEMS.store(0, Ordering::Relaxed);
}

#[cfg(feature = "io_perf_counters")]
pub fn render_perf_counters() -> String {
    let mut out = String::new();
    let _ = writeln!(&mut out, "ext4:");
    let _ = writeln!(
        &mut out,
        "  write_offsets_many_calls {}",
        perf_load(&WRITE_OFFSETS_MANY_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  write_offsets_many_items {}",
        perf_load(&WRITE_OFFSETS_MANY_ITEMS)
    );
    let _ = writeln!(
        &mut out,
        "  write_offsets_many_single_item_calls {}",
        perf_load(&WRITE_OFFSETS_MANY_SINGLE_ITEM_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  write_offsets_many_aligned_items {}",
        perf_load(&WRITE_OFFSETS_MANY_ALIGNED_ITEMS)
    );
    let _ = writeln!(
        &mut out,
        "  write_offsets_many_unaligned_items {}",
        perf_load(&WRITE_OFFSETS_MANY_UNALIGNED_ITEMS)
    );
    out
}

pub struct Ext4FileSystem {
    ext4: Mutex<Ext4>,
}

impl Ext4FileSystem {
    pub fn open(block_device: Arc<dyn OsBlockDevice>) -> Arc<Self> {
        let ext4_dev: Arc<dyn Ext4BlockDevice> = Arc::new(Ext4BlockDeviceAdapter::new(block_device));
        let ext4 = Ext4::open(ext4_dev);
        Arc::new(Self {
            ext4: Mutex::new(ext4),
        })
    }

    /// 返回根目录对应的稳定内存 inode。
    pub fn root_inode(fs: &Arc<Self>) -> Arc<Inode> {
        Inode::from_vfs_node(Arc::new(Ext4Inode::new(Arc::clone(fs), EXT4_ROOT_INODE, true)))
    }
}

pub struct Ext4Inode {
    fs: Arc<Ext4FileSystem>,
    inode_num: u32,
    file_type: VfsFileType,
}

impl Ext4Inode {
    /// 创建新的 ext4 内存 inode 包装对象。
    fn new(fs: Arc<Ext4FileSystem>, inode_num: u32, is_dir: bool) -> Self {
        Self {
            fs,
            inode_num,
            file_type: if is_dir { VfsFileType::Directory } else { VfsFileType::Regular },
        }
    }

    fn new_with_type(fs: Arc<Ext4FileSystem>, inode_num: u32, file_type: VfsFileType) -> Self {
        Self {
            fs,
            inode_num,
            file_type,
        }
    }

    fn inode_file_type(ext4: &Ext4, inode_num: u32) -> VfsFileType {
        let inode_ref = ext4.get_inode_ref(inode_num);
        match inode_ref.inode.file_type() {
            InodeFileType::S_IFDIR => VfsFileType::Directory,
            InodeFileType::S_IFLNK => VfsFileType::Symlink,
            InodeFileType::S_IFCHR => VfsFileType::Char,
            InodeFileType::S_IFBLK => VfsFileType::Block,
            InodeFileType::S_IFIFO => VfsFileType::Fifo,
            InodeFileType::S_IFSOCK => VfsFileType::Socket,
            InodeFileType::S_IFREG => VfsFileType::Regular,
            _ => VfsFileType::Unknown,
        }
    }

    fn dirent_file_type(de_type: u8) -> VfsFileType {
        match de_type {
            2 => VfsFileType::Directory,
            7 => VfsFileType::Symlink,
            3 => VfsFileType::Char,
            4 => VfsFileType::Block,
            5 => VfsFileType::Fifo,
            6 => VfsFileType::Socket,
            1 => VfsFileType::Regular,
            _ => VfsFileType::Unknown,
        }
    }

    fn linux_dirent_file_type(dtype: u8) -> VfsFileType {
        match dtype {
            4 => VfsFileType::Directory,
            10 => VfsFileType::Symlink,
            2 => VfsFileType::Char,
            6 => VfsFileType::Block,
            1 => VfsFileType::Fifo,
            12 => VfsFileType::Socket,
            8 => VfsFileType::Regular,
            _ => VfsFileType::Unknown,
        }
    }

    fn prime_dentry_cache_from_dirents(&self, ext4: &Ext4, buf: &[u8]) {
        let fs_id = self.fs_id();
        let parent_ino = self.ino();
        let mut cursor = 0usize;
        while cursor + 19 <= buf.len() {
            let reclen = u16::from_le_bytes([buf[cursor + 16], buf[cursor + 17]]) as usize;
            if reclen == 0 || cursor + reclen > buf.len() {
                break;
            }
            let name_end = buf[cursor + 19..cursor + reclen]
                .iter()
                .position(|&b| b == 0)
                .map(|idx| cursor + 19 + idx)
                .unwrap_or(cursor + reclen);
            if name_end == cursor + 19 {
                cursor += reclen;
                continue;
            }

            let ino = u64::from_le_bytes([
                buf[cursor],
                buf[cursor + 1],
                buf[cursor + 2],
                buf[cursor + 3],
                buf[cursor + 4],
                buf[cursor + 5],
                buf[cursor + 6],
                buf[cursor + 7],
            ]);
            let Ok(name) = core::str::from_utf8(&buf[cursor + 19..name_end]) else {
                cursor += reclen;
                continue;
            };
            if ino == 0 || name == "." || name == ".." {
                cursor += reclen;
                continue;
            }

            let mut file_type = Self::linux_dirent_file_type(buf[cursor + 18]);
            if file_type == VfsFileType::Unknown {
                file_type = Self::inode_file_type(ext4, ino as u32);
            }
            let child = Inode::from_vfs_node(
                Arc::new(Self::new_with_type(Arc::clone(&self.fs), ino as u32, file_type))
                    as Arc<dyn VfsNode>,
            );
            insert_dentry(fs_id, parent_ino, name, &child);
            cursor += reclen;
        }
    }

    fn ext4_getdents64(&self, offset: usize, buf: &mut [u8]) -> usize {
        if self.file_type != VfsFileType::Directory {
            return 0;
        }

        let ext4 = self.fs.ext4.lock();
        let written = ext4.ext4_dir_getdents64(self.inode_num, offset, buf);
        if written != 0 {
            self.prime_dentry_cache_from_dirents(&ext4, &buf[..written]);
        }
        written
    }

    /// 查询目录项元数据，返回 `(inode 编号, 文件类型)`。
    fn lookup_child_meta(&self, name: &str) -> Option<(u32, VfsFileType)> {
        if self.file_type != VfsFileType::Directory {
            return None;
        }
        let ext4 = self.fs.ext4.lock();
        ext4.ext4_dir_lookup(self.inode_num, name).map(|(inode_num, _de_type)| {
            // Trust the inode's mode bits over the directory entry type.
            // A stale/corrupt d_type can otherwise turn a non-directory inode
            // into a cached "directory" and later panic inside ext4 dir helpers.
            let file_type = Self::inode_file_type(&ext4, inode_num);
            (inode_num, file_type)
        })
    }

    /// 在 ext4 后端内实现完整 truncate 语义。
    fn truncate_file(&self, new_size: usize) -> Result<(), FS_ERRNO> {
        if self.file_type == VfsFileType::Directory {
            return Err(FS_ERRNO::EISDIR);
        }

        let ext4 = self.fs.ext4.lock();
        let old_size = ext4.get_inode_ref(self.inode_num).inode.size() as usize;
        debug!(
            "Ext4Inode truncate: ino={} old_size={} new_size={}",
            self.inode_num,
            old_size,
            new_size
        );
        if old_size == new_size {
            return Ok(());
        }

        let zero_block = [0u8; BLOCK_SIZE];

        if new_size < old_size {
            let tail_off = new_size % BLOCK_SIZE;
            if new_size > 0 && tail_off != 0 {
                let zero_len = BLOCK_SIZE - tail_off;
                // 先把保留尾块的新 EOF 之后部分清零，避免未来再次扩容时旧数据重新可见。
                debug!(
                    "Ext4Inode truncate shrink tail: ino={} zero_from={} zero_len={}",
                    self.inode_num,
                    new_size,
                    zero_len
                );
                ext4.write_at(self.inode_num, new_size, &zero_block[..zero_len])?;
            }

            let mut inode_ref = ext4.get_inode_ref(self.inode_num);
            let block_size = BLOCK_SIZE as u64;
            let new_blocks = (new_size as u64).div_ceil(block_size);
            let old_blocks = (old_size as u64).div_ceil(block_size);
            if old_blocks > new_blocks {
                debug!(
                    "Ext4Inode truncate shrink blocks: ino={} old_blocks={} new_blocks={}",
                    self.inode_num,
                    old_blocks,
                    new_blocks
                );
                ext4.extent_remove_space(&mut inode_ref, new_blocks as u32, u32::MAX)?;
            }
            inode_ref.inode.set_size(new_size as u64);
            ext4.write_back_inode(&mut inode_ref);
            return Ok(());
        }

        // TODO：当前扩容采用显式补零，语义完整但不是稀疏文件实现，后续可按需优化。
        let mut cursor = old_size;
        while cursor < new_size {
            let chunk_len = min(BLOCK_SIZE, new_size - cursor);
            debug!(
                "Ext4Inode truncate grow chunk: ino={} off={} len={}",
                self.inode_num,
                cursor,
                chunk_len
            );
            ext4.write_at(self.inode_num, cursor, &zero_block[..chunk_len])?;
            cursor += chunk_len;
        }

        let mut inode_ref = ext4.get_inode_ref(self.inode_num);
        inode_ref.inode.set_size(new_size as u64);
        ext4.write_back_inode(&mut inode_ref);
        Ok(())
    }

    /// 将目录项从当前目录重命名到新父目录。
    fn rename_child_to(&self, old_name: &str, new_parent: &Self, new_name: &str) -> Result<(), FS_ERRNO> {
        if self.file_type != VfsFileType::Directory || new_parent.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        if !Arc::ptr_eq(&self.fs, &new_parent.fs) {
            return Err(FS_ERRNO::EXDEV);
        }

        let ext4 = self.fs.ext4.lock();
        let old_entry = ext4
            .ext4_dir_get_entries(self.inode_num)
            .into_iter()
            .find(|de| de.get_name() == old_name)
            .ok_or(FS_ERRNO::ENOENT)?;
        let child_ino = old_entry.inode;
        let child_ref = ext4.get_inode_ref(child_ino);
        let child_is_dir = child_ref.inode.is_dir();

        if let Some(target_entry) = ext4
            .ext4_dir_get_entries(new_parent.inode_num)
            .into_iter()
            .find(|de| de.get_name() == new_name)
        {
            let target_ino = target_entry.inode;
            let target_ref = ext4.get_inode_ref(target_ino);
            let target_is_dir = target_ref.inode.is_dir();
            if child_ino == target_ino {
                return Ok(());
            }
            if child_is_dir && !target_is_dir {
                return Err(FS_ERRNO::ENOTDIR);
            }
            if !child_is_dir && target_is_dir {
                return Err(FS_ERRNO::EISDIR);
            }
            if target_is_dir && ext4.dir_has_entry(target_ino) {
                return Err(FS_ERRNO::ENOTEMPTY);
            }
        }

        ext4.rename_entry(self.inode_num, old_name, new_parent.inode_num, new_name)?;
        Ok(())
    }
}

impl fmt::Debug for Ext4Inode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ext4Inode")
            .field("inode_num", &self.inode_num)
            .field("file_type", &self.file_type)
            .field("fs_ptr", &format_args!("{:p}", Arc::as_ptr(&self.fs)))
            .finish()
    }
}

impl VfsNode for Ext4Inode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        if self.file_type != VfsFileType::Directory {
            return Vec::new();
        }
        let ext4 = self.fs.ext4.lock();
        ext4
            .ext4_dir_get_entries(self.inode_num)
            .into_iter()
            .map(|de| {
                let dirent_type = Self::dirent_file_type(de.get_de_type());
                let file_type = if dirent_type == VfsFileType::Unknown {
                    Self::inode_file_type(&ext4, de.inode)
                } else {
                    dirent_type
                };
                (de.get_name(), file_type)
            })
            .collect()
    }

    fn getdents64(&self, offset: usize, buf: &mut [u8]) -> usize {
        self.ext4_getdents64(offset, buf)
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let (inode_num, file_type) = self.lookup_child_meta(name)?;
        Some(Arc::new(Self::new_with_type(Arc::clone(&self.fs), inode_num, file_type)) as Arc<dyn VfsNode>)
    }

    fn create(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        if self.file_type != VfsFileType::Directory {
            return None;
        }
        let ext4 = self.fs.ext4.lock();
        let inode = ext4
            .create(self.inode_num, name, InodeFileType::S_IFREG.bits())
            .ok()?;
        Some(Arc::new(Self::new_with_type(Arc::clone(&self.fs), inode.inode_num, VfsFileType::Regular)) as Arc<dyn VfsNode>)
    }

    fn mkdir(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        if self.file_type != VfsFileType::Directory {
            return None;
        }
        let ext4 = self.fs.ext4.lock();
        let inode = ext4
            .create(self.inode_num, name, InodeFileType::S_IFDIR.bits())
            .ok()?;
        Some(Arc::new(Self::new_with_type(Arc::clone(&self.fs), inode.inode_num, VfsFileType::Directory)) as Arc<dyn VfsNode>)
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn VfsNode>, FS_ERRNO> {
        if self.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        let ext4 = self.fs.ext4.lock();
        let inode = ext4
            .create(self.inode_num, name, InodeFileType::S_IFLNK.bits() | 0o777)
            .map_err(FS_ERRNO::from)?;
        ext4.write_at(inode.inode_num, 0, target.as_bytes()).map_err(FS_ERRNO::from)?;
        Ok(Arc::new(Self::new_with_type(
            Arc::clone(&self.fs),
            inode.inode_num,
            VfsFileType::Symlink,
        )) as Arc<dyn VfsNode>)
    }

    fn file_type(&self) -> VfsFileType {
        self.file_type
    }

    fn read_link(&self) -> Result<String, FS_ERRNO> {
        if self.file_type != VfsFileType::Symlink {
            return Err(FS_ERRNO::EINVAL);
        }
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        let size = inode_ref.inode.size() as usize;

        // ext4 fast symlink stores the link target inline in inode.i_block.
        // Treating it as regular file data would wrongly parse extents and panic.
        if size <= core::mem::size_of_val(&inode_ref.inode.block) && inode_ref.inode.blocks_count() == 0 {
            let mut buf = Vec::with_capacity(core::mem::size_of_val(&inode_ref.inode.block));
            for word in inode_ref.inode.block() {
                buf.extend_from_slice(&word.to_le_bytes());
            }
            buf.truncate(size);
            return String::from_utf8(buf).map_err(|_| FS_ERRNO::EINVAL);
        }

        let mut buf = vec![0u8; size];
        let read = ext4.read_at(self.inode_num, 0, &mut buf).map_err(FS_ERRNO::from)?;
        buf.truncate(read);
        String::from_utf8(buf).map_err(|_| FS_ERRNO::EINVAL)
    }

    fn stat_attrs(&self) -> VfsAttrs {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        let i = &inode_ref.inode;
        VfsAttrs {
            mode: Some(i.mode() as u32),
            ino: self.inode_num as u64,
            nlink: i.links_count() as u32,
            size: i.size() as usize,
            uid: Some(i.uid() as u32),
            gid: Some(i.gid() as u32),
            rdev: 0,
            atime: Some(decode_ext4_time(i.atime(), i.i_atime_extra())),
            mtime: Some(decode_ext4_time(i.mtime(), i.i_mtime_extra())),
            ctime: Some(decode_ext4_time(i.ctime(), i.i_ctime_extra())),
        }
    }

    fn statfs(&self) -> Result<VfsStatFs, FS_ERRNO> {
        let ext4 = self.fs.ext4.lock();
        let sb = ext4.super_block;
        Ok(VfsStatFs {
            f_type: STATFS_MAGIC_EXT4,
            f_bsize: sb.block_size() as u64,
            f_blocks: sb.blocks_count() as u64,
            f_bfree: sb.free_blocks_count(),
            f_bavail: sb.free_blocks_count(),
            f_files: sb.total_inodes() as u64,
            f_ffree: sb.free_inodes_count() as u64,
            f_fsid: [
                (Arc::as_ptr(&self.fs) as usize as u32) as i32,
                ((Arc::as_ptr(&self.fs) as usize as u64 >> 32) as u32) as i32,
            ],
            f_namelen: STATFS_NAMELEN_DEFAULT,
            f_frsize: sb.block_size() as u64,
            f_flags: 0,
            f_spare: [0; 4],
        })
    }

    fn clear(&self) {
        let _ = self.truncate_file(0);
    }

    fn truncate(&self, new_size: usize) -> Result<(), FS_ERRNO> {
        self.truncate_file(new_size)
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let ext4 = self.fs.ext4.lock();
        ext4.read_at(self.inode_num, offset, buf).unwrap_or(0)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let ext4 = self.fs.ext4.lock();
        ext4.write_at(self.inode_num, offset, buf).unwrap_or(0)
    }

    /// ext4 写入需要保留 ENOSPC/ENOTSUP，供 page cache 回写路径处理失败页。
    fn write_at_result(&self, offset: usize, buf: &[u8]) -> Result<usize, FS_ERRNO> {
        let ext4 = self.fs.ext4.lock();
        ext4.write_at(self.inode_num, offset, buf).map_err(FS_ERRNO::from)
    }

    fn ino(&self) -> u64 {
        self.inode_num as u64
    }

    fn fs_id(&self) -> u64 {
        Arc::as_ptr(&self.fs) as usize as u64
    }

    fn nlink(&self) -> u32 {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        inode_ref.inode.links_count() as u32
    }

    fn size(&self) -> usize {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        inode_ref.inode.size() as usize
    }

    fn mode(&self) -> Option<u32> {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        Some(inode_ref.inode.mode() as u32)
    }

    fn uid(&self) -> Option<u32> {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        Some(inode_ref.inode.uid() as u32)
    }

    fn gid(&self) -> Option<u32> {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        Some(inode_ref.inode.gid() as u32)
    }

    fn set_mode(&self, mode: u32) -> Result<(), FS_ERRNO> {
        info!("Ext4Inode set_mode: ino={} mode={:#o}", self.inode_num, mode);
        let ext4 = self.fs.ext4.lock();
        let mut inode_ref = ext4.get_inode_ref(self.inode_num);
        inode_ref.inode.set_mode(mode as u16);
        ext4.write_back_inode(&mut inode_ref);
        Ok(())
    }

    fn set_owner(&self, uid: u32, gid: u32) -> Result<(), FS_ERRNO> {
        if uid > u16::MAX as u32 || gid > u16::MAX as u32 {
            return Err(FS_ERRNO::EOVERFLOW);
        }
        let ext4 = self.fs.ext4.lock();
        let mut inode_ref = ext4.get_inode_ref(self.inode_num);
        inode_ref.inode.set_uid(uid as u16);
        inode_ref.inode.set_gid(gid as u16);
        ext4.write_back_inode(&mut inode_ref);
        Ok(())
    }

    fn check_access(&self, uid: u32, gid: u32, mode: u32) -> bool {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        inode_ref
            .inode
            .check_access(uid as u16, gid as u16, mode as u16, 0)
    }

    fn link(&self, _old_name: &str, _new_name: &str) -> Result<(), FS_ERRNO> {
        if self.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        let (child_ino, _) = self.lookup_child_meta(_old_name).ok_or(FS_ERRNO::ENOENT)?;
        let ext4 = self.fs.ext4.lock();
        let mut parent_ref = ext4.get_inode_ref(self.inode_num);
        let mut child_ref = ext4.get_inode_ref(child_ino);
        ext4.link(&mut parent_ref, &mut child_ref, _new_name)?;
        // Persist updated link counts/dir entries.
        ext4.write_back_inode(&mut parent_ref);
        ext4.write_back_inode(&mut child_ref);
        Ok(())
    }

    fn link_inode(&self, child: &Arc<dyn VfsNode>, new_name: &str) -> Result<(), FS_ERRNO> {
        if self.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        let child = child.as_any().downcast_ref::<Self>().ok_or(FS_ERRNO::EINVAL)?;
        if child.file_type == VfsFileType::Directory {
            return Err(FS_ERRNO::EISDIR);
        }
        let ext4 = self.fs.ext4.lock();
        let mut parent_ref = ext4.get_inode_ref(self.inode_num);
        let mut child_ref = ext4.get_inode_ref(child.inode_num);
        ext4.link(&mut parent_ref, &mut child_ref, new_name)?;
        ext4.write_back_inode(&mut parent_ref);
        ext4.write_back_inode(&mut child_ref);
        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), FS_ERRNO> {
        if self.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        debug!("Ext4Inode unlink: parent_inode={}, name='{}'", self.inode_num, name);
        let (child_ino, child_type) = self.lookup_child_meta(name).ok_or(FS_ERRNO::ENOENT)?;
        if child_type == VfsFileType::Directory {
            return Err(FS_ERRNO::EISDIR);
        }
        let ext4 = self.fs.ext4.lock();
        let mut parent_ref = ext4.get_inode_ref(self.inode_num);
        let mut child_ref = ext4.get_inode_ref(child_ino);
        // Hard-link case: remove only this directory entry and decrement nlink.
        if child_ref.inode.links_count() > 1 {
            ext4.dir_remove_entry(&mut parent_ref, name)?;
            let new_links = child_ref.inode.links_count() - 1;
            child_ref.inode.set_links_count(new_links);
            ext4.write_back_inode(&mut parent_ref);
            ext4.write_back_inode(&mut child_ref);
            log::debug!("Ext4Inode unlink: removed link '{}', new links_count={}", name, new_links);
            return Ok(());
        }
        // Normal case: remove directory entry, decrement nlink, and truncate if this is the last link.
        if child_ref.inode.links_count() == 1 {
            ext4.truncate_inode(&mut child_ref, 0)?;
            log::debug!("Ext4Inode unlink: truncated inode {} to 0 length", child_ino);
        }
        ext4.unlink(&mut parent_ref, &mut child_ref, name)
            .map(|_| ())?;
        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), FS_ERRNO> {
        if self.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        let (_, child_type) = self.lookup_child_meta(name).ok_or(FS_ERRNO::ENOENT)?;
        if child_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        let ext4 = self.fs.ext4.lock();
        debug!("Ext4Inode rmdir: parent_inode={}, name='{}'", self.inode_num, name);
        ext4.dir_remove(self.inode_num, name).map(|_| ())?;
        Ok(())
    }

    fn atime(&self) -> Option<InodeTime> {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        Some(decode_ext4_time(inode_ref.inode.atime(), inode_ref.inode.i_atime_extra()))
    }

    fn mtime(&self) -> Option<InodeTime> {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        Some(decode_ext4_time(inode_ref.inode.mtime(), inode_ref.inode.i_mtime_extra()))
    }

    fn ctime(&self) -> Option<InodeTime> {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        Some(decode_ext4_time(inode_ref.inode.ctime(), inode_ref.inode.i_ctime_extra()))
    }

    fn set_times(
        &self,
        atime: Option<InodeTime>,
        mtime: Option<InodeTime>,
        ctime: Option<InodeTime>,
    ) -> Result<(), FS_ERRNO> {
        let ext4 = self.fs.ext4.lock();
        let mut inode_ref = ext4.get_inode_ref(self.inode_num);

        if let Some(ts) = atime {
            let (sec_lo, extra) = encode_ext4_time(ts);
            inode_ref.inode.set_atime(sec_lo);
            inode_ref.inode.set_i_atime_extra(extra);
        }
        if let Some(ts) = mtime {
            let (sec_lo, extra) = encode_ext4_time(ts);
            inode_ref.inode.set_mtime(sec_lo);
            inode_ref.inode.set_i_mtime_extra(extra);
        }
        if let Some(ts) = ctime {
            let (sec_lo, extra) = encode_ext4_time(ts);
            inode_ref.inode.set_ctime(sec_lo);
            inode_ref.inode.set_i_ctime_extra(extra);
        }

        ext4.write_back_inode(&mut inode_ref);
        Ok(())
    }

    fn rename_child(
        &self,
        old_name: &str,
        new_parent: &Arc<dyn VfsNode>,
        new_name: &str,
    ) -> Result<(), FS_ERRNO> {
        let new_parent = new_parent.as_any().downcast_ref::<Self>().ok_or(FS_ERRNO::EXDEV)?;
        self.rename_child_to(old_name, new_parent, new_name)
    }
}
