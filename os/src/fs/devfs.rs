//! Block-device VFS nodes for `/dev`.
//!
//! [`BlockDevNode`] wraps an `Arc<dyn BlockDevice>` and exposes it as a VFS
//! node so that `sys_mount` can resolve `/dev/vda` (or `/dev/vdb`, etc.) into
//! the underlying block-device driver without a separate devfs daemon.
//!
//! The nodes are purely in-memory and are registered under the virtual `/dev`
//! directory by [`super::inode::init_dev`] at boot time.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use fs::vfs::VfsNode;
use fs::BlockDevice;

/// VFS node representing a raw block device (e.g. `/dev/vda`).
///
/// Supports `read_at` / `write_at` for direct sector-aligned block I/O.
/// All directory operations (`ls`, `find`, `mkdir`, …) return empty / `None`.
pub struct BlockDevNode {
    /// The underlying block device driver.
    pub device: Arc<dyn BlockDevice>,
}

impl BlockDevNode {
    /// Wrap `device` in a new node.
    pub fn new(device: Arc<dyn BlockDevice>) -> Self {
        Self { device }
    }
}

// SAFETY: single-processor kernel; `BlockDevice` is already `Send + Sync`.
unsafe impl Send for BlockDevNode {}
unsafe impl Sync for BlockDevNode {}

impl VfsNode for BlockDevNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_dir(&self) -> bool {
        false
    }

    fn ls(&self) -> Vec<String> {
        Vec::new()
    }

    fn find(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn create(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn mkdir(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn clear(&self) {}

    /// Read `buf.len()` bytes from the device starting at byte `offset`.
    ///
    /// Uses a 512-byte stack buffer for any partial-block reads.
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        const BLOCK_SIZE: usize = 512;
        let mut total = 0usize;
        let mut pos = offset;
        let mut tmp = [0u8; BLOCK_SIZE];
        while total < buf.len() {
            let blk = pos / BLOCK_SIZE;
            let blk_off = pos % BLOCK_SIZE;
            self.device.read_block(blk, &mut tmp);
            let copy = (BLOCK_SIZE - blk_off).min(buf.len() - total);
            buf[total..total + copy].copy_from_slice(&tmp[blk_off..blk_off + copy]);
            total += copy;
            pos += copy;
        }
        total
    }

    /// Write `buf` to the device starting at byte `offset`.
    ///
    /// Performs a read-modify-write for any partial leading/trailing blocks.
    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        const BLOCK_SIZE: usize = 512;
        let mut total = 0usize;
        let mut pos = offset;
        while total < buf.len() {
            let blk = pos / BLOCK_SIZE;
            let blk_off = pos % BLOCK_SIZE;
            let mut tmp = [0u8; BLOCK_SIZE];
            // Read the existing block content for partial writes.
            self.device.read_block(blk, &mut tmp);
            let copy = (BLOCK_SIZE - blk_off).min(buf.len() - total);
            tmp[blk_off..blk_off + copy].copy_from_slice(&buf[total..total + copy]);
            self.device.write_block(blk, &tmp);
            total += copy;
            pos += copy;
        }
        total
    }
}
