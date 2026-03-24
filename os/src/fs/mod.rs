//! File trait & inode(dir, file, pipe, stdin, stdout)

mod inode;
mod pipe;
mod stdio;
mod tty;
pub mod rootfs;
pub mod devfs;

use alloc::string::String;
use crate::mm::UserBuffer;
use crate::syscall::errno::ERRNO;
use crate::syscall::Pod;

/// trait File for all file types
pub trait File: Send + Sync {
    /// the file readable?
    fn readable(&self) -> bool;
    /// the file writable?
    fn writable(&self) -> bool;
    /// read from the file to buf, return the number of bytes read
    fn read(&self, buf: UserBuffer) -> usize;
    /// read from a fixed file offset without changing the current fd offset
    fn read_at(&self, _offset: usize, _buf: UserBuffer) -> usize {
        0
    }
    /// write to the file from buf, return the number of bytes written
    fn write(&self, buf: UserBuffer) -> usize;
    /// Handle an ioctl request on this file descriptor.
    fn ioctl(&self, _req: usize, _arg: usize) -> Result<isize, ERRNO> {
        Err(ERRNO::ENOTTY)
    }
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
    /// Returns the canonical path used when this file was opened, if any.
    fn path(&self) -> Option<String> {
        None
    }
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
    /// user ID of owner
    pub uid: u32,
    /// group ID of owner
    pub gid: u32,
    /// device ID (if special file)
    pub rdev: u64,
    /// padding to keep C ABI-compatible layout
    pub pad0: usize,
    /// total size, in bytes
    pub size: i64,
    /// preferred block size for I/O
    pub blksize: u32,
    /// padding to keep C ABI-compatible layout
    pub pad1: i32,
    /// number of 512-byte blocks allocated
    pub blocks: u64,
    /// time of last access (seconds)
    pub atime_sec: isize,
    /// time of last access (nanoseconds)
    pub atime_nsec: isize,
    /// time of last modification (seconds)
    pub mtime_sec: isize,
    /// time of last modification (nanoseconds)
    pub mtime_nsec: isize,
    /// time of last status change (seconds)
    pub ctime_sec: isize,
    /// time of last status change (nanoseconds)
    pub ctime_nsec: isize,
    /// reserved fields
    pub unused: [u32; 2],
}

impl Pod for Stat {}

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
    canonicalize, do_mount, do_umount, init_dev, init_rootfs, linkat, list_apps,
    lookup_inode, mkdir_at, mount_device, open_file, open_file_at, unlinkat,
    AT_FDCWD, AT_REMOVEDIR, OpenFlags, OSInode,
};
pub use pipe::{make_pipe, Pipe};
pub use stdio::new_stdio_files;
pub use tty::{Termios, TtyCore, TtyFile, WinSize};

/// Initialize the filesystem, including rootfs and devfs.
pub fn init() {
    init_rootfs();  // Virtual rootfs for booting system; meanwhile mount a real fs (e.g. ext4) to "/".
    init_dev();  // Initialize devfs, which provides device files (e.g. /dev/vda) for block devices.
}
