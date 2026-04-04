//! Minimal FAT32 implementation for a no_std kernel.
//!
//! Current scope (incremental):
//! - No partition table support (volume starts at LBA 0)
//! - Supports SFN (8.3) and VFAT LFN directory entries
//! - ASCII name lookup is case-insensitive; SFN rendering respects FAT NT case bits
//! - Basic directory operations: ls/find/create/mkdir in a directory tree
//! - File operations: read_at/write_at/clear

mod bpb;
mod dir;
mod fat;
mod inode;

use alloc::sync::Arc;

use crate::block_dev::BlockDevice;
use crate::lock::{BlockingMutex, LockHookTable, LockWaitHook, LockWakeHook};
use crate::vfs::{Inode, VfsNode};

pub use bpb::Fat32Bpb;

static FAT32_LOCK_HOOKS: LockHookTable = LockHookTable::new();

/// Configure FAT32 lock contention/wakeup hooks.
pub fn set_fat32_lock_hooks(wait: Option<LockWaitHook>, wake: Option<LockWakeHook>) {
    FAT32_LOCK_HOOKS.set_hooks(wait, wake);
}

/// FAT32 filesystem instance (volume assumed to start at LBA 0).
pub struct Fat32FileSystem {
    block_device: Arc<dyn BlockDevice>,
    bpb: Fat32Bpb,
    inner: BlockingMutex<fat::Fat32Inner>,
}

impl Fat32FileSystem {
    /// Open an existing FAT32 volume.
    pub fn open(block_device: Arc<dyn BlockDevice>) -> Arc<Self> {
        let bpb = Fat32Bpb::read_from(&block_device);
        let inner = fat::Fat32Inner::new(&bpb);
        Arc::new(Self {
            block_device,
            bpb,
            inner: BlockingMutex::new_with_hooks(inner, &FAT32_LOCK_HOOKS),
        })
    }

    pub fn bpb(&self) -> &Fat32Bpb {
        &self.bpb
    }

    pub(crate) fn device(&self) -> &Arc<dyn BlockDevice> {
        &self.block_device
    }

    pub(crate) fn inner(&self) -> &BlockingMutex<fat::Fat32Inner> {
        &self.inner
    }

    /// Root directory inode.
    pub fn root_inode(fs: &Arc<Self>) -> Inode {
        Inode::new(Arc::new(inode::FatInode::new_root(Arc::clone(fs))) as Arc<dyn VfsNode>)
    }
}
