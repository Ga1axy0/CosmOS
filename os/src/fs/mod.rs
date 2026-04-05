//! File trait & inode(dir, file, pipe, stdin, stdout)

mod inode;
mod pipe;
mod stdio;
mod tty;
pub mod rootfs;
pub mod devfs;

use alloc::string::String;
use alloc::sync::Arc;
use fs::errno::FS_ERRNO;
use crate::mm::UserBuffer;
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::syscall::Pod;
pub use fs::vfs::InodeTime;
use fs::Inode;

bitflags! {
    /// `fcntl(F_GETFL/F_SETFL)` 可见的文件状态位。
    pub struct FileStatusFlags: i32 {
        /// 追加写入。
        const APPEND = 0x400;
        /// 非阻塞 I/O。
        const NONBLOCK = 0x800;
    }
}

/// 文件访问模式，对应 `O_RDONLY/O_WRONLY/O_RDWR`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccessMode {
    /// 只读打开。
    ReadOnly,
    /// 只写打开。
    WriteOnly,
    /// 读写打开。
    ReadWrite,
}

impl AccessMode {
    /// 从 `open` 低两位访问模式中解析访问权限。
    pub fn from_open_bits(bits: i32) -> Result<Self, ERRNO> {
        match bits & 0x3 {
            0 => Ok(Self::ReadOnly),
            1 => Ok(Self::WriteOnly),
            2 => Ok(Self::ReadWrite),
            _ => Err(ERRNO::EINVAL),
        }
    }

    /// 返回该访问模式是否允许读。
    pub fn readable(self) -> bool {
        matches!(self, Self::ReadOnly | Self::ReadWrite)
    }

    /// 返回该访问模式是否允许写。
    pub fn writable(self) -> bool {
        matches!(self, Self::WriteOnly | Self::ReadWrite)
    }

    /// 转回 `F_GETFL` 需要返回的访问模式位。
    pub fn bits(self) -> i32 {
        match self {
            Self::ReadOnly => 0,
            Self::WriteOnly => 1,
            Self::ReadWrite => 2,
        }
    }
}

/// 打开文件描述内部状态，对应 Linux 的 open file description 可变部分。
struct FileDescriptionInner {
    /// 当前文件偏移。
    offset: usize,
    /// 当前文件状态位。
    status_flags: FileStatusFlags,
}

/// 打开文件描述，对应 Linux 的 open file description。
pub struct FileDescription {
    /// 底层具体文件对象。
    file: Arc<dyn File + Send + Sync>,
    /// 打开时确定的访问模式。
    access_mode: AccessMode,
    /// `F_GETFL` 需要保留返回、但 `F_SETFL` 不可修改的状态位。
    status_fixed_bits: i32,
    /// 共享的偏移与状态位。
    inner: SpinNoIrqLock<FileDescriptionInner>,
}

impl FileDescription {
    /// 基于底层文件对象创建一个打开文件描述。
    pub fn new(
        file: Arc<dyn File + Send + Sync>,
        access_mode: AccessMode,
        status_flags: FileStatusFlags,
        status_fixed_bits: i32,
    ) -> Self {
        Self {
            file,
            access_mode,
            status_fixed_bits,
            inner: SpinNoIrqLock::new(FileDescriptionInner {
                    offset: 0,
                    status_flags,
                }),
        }
    }

    /// 返回底层文件对象是否允许当前描述执行读操作。
    pub fn readable(&self) -> bool {
        self.access_mode.readable() && self.file.readable()
    }

    /// 返回底层文件对象是否允许当前描述执行写操作。
    pub fn writable(&self) -> bool {
        self.access_mode.writable() && self.file.writable()
    }

    /// 顺序读取并推进共享文件偏移。
    pub fn read(&self, buf: UserBuffer) -> usize {
        if self.file.is_seekable() {
            let mut inner = self.inner.lock();
            let read_size = self.file.read_at(inner.offset, buf);
            inner.offset += read_size;
            return read_size;
        }
        // TODO: 非阻塞语义目前仍由具体后端决定，这里暂不根据 `O_NONBLOCK` 改写行为。
        self.file.read_at(0, buf)
    }

    /// 顺序写入并推进共享文件偏移。
    pub fn write(&self, buf: UserBuffer) -> usize {
        if self.file.is_seekable() {
            let mut inner = self.inner.lock();
            if inner.status_flags.contains(FileStatusFlags::APPEND) {
                // TODO: 当前仅保证同一 FileDescription 内的追加写顺序；跨描述竞争仍需 inode 级串行化。
                inner.offset = self.file.stat().size.max(0) as usize;
            }
            let write_size = self.file.write_at(inner.offset, buf);
            inner.offset += write_size;
            return write_size;
        }
        // TODO: 非阻塞语义目前仍由具体后端决定，这里暂不根据 `O_NONBLOCK` 改写行为。
        self.file.write_at(0, buf)
    }

