//! Minimal FAT32 implementation for a no_std kernel.
//!
//! Current scope (incremental):
//! - No partition table support (volume starts at LBA 0)
//! - SFN (8.3) only; LFN entries are ignored
//! - Basic directory operations: ls/find/create in a single directory
//! - File operations: read_at/write_at/clear

mod bpb;
mod dir;
mod fat;
mod inode;

use alloc::sync::Arc;
use spin::Mutex;

use crate::block_dev::BlockDevice;
use crate::vfs::{Inode, VfsNode};

pub use bpb::Fat32Bpb;

/// FAT32 filesystem instance (volume assumed to start at LBA 0).
pub struct Fat32FileSystem {
    block_device: Arc<dyn BlockDevice>,
    bpb: Fat32Bpb,
    inner: Mutex<fat::Fat32Inner>,
}

impl Fat32FileSystem {
    /// Open an existing FAT32 volume.
    pub fn open(block_device: Arc<dyn BlockDevice>) -> Arc<Self> {
        let bpb = Fat32Bpb::read_from(&block_device);
        let inner = fat::Fat32Inner::new(&bpb);
        Arc::new(Self {
            block_device,
            bpb,
            inner: Mutex::new(inner),
        })
    }

    pub fn bpb(&self) -> &Fat32Bpb {
        &self.bpb
    }

    pub(crate) fn device(&self) -> &Arc<dyn BlockDevice> {
        &self.block_device
    }

    pub(crate) fn inner(&self) -> &Mutex<fat::Fat32Inner> {
        &self.inner
    }

    /// Root directory inode.
    pub fn root_inode(fs: &Arc<Self>) -> Inode {
        Inode::new(Arc::new(inode::FatInode::new_root(Arc::clone(fs))) as Arc<dyn VfsNode>)
    }
}
