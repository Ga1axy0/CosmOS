use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};
use core::any::Any;
use log::debug;
use spin::Mutex;

use crate::block_dev::BlockDevice as OsBlockDevice;
use crate::errno::FS_ERRNO;
use crate::vfs::{Inode, InodeTime, VfsNode};
use crate::BLOCK_SZ;

use ext4_rs::{
    BlockDevice as Ext4BlockDevice, Ext4, InodeFileType
};

/// Adapts the OS block-id based device into ext4_rs offset-based IO.
struct Ext4BlockDeviceAdapter {
    inner: Arc<dyn OsBlockDevice>,
}

/// 4KiB 对齐的扇区缓冲区，避免 virtio 直接访问跨物理页的 512-byte 缓冲。
#[repr(align(4096))]
struct AlignedSector([u8; BLOCK_SZ]);

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
        let end_block = (offset + len + BLOCK_SZ - 1) / BLOCK_SZ;

        for block_id in start_block..end_block {
            let mut sector = Box::new(AlignedSector([0u8; BLOCK_SZ]));

            // debug!("Ext4BlockDeviceAdapter read: block_id={}, offset={}, len={}", block_id, offset, len);
            self.inner.read_block(block_id, &mut sector.0);

            let block_start = block_id * BLOCK_SZ;
            let src_start = offset.saturating_sub(block_start);
            let src_end = BLOCK_SZ.min(offset + len - block_start);
            if src_start >= src_end {
                continue;
            }

            let dst_start = block_start + src_start - offset;
            let copy_len = src_end - src_start;
            out[dst_start..dst_start + copy_len].copy_from_slice(&sector.0[src_start..src_end]);
        }

        out
    }

    fn write_offset(&self, offset: usize, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let start_block = offset / BLOCK_SZ;
        let end_block = (offset + data.len() + BLOCK_SZ - 1) / BLOCK_SZ;

        for block_id in start_block..end_block {
            let block_start = block_id * BLOCK_SZ;
            let seg_start = offset.max(block_start);
            let seg_end = (offset + data.len()).min(block_start + BLOCK_SZ);
            if seg_start >= seg_end {
                continue;
            }

            let src_start = seg_start - offset;
            let src_end = seg_end - offset;
            let dst_start = seg_start - block_start;
            let dst_end = seg_end - block_start;

            if dst_start == 0 && dst_end == BLOCK_SZ {
                // 这里使用固定大小的跳板缓冲区，避免直接把可能跨物理页的 heap slice 交给 virtio 块设备。
                let mut sector = Box::new(AlignedSector([0u8; BLOCK_SZ]));
                sector.0.copy_from_slice(&data[src_start..src_end]);
                self.inner.write_block(block_id, &sector.0);
            } else {
                let mut sector = Box::new(AlignedSector([0u8; BLOCK_SZ]));

                // debug!("Ext4BlockDeviceAdapter partial block read: block_id={}, dst_start={}, dst_end={}, src_start={}, src_end={}", block_id, dst_start, dst_end, src_start, src_end);
                self.inner.read_block(block_id, &mut sector.0);
                
                sector.0[dst_start..dst_end].copy_from_slice(&data[src_start..src_end]);

                // debug!("Ext4BlockDeviceAdapter partial block write: block_id={}, dst_start={}, dst_end={}, src_start={}, src_end={}", block_id, dst_start, dst_end, src_start, src_end);
                self.inner.write_block(block_id, &sector.0);
            }
        }
    }
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

    pub fn root_inode(fs: &Arc<Self>) -> Inode {
        Inode::new(Arc::new(Ext4Inode::new(Arc::clone(fs), EXT4_ROOT_INODE, true)))
    }
}

pub struct Ext4Inode {
    fs: Arc<Ext4FileSystem>,
    inode_num: u32,
    is_dir: bool,
}

impl Ext4Inode {
    fn new(fs: Arc<Ext4FileSystem>, inode_num: u32, is_dir: bool) -> Self {
        Self {
            fs,
            inode_num,
            is_dir,
        }
    }

    fn lookup_child_meta(&self, name: &str) -> Option<(u32, bool)> {
        if !self.is_dir {
            return None;
        }
        let ext4 = self.fs.ext4.lock();
        ext4.ext4_dir_get_entries(self.inode_num)
            .into_iter()
            .find(|de| de.get_name() == name)
            .map(|de| (de.inode, de.get_de_type() == 2))
    }