    /// 从固定偏移读取，不影响共享文件偏移。
    pub fn read_at(&self, offset: usize, buf: UserBuffer) -> usize {
        self.file.read_at(offset, buf)
    }

    /// 向固定偏移写入，不影响共享文件偏移。
    pub fn write_at(&self, offset: usize, buf: UserBuffer) -> usize {
        self.file.write_at(offset, buf)
    }

    /// 获取当前 `F_GETFL` 可见状态值。
    pub fn status_bits(&self) -> i32 {
        let inner = self.inner.lock();
        self.access_mode.bits() | self.status_fixed_bits | inner.status_flags.bits()
    }

    /// 覆盖当前可变文件状态位。
    pub fn set_status_flags(&self, status_flags: FileStatusFlags) {
        self.inner.lock().status_flags = status_flags;
    }

    /// 返回当前文件状态位快照。
    pub fn status_flags(&self) -> FileStatusFlags {
        self.inner.lock().status_flags
    }

    /// 转发 `ioctl` 到底层文件对象。
    pub fn ioctl(&self, req: usize, arg: usize) -> Result<isize, ERRNO> {
        self.file.ioctl(req, arg)
    }

    /// 转发 `stat` 到底层文件对象。
    pub fn stat(&self) -> Stat {
        self.file.stat()
    }

    /// 返回底层文件对象是否为目录。
    pub fn is_dir(&self) -> bool {
        self.file.is_dir()
    }

    /// 返回打开路径。
    pub fn path(&self) -> Option<String> {
        self.file.path()
    }

    /// If this `FileDescription` refers to a real filesystem inode, return it.
    /// This forwards to the underlying `File` object's `as_inode` method.
    pub fn as_inode(&self) -> Option<Arc<Inode>> {
        self.file.as_inode()
    }

    /// 读取目录项并推进共享目录位置。
    pub fn getdents64(&self, buf: &mut [u8]) -> usize {
        let mut inner = self.inner.lock();
        let read_size = self.file.getdents64(inner.offset, buf);
        if read_size == 0 {
            return 0;
        }

        // 目录位置语义由底层 `linux_dirent64.d_off` 决定。
        // 当前内核目录实现将其编码为“下一个 entry index”。
        let mut cursor = 0usize;
        let mut next_off = inner.offset;
        let mut parsed_ok = false;
        while cursor + 19 <= read_size {
            let reclen = u16::from_le_bytes([buf[cursor + 16], buf[cursor + 17]]) as usize;
            if reclen == 0 || cursor + reclen > read_size {
                break;
            }
            let d_off = i64::from_le_bytes([
                buf[cursor + 8],
                buf[cursor + 9],
                buf[cursor + 10],
                buf[cursor + 11],
                buf[cursor + 12],
                buf[cursor + 13],
                buf[cursor + 14],
                buf[cursor + 15],
            ]);
            if d_off >= 0 {
                next_off = d_off as usize;
                parsed_ok = true;
            }
            cursor += reclen;
        }

        if parsed_ok {
            inner.offset = next_off;
        } else {
            // 回退：若底层未按 linux_dirent64 填充，则保持旧行为（字节偏移）。
            inner.offset += read_size;
        }
        read_size
    }

    /// 查询底层文件对象当前已就绪的 `poll` 事件。
    pub fn poll(&self, events: u16) -> u16 {
        self.file.poll(events)
    }

    /// 返回该描述对应的可轮询事件源身份。
    pub fn poll_source_id(&self) -> usize {
        self.file.poll_source_id()
    }

    /// `offset` 使用有符号 64 位，以兼容 SEEK_END 处的负位移。
    pub fn seek(&self, offset: i64, whence: u8) -> Result<u64, ERRNO> {
        if !self.file.is_seekable() {
            return Err(ERRNO::ESPIPE);
        }
        let mut inner = self.inner.lock();
        let new_offset = match whence {
            0 => offset,                      // SEEK_SET
            1 => inner.offset as i64 + offset,        // SEEK_CUR
            2 => self.file.stat().size + offset, // SEEK_END
            _ => return Err(ERRNO::EINVAL),
        };
        if new_offset < 0 {
            return Err(ERRNO::EINVAL);
        }
        inner.offset = new_offset as usize;
        Ok(new_offset as u64)
    }
}

