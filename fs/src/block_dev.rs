//! Block device interface.
//!
//! Define the block read-write interface [BlockDevice] that the device driver needs to implement

use core::any::Any;

use crate::BLOCK_SZ;

/// A contiguous 512-byte-block write request.
pub struct BlockWrite<'a> {
    pub start_block: usize,
    pub data: &'a [u8],
}

/// A contiguous 512-byte-block read request.
pub struct BlockRead<'a> {
    pub start_block: usize,
    pub data: &'a mut [u8],
}

pub trait BlockDevice: Send + Sync + Any {
    /// Return this block device as `Any` for downcasting.
    fn as_any(&self) -> &dyn Any;
    /// Read a block from the block device.
    fn read_block(&self, block_id: usize, buf: &mut [u8]);
    /// Write a block to the block device.
    fn write_block(&self, block_id: usize, buf: &[u8]);
    /// Read a contiguous range of 512-byte blocks.
    fn read_blocks(&self, start_block: usize, buf: &mut [u8]) {
        assert!(buf.len() % BLOCK_SZ == 0);
        for (idx, block) in buf.chunks_mut(BLOCK_SZ).enumerate() {
            self.read_block(start_block + idx, block);
        }
    }
    /// Write a contiguous range of 512-byte blocks.
    fn write_blocks(&self, start_block: usize, buf: &[u8]) {
        assert!(buf.len() % BLOCK_SZ == 0);
        for (idx, block) in buf.chunks(BLOCK_SZ).enumerate() {
            self.write_block(start_block + idx, block);
        }
    }
    /// Write multiple independent contiguous ranges.
    fn write_blocks_many(&self, writes: &[BlockWrite<'_>]) {
        for write in writes {
            if !write.data.is_empty() {
                self.write_blocks(write.start_block, write.data);
            }
        }
    }

    /// Read multiple independent contiguous ranges.
    ///
    /// Devices with a request queue should override this to submit several
    /// requests before waiting for completions.
    fn read_blocks_many(&self, reads: &mut [BlockRead<'_>]) {
        for read in reads {
            if !read.data.is_empty() {
                self.read_blocks(read.start_block, read.data);
            }
        }
    }
}
