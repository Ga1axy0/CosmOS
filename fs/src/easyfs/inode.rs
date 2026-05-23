use alloc::{string::String, sync::Arc, vec::Vec};
use core::any::Any;
use spin::{Mutex, MutexGuard};

use crate::{
    BlockDevice, EasyFileSystem, STATFS_MAGIC_EASYFS, STATFS_NAMELEN_EASYFS, VfsStatFs,
    block_cache::get_block_cache,
    easyfs::layout::{DIRENT_SZ, DirEntry, DiskInode, DiskInodeType, SuperBlock},
    errno::FS_ERRNO,
    vfs::{Inode, VfsAttrs, VfsFileType, VfsNode},
};

/// EasyFS-backed inode implementation.
pub struct EasyInode {
    pub block_id: usize,
    pub block_offset: usize,
    fs: Arc<Mutex<EasyFileSystem>>,
    block_device: Arc<dyn BlockDevice>,
}

impl EasyInode {
    /// We should not acquire efs lock here.
    pub fn new(
        block_id: u32,
        block_offset: usize,
        fs: Arc<Mutex<EasyFileSystem>>,
        block_device: Arc<dyn BlockDevice>,
    ) -> Self {
        Self {
            block_id: block_id as usize,
            block_offset,
            fs,
            block_device,
        }
    }