/// trait File for all file types
pub trait File: Send + Sync {
    /// the file readable?
    fn readable(&self) -> bool;
    /// the file writable?
    fn writable(&self) -> bool;
    /// 从固定偏移读取数据。
    fn read_at(&self, _offset: usize, _buf: UserBuffer) -> usize {
        0
    }
    /// 向固定偏移写入数据。
    fn write_at(&self, _offset: usize, _buf: UserBuffer) -> usize {
        0
    }
    /// Query readiness for a subset of poll events.
    ///
    /// Input `events` is a bitmask compatible with Linux `poll(2)` bits
    /// (`POLLIN=0x001`, `POLLOUT=0x004`, ...). The return value should contain
    /// the subset that is currently ready.
    fn poll(&self, events: u16) -> u16 {
        const POLLIN: u16 = 0x001;
        const POLLOUT: u16 = 0x004;
        let mut ready = 0u16;
        if (events & POLLIN) != 0 && self.readable() {
            ready |= POLLIN;
        }
        if (events & POLLOUT) != 0 && self.writable() {
            ready |= POLLOUT;
        }
        ready
    }
    /// 返回该对象的 poll 事件源标识。
    fn poll_source_id(&self) -> usize {
        self as *const Self as *const () as usize
    }
    /// Handle an ioctl request on this file descriptor.
    fn ioctl(&self, _req: usize, _arg: usize) -> Result<isize, ERRNO> {
        Err(ERRNO::ENOTTY)
    }
    /// Returns true if this file descriptor refers to a directory.
    fn is_dir(&self) -> bool {
        false
    }
    /// Fill `buf` with `linux_dirent64` records starting from the given directory position.
    fn getdents64(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        0
    }
    /// 返回该对象是否支持共享文件偏移。
    fn is_seekable(&self) -> bool {
        false
    }
    /// get file metadata
    fn stat(&self) -> Stat;
    /// Returns the canonical path used when this file was opened, if any.
    fn path(&self) -> Option<String> {
        None
    }
    /// If this `File` is a wrapper around a real filesystem inode, return it.
    /// Default implementation returns `None` for non-inode file types.
    fn as_inode(&self) -> Option<Arc<Inode>> {
        None
    }
    /// Change the file mode bits, if supported by this file type.
    fn chmod(&self, _mode: u32) -> Result<(), FS_ERRNO> {
        Err(FS_ERRNO::EOPNOTSUPP)
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
    /// Linux-style `st_mode` bits: file type + special bits + rwx permissions.
    pub struct StatMode: u32 {
        /// file-type mask (`S_IFMT`)
        const TYPE_MASK = 0o170000;
        /// special + rwx permission mask (`S_ISUID|S_ISGID|S_ISVTX|0777`)
        const PERM_MASK = 0o007777;
        /// fifo/pipe
        const FIFO  = 0o010000;
        /// character device
        const CHAR  = 0o020000;
        /// directory
        const DIR   = 0o040000;
        /// block device
        const BLOCK = 0o060000;
        /// ordinary regular file
        const FILE  = 0o100000;
        /// symbolic link
        const LINK  = 0o120000;
        /// socket
        const SOCK  = 0o140000;

        /// set-user-ID bit
        const SUID  = 0o004000;
        /// set-group-ID bit
        const SGID  = 0o002000;
        /// sticky bit
        const STICKY = 0o001000;

        /// owner permissions
        const OWNER_R = 0o000400;
        /// owner write permission
        const OWNER_W = 0o000200;
        /// owner execute permission
        const OWNER_X = 0o000100;
        /// group permissions
        const GROUP_R = 0o000040;
        /// group write permission
        const GROUP_W = 0o000020;
        /// group execute permission
        const GROUP_X = 0o000010;
        /// other permissions
        const OTHER_R = 0o000004;
        /// other write permission
        const OTHER_W = 0o000002;
        /// other execute permission
        const OTHER_X = 0o000001;
    }
}

pub use inode::{
    canonicalize, do_mount, do_umount, init_dev, init_rootfs, inode_stat, linkat,
    list_apps, lookup_inode, mkdir_at, mount_device, open_file, open_file_at,
    rename_at, unlinkat, AT_EMPTY_PATH, AT_FDCWD, AT_REMOVEDIR, AT_SYMLINK_NOFOLLOW,
    OpenFlags, OSInode,
};
pub use pipe::{make_pipe, Pipe};
pub use stdio::new_stdio_files;
pub use tty::{Termios, TtyCore, TtyFile, WinSize};

/// Initialize the filesystem, including rootfs and devfs.
pub fn init() {
    init_rootfs();  // Virtual rootfs for booting system; meanwhile mount a real fs (e.g. ext4) to "/".
    init_dev();  // Initialize devfs, which provides device files (e.g. /dev/vda) for block devices.
}