    fn rename_child_to(&self, old_name: &str, new_parent: &Self, new_name: &str) -> Result<(), FS_ERRNO> {
        if !self.is_dir || !new_parent.is_dir {
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

impl VfsNode for Ext4Inode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn ls(&self) -> Vec<String> {
        if !self.is_dir {
            return Vec::new();
        }
        let ext4 = self.fs.ext4.lock();
        ext4
            .ext4_dir_get_entries(self.inode_num)
            .into_iter()
            .map(|de| de.get_name())
            .filter(|name| name != "." && name != "..")
            .collect()
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let (inode_num, is_dir) = self.lookup_child_meta(name)?;
        Some(Arc::new(Self::new(Arc::clone(&self.fs), inode_num, is_dir)) as Arc<dyn VfsNode>)
    }

    fn create(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        if !self.is_dir {
            return None;
        }
        let ext4 = self.fs.ext4.lock();
        let inode = ext4
            .create(self.inode_num, name, InodeFileType::S_IFREG.bits())
            .ok()?;
        Some(Arc::new(Self::new(Arc::clone(&self.fs), inode.inode_num, false)) as Arc<dyn VfsNode>)
    }

    fn mkdir(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        if !self.is_dir {
            return None;
        }
        let ext4 = self.fs.ext4.lock();
        let inode = ext4
            .create(self.inode_num, name, InodeFileType::S_IFDIR.bits())
            .ok()?;
        Some(Arc::new(Self::new(Arc::clone(&self.fs), inode.inode_num, true)) as Arc<dyn VfsNode>)
    }

    fn is_dir(&self) -> bool {
        self.is_dir
    }

    fn clear(&self) {
        if self.is_dir {
            return;
        }
        let ext4 = self.fs.ext4.lock();
        let mut inode_ref = ext4.get_inode_ref(self.inode_num);
        // let _ = ext4.truncate_inode(&mut inode_ref, 0);
        ext4.truncate_inode(&mut inode_ref, 0).unwrap();
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let ext4 = self.fs.ext4.lock();
        ext4.read_at(self.inode_num, offset, buf).unwrap_or(0)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let ext4 = self.fs.ext4.lock();
        ext4.write_at(self.inode_num, offset, buf).unwrap_or(0)
    }

    fn ino(&self) -> u64 {
        self.inode_num as u64
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

    fn set_mode(&self, mode: u32) -> Result<(), FS_ERRNO> {
        let ext4 = self.fs.ext4.lock();
        let mut inode_ref = ext4.get_inode_ref(self.inode_num);
        inode_ref.inode.set_mode(mode as u16);
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
        if !self.is_dir {
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
        if !self.is_dir {
            return Err(FS_ERRNO::ENOTDIR);
        }
        let child = child.as_any().downcast_ref::<Self>().ok_or(FS_ERRNO::EINVAL)?;
        if child.is_dir {
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
        if !self.is_dir {
            return Err(FS_ERRNO::ENOTDIR);
        }
        debug!("Ext4Inode unlink: parent_inode={}, name='{}'", self.inode_num, name);
        let (child_ino, is_dir) = self.lookup_child_meta(name).ok_or(FS_ERRNO::ENOENT)?;
        if is_dir {
            return Err(FS_ERRNO::EISDIR);
        }
        let ext4 = self.fs.ext4.lock();
        let mut parent_ref = ext4.get_inode_ref(self.inode_num);
        let mut child_ref = ext4.get_inode_ref(child_ino);
        // Hard-link case: remove only this directory entry and decrement nlink.
        if !is_dir && child_ref.inode.links_count() > 1 {
            ext4.dir_remove_entry(&mut parent_ref, name)?;
            let new_links = child_ref.inode.links_count() - 1;
            child_ref.inode.set_links_count(new_links);
            ext4.write_back_inode(&mut parent_ref);
            ext4.write_back_inode(&mut child_ref);
            log::debug!("Ext4Inode unlink: removed link '{}', new links_count={}", name, new_links);
            return Ok(());
        }
        // Normal case: remove directory entry, decrement nlink, and truncate if this is the last link.
        if !is_dir && child_ref.inode.links_count() == 1 {
            ext4.truncate_inode(&mut child_ref, 0)?;
            log::debug!("Ext4Inode unlink: truncated inode {} to 0 length", child_ino);
        }
        ext4.unlink(&mut parent_ref, &mut child_ref, name)
            .map(|_| ())?;
        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), FS_ERRNO> {
        if !self.is_dir {
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
