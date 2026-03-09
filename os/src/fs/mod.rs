//! File trait & inode(dir, file, pipe, stdin, stdout)

mod inode;
mod pipe;
mod stdio;

use crate::mm::UserBuffer;

/// trait File for all file types
pub trait File: Send + Sync {
    /// the file readable?
    fn readable(&self) -> bool;
    /// the file writable?
    fn writable(&self) -> bool;
    /// read from the file to buf, return the number of bytes read
    fn read(&self, buf: UserBuffer) -> usize;
    /// write to the file from buf, return the number of bytes written
    fn write(&self, buf: UserBuffer) -> usize;
    /// Returns true if this file descriptor refers to a directory.
    fn is_dir(&self) -> bool {
        false
    }
    /// Fill `buf` with `linux_dirent64` records starting from the current directory position.
    /// Returns the number of bytes written into `buf`.
    /// The internal position is advanced accordingly.
    fn getdents64(&self, _buf: &mut [u8]) -> usize {
        0
    }
    /// get file metadata
    fn stat(&self) -> Stat;
}

/// The stat of a inode
#[repr(C)]
#[derive(Debug)]
pub struct Stat {
    /// ID of device containing file
    pub dev: u64,
    /// inode number
    pub ino: u64,
    /// file type and mode
    pub mode: StatMode,
    /// number of hard links
    pub nlink: u32,
    /// unused pad
    pad: [u64; 7],
}

bitflags! {
    /// The mode of a inode
    /// whether a directory, regular file, char device or fifo
    pub struct StatMode: u32 {
        /// null
        const NULL  = 0;
        /// directory
        const DIR   = 0o040000;
        /// ordinary regular file
        const FILE  = 0o100000;
        /// character device
        const CHAR  = 0o020000;
        /// fifo/pipe
        const FIFO  = 0o010000;
    }
}

pub use inode::{
    canonicalize, linkat, list_apps, lookup_inode, mkdir_at, open_file, open_file_at, unlinkat,
    OpenFlags, OSInode,
};
pub use pipe::{make_pipe, Pipe};
pub use stdio::{Stdin, Stdout};