    fn read_disk_inode<V>(&self, f: impl FnOnce(&DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .read(self.block_offset, f)
    }

    fn modify_disk_inode<V>(&self, f: impl FnOnce(&mut DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .modify(self.block_offset, f)
    }

    fn find_inode_id(&self, name: &str, disk_inode: &DiskInode) -> Option<u32> {
        // assert it is a directory
        assert!(disk_inode.is_dir());
        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
        let mut dirent = DirEntry::empty();
        for i in 0..file_count {
            assert_eq!(
                disk_inode.read_at(DIRENT_SZ * i, dirent.as_bytes_mut(), &self.block_device,),
                DIRENT_SZ,
            );

            if dirent.name() == name {
                return Some(dirent.inode_id());
            }
        }
        None
    }

    fn increase_size(
        &self,
        new_size: u32,
        disk_inode: &mut DiskInode,
        fs: &mut MutexGuard<EasyFileSystem>,
    ) {
        if new_size < disk_inode.size {
            return;
        }
        let blocks_needed = disk_inode.blocks_num_needed(new_size);
        let mut v: Vec<u32> = Vec::new();
        for _ in 0..blocks_needed {
            v.push(fs.alloc_data());
        }
        disk_inode.increase_size(new_size, v, &self.block_device);
    }
}

impl VfsNode for EasyInode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        // Phase 1: collect (name, block_id, block_offset) while holding the fs lock.
        let entries: Vec<(String, u32, usize)> = {
            let fs = self.fs.lock();
            self.read_disk_inode(|disk_inode| {
                let file_count = (disk_inode.size as usize) / DIRENT_SZ;
                let mut v = Vec::new();
                for i in 0..file_count {
                    let mut dirent = DirEntry::empty();
                    assert_eq!(
                        disk_inode.read_at(i * DIRENT_SZ, dirent.as_bytes_mut(), &self.block_device),
                        DIRENT_SZ,
                    );
                    let inode_id = dirent.inode_id();
                    let pos = fs.get_disk_inode_pos(inode_id);
                    v.push((String::from(dirent.name()), pos.0, pos.1));
                }
                v
            })
        };
        // Phase 2: resolve file type for each child without holding any locks.
        entries
            .into_iter()
            .map(|(name, block_id, block_offset)| {
                let child = Self::new(block_id, block_offset, self.fs.clone(), self.block_device.clone());
                let file_type = if child.read_disk_inode(|di| di.is_dir()) {
                    VfsFileType::Directory
                } else {
                    VfsFileType::Regular
                };
                (name, file_type)
            })
            .collect()
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            self.find_inode_id(name, disk_inode).map(|inode_id| {
                let (block_id, block_offset) = fs.get_disk_inode_pos(inode_id);
                Arc::new(Self::new(
                    block_id,
                    block_offset,
                    self.fs.clone(),
                    self.block_device.clone(),
                )) as Arc<dyn VfsNode>
            })
        })
    }

    fn create(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let mut fs = self.fs.lock();
        if self
            .modify_disk_inode(|root_inode| {
                // assert it is a directory
                assert!(root_inode.is_dir());
                // has the file been created?
                self.find_inode_id(name, root_inode)
            })
            .is_some()
        {
            return None;
        }
        // create a new file
        // alloc a inode with an indirect block
        let new_inode_id = fs.alloc_inode();
        // initialize inode
        let (new_inode_block_id, new_inode_block_offset) = fs.get_disk_inode_pos(new_inode_id);
        get_block_cache(new_inode_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(new_inode_block_offset, |new_inode: &mut DiskInode| {
                new_inode.initialize(DiskInodeType::File);
            });
        self.modify_disk_inode(|root_inode| {
            // append file in the dirent
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let new_size = (file_count + 1) * DIRENT_SZ;
            // increase size
            self.increase_size(new_size as u32, root_inode, &mut fs);
            // write dirent
            let dirent = DirEntry::new(name, new_inode_id);
            root_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device,
            );
        });

        let (block_id, block_offset) = fs.get_disk_inode_pos(new_inode_id);
        Some(Arc::new(Self::new(
            block_id,
            block_offset,
            self.fs.clone(),
            self.block_device.clone(),
        )) as Arc<dyn VfsNode>)
        // release efs lock automatically by compiler
    }

    /// EasyFS does not support subdirectories; always returns `None`.
    fn mkdir(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn file_type(&self) -> VfsFileType {
        if self.read_disk_inode(|d| d.is_dir()) {
            VfsFileType::Directory
        } else {
            VfsFileType::Regular
        }
    }

    fn stat_attrs(&self) -> VfsAttrs {
        let (is_dir, size) =
            self.read_disk_inode(|di| (di.is_dir(), di.size as usize));
        let ino = ((self.block_id as u64) << 32) | self.block_offset as u64;
        VfsAttrs {
            mode: if is_dir { Some(0o40755) } else { Some(0o100644) },
            ino,
            nlink: 1,
            size,
            uid: None,
            gid: None,
            rdev: 0,
            atime: None,
            mtime: None,
            ctime: None,
        }
    }

    fn statfs(&self) -> Result<VfsStatFs, FS_ERRNO> {
        let fs = self.fs.lock();
        let block_device = Arc::clone(&fs.block_device);

        let (total_blocks, data_area_blocks) = get_block_cache(0, Arc::clone(&block_device))
            .lock()
            .read(0, |super_block: &SuperBlock| {
                (super_block.total_blocks as u64, super_block.data_area_blocks as u64)
            });

        let total_inodes = fs.inode_bitmap.maximum() as u64;
        let used_inodes = fs.inode_bitmap.count_allocated(&block_device) as u64;
        let used_data_blocks = fs.data_bitmap.count_allocated(&block_device) as u64;

        let mut stat = VfsStatFs {
            f_type: STATFS_MAGIC_EASYFS,
            f_bsize: crate::BLOCK_SZ as u64,
            f_blocks: total_blocks,
            f_bfree: data_area_blocks.saturating_sub(used_data_blocks),
            f_bavail: data_area_blocks.saturating_sub(used_data_blocks),
            f_files: total_inodes,
            f_ffree: total_inodes.saturating_sub(used_inodes),
            f_fsid: [(Arc::as_ptr(&self.fs) as usize as u32) as i32, ((Arc::as_ptr(&self.fs) as usize as u64 >> 32) as u32) as i32],
            f_namelen: STATFS_NAMELEN_EASYFS,
            f_frsize: crate::BLOCK_SZ as u64,
            f_flags: 0,
            f_spare: [0; 4],
        };
        if stat.f_bfree > stat.f_blocks {
            stat.f_bfree = stat.f_blocks;
            stat.f_bavail = stat.f_blocks;
        }
        Ok(stat)
    }

    fn clear(&self) {
        let mut fs = self.fs.lock();
        self.modify_disk_inode(|disk_inode| {
            let size = disk_inode.size;
            let data_blocks_dealloc = disk_inode.clear_size(&self.block_device);
            assert!(data_blocks_dealloc.len() == DiskInode::total_blocks(size) as usize);
            for data_block in data_blocks_dealloc.into_iter() {
                fs.dealloc_data(data_block);
            }
        });
    }

    fn truncate(&self, _new_size: usize) -> Result<(), FS_ERRNO> {
        // TODO：补齐 EasyFS 的通用 truncate 语义，包括缩容回收与扩容补零。
        Err(FS_ERRNO::EOPNOTSUPP)
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| disk_inode.read_at(offset, buf, &self.block_device))
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let mut fs = self.fs.lock();
        self.modify_disk_inode(|disk_inode| {
            self.increase_size((offset + buf.len()) as u32, disk_inode, &mut fs);
            disk_inode.write_at(offset, buf, &self.block_device)
        })
    }

    fn size(&self) -> usize {
        self.read_disk_inode(|disk_inode| disk_inode.size as usize)
    }

    fn ino(&self) -> u64 {
        ((self.block_id as u64) << 32) | self.block_offset as u64
    }

    fn fs_id(&self) -> u64 {
        Arc::as_ptr(&self.fs) as usize as u64
    }
}

impl EasyFileSystem {
    /// 返回根目录对应的稳定内存 inode。
    pub fn root_inode(efs: &Arc<Mutex<Self>>) -> Arc<Inode> {
        let block_device = Arc::clone(&efs.lock().block_device);
        // acquire efs lock temporarily
        let (block_id, block_offset) = efs.lock().get_disk_inode_pos(0);
        // release efs lock
        Inode::from_vfs_node(Arc::new(EasyInode::new(
            block_id,
            block_offset,
            Arc::clone(efs),
            block_device,
        )))
    }
}
