//! File trait & inode(dir, file, pipe, stdin, stdout)

pub mod cgroupfs;
pub mod devfs;
mod inode;
mod page_cache;
mod pipe;
pub mod procfs;
pub mod rootfs;
mod stdio;
pub mod sysfs;
/// In-memory tmpfs backend that can be mounted into the virtual namespace.
pub mod tmpfs;
mod tty;

use crate::mm::UserBuffer;
use crate::sync::{SleepMutex, SpinNoIrqLock};
use crate::syscall::errno::ERRNO;
use crate::syscall::Pod;
use crate::timer::get_time_us;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
#[cfg(feature = "io_perf_counters")]
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};
pub use fs::vfs::{InodeTime, VfsFileType};
use fs::{
    dentry_cache_stats, errno::FS_ERRNO, inode_cache_stats, DentryCacheStats, Inode,
    InodeCacheStats,
};
use lazy_static::*;
pub use page_cache::{
    discard_inode, mapping_for_inode, mark_cached_page_dirty, page_cache_stats, reclaim_if_needed,
    release_mapped_page, retain_mapped_page, sync_all as sync_page_cache_all,
    sync_fs as sync_page_cache_fs, sync_inode_range, truncate_inode, CachePage, PageCacheStats,
    PAGE_CACHE_MANAGER,
};

/// Cumulative directory-iteration counters used by `/proc/mm_perf`.
#[derive(Clone, Copy, Debug, Default)]
pub struct GetdentsPerfCounters {
    /// Number of `getdents64` calls serviced by the kernel.
    pub calls: usize,
    /// Total bytes returned across all `getdents64` calls.
    pub bytes: usize,
    /// Total time spent inside kernel-side directory iteration paths.
    pub total_us: usize,
    /// Number of times a full directory snapshot (`inode.ls()`) was rebuilt.
    pub dir_snapshots: usize,
    /// Sum of directory entry counts observed across those rebuilds.
    pub dir_snapshot_entries: usize,
    /// Total time spent rebuilding directory snapshots.
    pub dir_snapshot_us: usize,
}

static GETDENTS_CALLS: AtomicUsize = AtomicUsize::new(0);
static GETDENTS_BYTES: AtomicUsize = AtomicUsize::new(0);
static GETDENTS_TOTAL_US: AtomicUsize = AtomicUsize::new(0);
static DIR_SNAPSHOT_CALLS: AtomicUsize = AtomicUsize::new(0);
static DIR_SNAPSHOT_ENTRIES: AtomicUsize = AtomicUsize::new(0);
static DIR_SNAPSHOT_TOTAL_US: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static LOOKUP_INODE_FOLLOW_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static LOOKUP_INODE_FOLLOW_OK: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static LOOKUP_INODE_FOLLOW_ERR: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static LOOKUP_INODE_FOLLOW_COMPONENTS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static LOOKUP_INODE_FOLLOW_RESTARTS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static LOOKUP_INODE_FOLLOW_TOTAL_US: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static INODE_STAT_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static INODE_STAT_TOTAL_US: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static NEWFSTATAT_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static NEWFSTATAT_EMPTY_PATH_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static NEWFSTATAT_TOTAL_US: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static NEWFSTATAT_RESOLVE_US: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static NEWFSTATAT_LOOKUP_US: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static NEWFSTATAT_INODE_STAT_US: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static NEWFSTATAT_COPYOUT_US: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn record_getdents_perf(bytes: usize, elapsed_us: usize) {
    GETDENTS_CALLS.fetch_add(1, Ordering::Relaxed);
    GETDENTS_BYTES.fetch_add(bytes, Ordering::Relaxed);
    GETDENTS_TOTAL_US.fetch_add(elapsed_us, Ordering::Relaxed);
}

#[inline]
fn record_dir_snapshot_perf(entries: usize, elapsed_us: usize) {
    DIR_SNAPSHOT_CALLS.fetch_add(1, Ordering::Relaxed);
    DIR_SNAPSHOT_ENTRIES.fetch_add(entries, Ordering::Relaxed);
    DIR_SNAPSHOT_TOTAL_US.fetch_add(elapsed_us, Ordering::Relaxed);
}

/// Return cumulative directory-iteration counters for `/proc/mm_perf`.
pub fn getdents_perf_counters() -> GetdentsPerfCounters {
    GetdentsPerfCounters {
        calls: GETDENTS_CALLS.load(Ordering::Relaxed),
        bytes: GETDENTS_BYTES.load(Ordering::Relaxed),
        total_us: GETDENTS_TOTAL_US.load(Ordering::Relaxed),
        dir_snapshots: DIR_SNAPSHOT_CALLS.load(Ordering::Relaxed),
        dir_snapshot_entries: DIR_SNAPSHOT_ENTRIES.load(Ordering::Relaxed),
        dir_snapshot_us: DIR_SNAPSHOT_TOTAL_US.load(Ordering::Relaxed),
    }
}

/// Return the current global dentry-cache footprint for `/proc/mm_perf`.
pub fn dentry_perf_counters() -> DentryCacheStats {
    dentry_cache_stats()
}

/// Return the current global inode-cache footprint for `/proc/mm_perf`.
pub fn inode_perf_counters() -> InodeCacheStats {
    inode_cache_stats()
}

#[cfg(feature = "io_perf_counters")]
fn perf_load(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::Relaxed)
}

#[cfg(feature = "io_perf_counters")]
pub(crate) fn record_lookup_inode_follow_perf(
    components: usize,
    restarts: usize,
    ok: bool,
    elapsed_us: usize,
) {
    LOOKUP_INODE_FOLLOW_CALLS.fetch_add(1, Ordering::Relaxed);
    if ok {
        LOOKUP_INODE_FOLLOW_OK.fetch_add(1, Ordering::Relaxed);
    } else {
        LOOKUP_INODE_FOLLOW_ERR.fetch_add(1, Ordering::Relaxed);
    }
    LOOKUP_INODE_FOLLOW_COMPONENTS.fetch_add(components, Ordering::Relaxed);
    LOOKUP_INODE_FOLLOW_RESTARTS.fetch_add(restarts, Ordering::Relaxed);
    LOOKUP_INODE_FOLLOW_TOTAL_US.fetch_add(elapsed_us, Ordering::Relaxed);
}

#[cfg(not(feature = "io_perf_counters"))]
pub(crate) fn record_lookup_inode_follow_perf(
    _components: usize,
    _restarts: usize,
    _ok: bool,
    _elapsed_us: usize,
) {
}

#[cfg(feature = "io_perf_counters")]
pub(crate) fn record_inode_stat_perf(elapsed_us: usize) {
    INODE_STAT_CALLS.fetch_add(1, Ordering::Relaxed);
    INODE_STAT_TOTAL_US.fetch_add(elapsed_us, Ordering::Relaxed);
}

#[cfg(not(feature = "io_perf_counters"))]
pub(crate) fn record_inode_stat_perf(_elapsed_us: usize) {}

#[cfg(feature = "io_perf_counters")]
pub(crate) fn record_newfstatat_perf(
    empty_path: bool,
    resolve_us: usize,
    lookup_us: usize,
    inode_stat_us: usize,
    copyout_us: usize,
    total_us: usize,
) {
    NEWFSTATAT_CALLS.fetch_add(1, Ordering::Relaxed);
    if empty_path {
        NEWFSTATAT_EMPTY_PATH_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    NEWFSTATAT_RESOLVE_US.fetch_add(resolve_us, Ordering::Relaxed);
    NEWFSTATAT_LOOKUP_US.fetch_add(lookup_us, Ordering::Relaxed);
    NEWFSTATAT_INODE_STAT_US.fetch_add(inode_stat_us, Ordering::Relaxed);
    NEWFSTATAT_COPYOUT_US.fetch_add(copyout_us, Ordering::Relaxed);
    NEWFSTATAT_TOTAL_US.fetch_add(total_us, Ordering::Relaxed);
}

#[cfg(not(feature = "io_perf_counters"))]
pub(crate) fn record_newfstatat_perf(
    _empty_path: bool,
    _resolve_us: usize,
    _lookup_us: usize,
    _inode_stat_us: usize,
    _copyout_us: usize,
    _total_us: usize,
) {
}

#[cfg(feature = "io_perf_counters")]
/// Reset filesystem metadata counters exported through `/proc/io_perf`.
pub fn reset_perf_counters() {
    GETDENTS_CALLS.store(0, Ordering::Relaxed);
    GETDENTS_BYTES.store(0, Ordering::Relaxed);
    GETDENTS_TOTAL_US.store(0, Ordering::Relaxed);
    DIR_SNAPSHOT_CALLS.store(0, Ordering::Relaxed);
    DIR_SNAPSHOT_ENTRIES.store(0, Ordering::Relaxed);
    DIR_SNAPSHOT_TOTAL_US.store(0, Ordering::Relaxed);
    LOOKUP_INODE_FOLLOW_CALLS.store(0, Ordering::Relaxed);
    LOOKUP_INODE_FOLLOW_OK.store(0, Ordering::Relaxed);
    LOOKUP_INODE_FOLLOW_ERR.store(0, Ordering::Relaxed);
    LOOKUP_INODE_FOLLOW_COMPONENTS.store(0, Ordering::Relaxed);
    LOOKUP_INODE_FOLLOW_RESTARTS.store(0, Ordering::Relaxed);
    LOOKUP_INODE_FOLLOW_TOTAL_US.store(0, Ordering::Relaxed);
    INODE_STAT_CALLS.store(0, Ordering::Relaxed);
    INODE_STAT_TOTAL_US.store(0, Ordering::Relaxed);
    NEWFSTATAT_CALLS.store(0, Ordering::Relaxed);
    NEWFSTATAT_EMPTY_PATH_CALLS.store(0, Ordering::Relaxed);
    NEWFSTATAT_TOTAL_US.store(0, Ordering::Relaxed);
    NEWFSTATAT_RESOLVE_US.store(0, Ordering::Relaxed);
    NEWFSTATAT_LOOKUP_US.store(0, Ordering::Relaxed);
    NEWFSTATAT_INODE_STAT_US.store(0, Ordering::Relaxed);
    NEWFSTATAT_COPYOUT_US.store(0, Ordering::Relaxed);
}

#[cfg(feature = "io_perf_counters")]
/// Render filesystem metadata counters for `/proc/io_perf`.
pub fn render_perf_counters() -> String {
    let mut out = String::new();
    let getdents_calls = perf_load(&GETDENTS_CALLS);
    let lookup_calls = perf_load(&LOOKUP_INODE_FOLLOW_CALLS);
    let inode_stat_calls = perf_load(&INODE_STAT_CALLS);
    let newfstatat_calls = perf_load(&NEWFSTATAT_CALLS);
    let _ = writeln!(&mut out, "fs_meta:");
    let _ = writeln!(&mut out, "  getdents_calls {}", getdents_calls);
    let _ = writeln!(&mut out, "  getdents_bytes {}", perf_load(&GETDENTS_BYTES));
    let _ = writeln!(
        &mut out,
        "  getdents_total_us {}",
        perf_load(&GETDENTS_TOTAL_US)
    );
    let _ = writeln!(
        &mut out,
        "  dir_snapshot_calls {}",
        perf_load(&DIR_SNAPSHOT_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  dir_snapshot_entries {}",
        perf_load(&DIR_SNAPSHOT_ENTRIES)
    );
    let _ = writeln!(
        &mut out,
        "  dir_snapshot_total_us {}",
        perf_load(&DIR_SNAPSHOT_TOTAL_US)
    );
    let _ = writeln!(&mut out, "  lookup_inode_follow_calls {}", lookup_calls);
    let _ = writeln!(
        &mut out,
        "  lookup_inode_follow_ok {}",
        perf_load(&LOOKUP_INODE_FOLLOW_OK)
    );
    let _ = writeln!(
        &mut out,
        "  lookup_inode_follow_err {}",
        perf_load(&LOOKUP_INODE_FOLLOW_ERR)
    );
    let _ = writeln!(
        &mut out,
        "  lookup_inode_follow_components {}",
        perf_load(&LOOKUP_INODE_FOLLOW_COMPONENTS)
    );
    let _ = writeln!(
        &mut out,
        "  lookup_inode_follow_restarts {}",
        perf_load(&LOOKUP_INODE_FOLLOW_RESTARTS)
    );
    let _ = writeln!(
        &mut out,
        "  lookup_inode_follow_total_us {}",
        perf_load(&LOOKUP_INODE_FOLLOW_TOTAL_US)
    );
    let _ = writeln!(
        &mut out,
        "  avg_lookup_inode_follow_us_x100 {}",
        if lookup_calls == 0 {
            0
        } else {
            perf_load(&LOOKUP_INODE_FOLLOW_TOTAL_US).saturating_mul(100) / lookup_calls
        }
    );
    let _ = writeln!(&mut out, "  inode_stat_calls {}", inode_stat_calls);
    let _ = writeln!(
        &mut out,
        "  inode_stat_total_us {}",
        perf_load(&INODE_STAT_TOTAL_US)
    );
    let _ = writeln!(
        &mut out,
        "  avg_inode_stat_us_x100 {}",
        if inode_stat_calls == 0 {
            0
        } else {
            perf_load(&INODE_STAT_TOTAL_US).saturating_mul(100) / inode_stat_calls
        }
    );
    let _ = writeln!(&mut out, "  newfstatat_calls {}", newfstatat_calls);
    let _ = writeln!(
        &mut out,
        "  newfstatat_empty_path_calls {}",
        perf_load(&NEWFSTATAT_EMPTY_PATH_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  newfstatat_total_us {}",
        perf_load(&NEWFSTATAT_TOTAL_US)
    );
    let _ = writeln!(
        &mut out,
        "  newfstatat_resolve_us {}",
        perf_load(&NEWFSTATAT_RESOLVE_US)
    );
    let _ = writeln!(
        &mut out,
        "  newfstatat_lookup_us {}",
        perf_load(&NEWFSTATAT_LOOKUP_US)
    );
    let _ = writeln!(
        &mut out,
        "  newfstatat_inode_stat_us {}",
        perf_load(&NEWFSTATAT_INODE_STAT_US)
    );
    let _ = writeln!(
        &mut out,
        "  newfstatat_copyout_us {}",
        perf_load(&NEWFSTATAT_COPYOUT_US)
    );
    let _ = writeln!(
        &mut out,
        "  avg_newfstatat_us_x100 {}",
        if newfstatat_calls == 0 {
            0
        } else {
            perf_load(&NEWFSTATAT_TOTAL_US).saturating_mul(100) / newfstatat_calls
        }
    );
    out
}

fn encode_dirent64_records(
    entries: &[(String, VfsFileType)],
    offset: usize,
    buf: &mut [u8],
) -> usize {
    let mut written = 0usize;

    for (i, (name, file_type)) in entries.iter().enumerate().skip(offset) {
        let name_bytes = name.as_bytes();
        let reclen = (19 + name_bytes.len() + 1 + 7) & !7usize;
        if written + reclen > buf.len() {
            break;
        }

        buf[written..written + 8].copy_from_slice(&((i + 1) as u64).to_le_bytes());
        let next_off = (i + 1) as i64;
        buf[written + 8..written + 16].copy_from_slice(&next_off.to_le_bytes());
        buf[written + 16..written + 18].copy_from_slice(&(reclen as u16).to_le_bytes());
        buf[written + 18] = match file_type {
            VfsFileType::Directory => 4,
            VfsFileType::Symlink => 10,
            VfsFileType::Char => 2,
            VfsFileType::Block => 6,
            VfsFileType::Fifo => 1,
            VfsFileType::Socket => 12,
            VfsFileType::Regular => 8,
            VfsFileType::Unknown => 0,
        };
        buf[written + 19..written + 19 + name_bytes.len()].copy_from_slice(name_bytes);
        buf[written + 19 + name_bytes.len()] = 0;
        for b in &mut buf[written + 19 + name_bytes.len() + 1..written + reclen] {
            *b = 0;
        }
        written += reclen;
    }

    written
}

/// Kernel-side alias for the shared filesystem statistics snapshot.
pub type StatFs64 = fs::VfsStatFs;

/// Convert a 64-bit seed into a stable `fsid_t`-style pair.
pub(crate) fn fsid_from_u64(seed: u64) -> [i32; 2] {
    [(seed as u32) as i32, (seed >> 32) as u32 as i32]
}

/// Build a zero-initialised filesystem stat snapshot.
pub(crate) fn empty_statfs(f_type: u64, bsize: u64, fsid_seed: u64, namelen: u64) -> StatFs64 {
    StatFs64 {
        f_type,
        f_bsize: bsize,
        f_blocks: 0,
        f_bfree: 0,
        f_bavail: 0,
        f_files: 0,
        f_ffree: 0,
        f_fsid: fsid_from_u64(fsid_seed),
        f_namelen: namelen,
        f_frsize: bsize,
        f_flags: 0,
        f_spare: [0; 4],
    }
}

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

const LOCK_SH: i32 = 1;
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;
const LOCK_UN: i32 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlockKind {
    Shared,
    Exclusive,
}

#[derive(Clone, Copy, Debug)]
struct FlockRecord {
    fs_id: u64,
    ino: u64,
    owner: usize,
    kind: FlockKind,
}

lazy_static! {
    static ref FLOCK_TABLE: SpinNoIrqLock<Vec<FlockRecord>> = SpinNoIrqLock::new(Vec::new());
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
    /// 目录项快照，避免遍历期间删除目录项导致位置漂移漏读。
    dirent_snapshot: Option<Vec<(String, VfsFileType)>>,
}

/// 套接字的不可变元信息。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketSpec {
    /// `socket(2)` 的 `domain` 参数，也就是协议族/地址族。
    pub family: i32,
    /// `socket(2)` 的基础 `type`，不包含 `SOCK_CLOEXEC`/`SOCK_NONBLOCK` 标志。
    pub socket_type: i32,
    /// `socket(2)` 的 `protocol` 参数。
    pub protocol: i32,
}

/// 打开文件描述，对应 Linux 的 open file description。
pub struct FileDescription {
    /// 底层具体文件对象。
    file: Arc<dyn File + Send + Sync>,
    /// 打开时确定的访问模式。
    access_mode: AccessMode,
    /// `F_GETFL` 需要保留返回、但 `F_SETFL` 不可修改的状态位。
    status_fixed_bits: i32,
    /// 套接字 fd 的固定元信息；非套接字为 `None`。
    socket_spec: Option<SocketSpec>,
    /// 共享的偏移与状态位。
    inner: SleepMutex<FileDescriptionInner>,
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
            socket_spec: None,
            inner: SleepMutex::new(FileDescriptionInner {
                offset: 0,
                status_flags,
                dirent_snapshot: None,
            }),
        }
    }

    /// 基于底层套接字对象创建一个打开文件描述，并附带固定的
    /// `family/type/protocol` 元信息。
    pub fn new_socket(
        file: Arc<dyn File + Send + Sync>,
        access_mode: AccessMode,
        status_flags: FileStatusFlags,
        status_fixed_bits: i32,
        socket_spec: SocketSpec,
    ) -> Self {
        Self {
            file,
            access_mode,
            status_fixed_bits,
            socket_spec: Some(socket_spec),
            inner: SleepMutex::new(FileDescriptionInner {
                offset: 0,
                status_flags,
                dirent_snapshot: None,
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
        self.read_result(buf).unwrap_or(0)
    }

    /// 顺序读取并推进共享文件偏移，同时保留底层 errno。
    pub fn read_result(&self, buf: UserBuffer) -> Result<usize, ERRNO> {
        if self.file.is_seekable() {
            let mut inner = self.inner.lock();
            let read_size = self.file.read_at_result(inner.offset, buf)?;
            inner.offset += read_size;
            return Ok(read_size);
        }
        if self.status_flags().contains(FileStatusFlags::NONBLOCK) {
            if let Some(pipe) = self.as_any().downcast_ref::<pipe::Pipe>() {
                return pipe.read_nonblocking(buf);
            }
        }
        self.file.read_at_result(0, buf)
    }

    /// 顺序写入并推进共享文件偏移。
    pub fn write(&self, buf: UserBuffer) -> usize {
        self.write_result(buf).unwrap_or(0)
    }

    /// 顺序写入并推进共享文件偏移，同时保留底层 errno。
    pub fn write_result(&self, buf: UserBuffer) -> Result<usize, ERRNO> {
        if self.file.is_seekable() {
            let mut inner = self.inner.lock();
            if inner.status_flags.contains(FileStatusFlags::APPEND) {
                // TODO: 当前仅保证同一 FileDescription 内的追加写顺序；跨描述竞争仍需 inode 级串行化。
                inner.offset = self.file.stat().size.max(0) as usize;
            }
            let write_size = self.file.write_at_result(inner.offset, buf)?;
            inner.offset += write_size;
            return Ok(write_size);
        }
        if self.status_flags().contains(FileStatusFlags::NONBLOCK) {
            if let Some(pipe) = self.as_any().downcast_ref::<pipe::Pipe>() {
                return pipe.write_nonblocking(buf);
            }
        }
        self.file.write_at_result(0, buf)
    }

    /// 从固定偏移读取，不影响共享文件偏移。
    pub fn read_at(&self, offset: usize, buf: UserBuffer) -> usize {
        self.file.read_at(offset, buf)
    }

    /// 从固定偏移读取到内核缓冲区，不影响共享文件偏移。
    pub fn read_bytes_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize, ERRNO> {
        self.file.read_bytes_at(offset, buf)
    }

    /// 向固定偏移写入，不影响共享文件偏移。
    pub fn write_at(&self, offset: usize, buf: UserBuffer) -> usize {
        self.file.write_at(offset, buf)
    }

    /// 将内核缓冲区顺序写入并推进共享文件偏移。
    pub fn write_bytes(&self, buf: &[u8]) -> Result<usize, ERRNO> {
        if self.file.is_seekable() {
            let mut inner = self.inner.lock();
            if inner.status_flags.contains(FileStatusFlags::APPEND) {
                inner.offset = self.file.stat().size.max(0) as usize;
            }
            let write_size = self.file.write_bytes_at(inner.offset, buf)?;
            inner.offset += write_size;
            return Ok(write_size);
        }
        self.file.write_bytes_at(0, buf)
    }

    /// 将内核缓冲区写入固定偏移，不影响共享文件偏移。
    pub fn write_bytes_at(&self, offset: usize, buf: &[u8]) -> Result<usize, ERRNO> {
        self.file.write_bytes_at(offset, buf)
    }

    /// 返回该打开文件描述是否支持位置相关 I/O（`lseek/pread/pwrite`）。
    pub fn is_seekable(&self) -> bool {
        self.file.is_seekable()
    }

    /// 调整底层文件对象的逻辑长度。
    pub fn truncate(&self, new_size: usize) -> Result<(), ERRNO> {
        self.file.truncate(new_size)
    }

    /// Reserve or deallocate file space on the underlying object.
    pub fn fallocate(&self, mode: i32, offset: usize, len: usize) -> Result<(), ERRNO> {
        self.file.fallocate(mode, offset, len)
    }

    /// 获取当前 `F_GETFL` 可见状态值。
    pub fn status_bits(&self) -> i32 {
        let inner = self.inner.lock();
        self.access_mode.bits() | self.status_fixed_bits | inner.status_flags.bits()
    }

    /// 是否为 `O_PATH` 打开的描述符。这类描述符仅引用文件在树中的位置，
    /// 不关联可操作的文件对象，套接字相关系统调用需以 `EBADF` 拒绝
    /// （对应 Linux `fdget(FMODE_PATH)` 的行为）。
    pub fn is_path(&self) -> bool {
        const O_PATH: i32 = 0x200000;
        self.status_fixed_bits & O_PATH != 0
    }

    /// 返回套接字的固定元信息；非套接字描述符返回 `None`。
    pub fn socket_spec(&self) -> Option<SocketSpec> {
        self.socket_spec
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

    /// 将底层文件对象的脏数据同步到底层存储。
    pub fn sync(&self) -> Result<(), ERRNO> {
        self.file.sync()
    }

    /// 转发 `statfs` 到底层文件对象。
    pub fn statfs(&self) -> Result<StatFs64, ERRNO> {
        self.file.statfs()
    }

    /// 返回底层文件对象是否为目录。
    pub fn is_dir(&self) -> bool {
        self.file.is_dir()
    }

    /// 返回打开路径。
    pub fn path(&self) -> Option<String> {
        self.file.path()
    }

    /// 返回该打开文件描述关联的 inode；非 inode 类型文件返回 `None`。
    pub fn as_inode(&self) -> Option<Arc<Inode>> {
        self.file.as_inode()
    }

    /// 返回该打开文件描述最终关联的稳定 inode。
    pub fn backing_inode(&self) -> Option<Arc<Inode>> {
        self.file.backing_inode()
    }

    /// Apply a BSD `flock(2)` lock to this open file description.
    pub fn flock(&self, operation: i32) -> Result<(), ERRNO> {
        let op = operation & !LOCK_NB;
        let kind = match op {
            LOCK_SH => Some(FlockKind::Shared),
            LOCK_EX => Some(FlockKind::Exclusive),
            LOCK_UN => None,
            _ => return Err(ERRNO::EINVAL),
        };
        if operation & !(LOCK_SH | LOCK_EX | LOCK_NB | LOCK_UN) != 0 {
            return Err(ERRNO::EINVAL);
        }

        let inode = self.backing_inode().ok_or(ERRNO::EINVAL)?;
        let fs_id = inode.fs_id();
        let ino = inode.ino();
        let owner = self as *const Self as usize;
        let mut table = FLOCK_TABLE.lock();

        if kind.is_none() {
            table.retain(|record| {
                !(record.fs_id == fs_id && record.ino == ino && record.owner == owner)
            });
            return Ok(());
        }

        let kind = kind.unwrap();
        let has_conflict = table.iter().any(|record| {
            if record.fs_id != fs_id || record.ino != ino || record.owner == owner {
                return false;
            }
            kind == FlockKind::Exclusive || record.kind == FlockKind::Exclusive
        });
        if has_conflict {
            return Err(ERRNO::EAGAIN);
        }

        table.retain(|record| {
            !(record.fs_id == fs_id && record.ino == ino && record.owner == owner)
        });
        table.push(FlockRecord {
            fs_id,
            ino,
            owner,
            kind,
        });
        Ok(())
    }

    /// 读取目录项并推进共享目录位置。
    pub fn getdents64(&self, buf: &mut [u8]) -> usize {
        let start_us = get_time_us();
        let mut inner = self.inner.lock();
        let read_size = if self.file.is_dir() {
            if let Some(inode) = self.as_inode() {
                if inode.prefer_native_getdents64() {
                    self.file.getdents64(inner.offset, buf)
                } else {
                    if inner.offset == 0 || inner.dirent_snapshot.is_none() {
                        let snapshot_start_us = get_time_us();
                        let snapshot = inode.ls();
                        record_dir_snapshot_perf(
                            snapshot.len(),
                            get_time_us().saturating_sub(snapshot_start_us),
                        );
                        inner.dirent_snapshot = Some(snapshot);
                    }
                    encode_dirent64_records(
                        inner.dirent_snapshot.as_ref().unwrap().as_slice(),
                        inner.offset,
                        buf,
                    )
                }
            } else {
                self.file.getdents64(inner.offset, buf)
            }
        } else {
            self.file.getdents64(inner.offset, buf)
        };
        record_getdents_perf(read_size, get_time_us().saturating_sub(start_us));
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

    /// Returns the underlying file object as `Any` for downcasting.
    pub fn as_any(&self) -> &dyn Any {
        self.file.as_any()
    }

    /// `offset` 使用有符号 64 位，以兼容 SEEK_END 处的负位移。
    pub fn seek(&self, offset: i64, whence: u8) -> Result<u64, ERRNO> {
        if !self.file.is_seekable() {
            return Err(ERRNO::ESPIPE);
        }
        let mut inner = self.inner.lock();
        let new_offset = match whence {
            0 => offset,                         // SEEK_SET
            1 => inner.offset as i64 + offset,   // SEEK_CUR
            2 => self.file.stat().size + offset, // SEEK_END
            _ => return Err(ERRNO::EINVAL),
        };
        if new_offset < 0 {
            return Err(ERRNO::EINVAL);
        }
        inner.offset = new_offset as usize;
        inner.dirent_snapshot = None;
        Ok(new_offset as u64)
    }
}

impl Drop for FileDescription {
    fn drop(&mut self) {
        let owner = self as *const Self as usize;
        FLOCK_TABLE.lock().retain(|record| record.owner != owner);
    }
}

/// trait File for all file types
pub trait File: Send + Sync + Any {
    /// Returns this file as `Any` for runtime downcasting.
    fn as_any(&self) -> &dyn Any;

    /// the file readable?
    fn readable(&self) -> bool;
    /// the file writable?
    fn writable(&self) -> bool;
    /// 从固定偏移读取数据。
    fn read_at(&self, _offset: usize, _buf: UserBuffer) -> usize {
        0
    }
    /// 从固定偏移读取数据，同时保留底层 errno。
    fn read_at_result(&self, offset: usize, buf: UserBuffer) -> Result<usize, ERRNO> {
        Ok(self.read_at(offset, buf))
    }
    /// 从固定偏移读取到内核缓冲区。
    fn read_bytes_at(&self, _offset: usize, _buf: &mut [u8]) -> Result<usize, ERRNO> {
        Err(ERRNO::EOPNOTSUPP)
    }
    /// 向固定偏移写入数据。
    fn write_at(&self, _offset: usize, _buf: UserBuffer) -> usize {
        0
    }
    /// 向固定偏移写入数据，同时保留底层 errno。
    fn write_at_result(&self, offset: usize, buf: UserBuffer) -> Result<usize, ERRNO> {
        Ok(self.write_at(offset, buf))
    }
    /// 从内核缓冲区向固定偏移写入。
    fn write_bytes_at(&self, _offset: usize, _buf: &[u8]) -> Result<usize, ERRNO> {
        Err(ERRNO::EOPNOTSUPP)
    }
    /// 调整文件逻辑长度。
    fn truncate(&self, _new_size: usize) -> Result<(), ERRNO> {
        Err(ERRNO::EOPNOTSUPP)
    }
    /// Reserve or deallocate file space without forcing eager allocation.
    fn fallocate(&self, _mode: i32, _offset: usize, _len: usize) -> Result<(), ERRNO> {
        Err(ERRNO::EOPNOTSUPP)
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
    /// get filesystem metadata
    fn statfs(&self) -> Result<StatFs64, ERRNO> {
        Err(ERRNO::ENOSYS)
    }
    /// 将该文件对象的脏数据同步到底层存储。
    fn sync(&self) -> Result<(), ERRNO> {
        Ok(())
    }
    /// Returns the canonical path used when this file was opened, if any.
    fn path(&self) -> Option<String> {
        None
    }
    /// 返回该文件对象关联的 inode；非 inode 类型文件返回 `None`。
    fn as_inode(&self) -> Option<Arc<Inode>> {
        None
    }
    /// Change the file mode bits, if supported by this file type.
    fn chmod(&self, _mode: u32) -> Result<(), FS_ERRNO> {
        Err(FS_ERRNO::EOPNOTSUPP)
    }

    /// 返回该文件对象最终关联的稳定 inode；不支持文件映射的对象返回 `None`。
    fn backing_inode(&self) -> Option<Arc<Inode>> {
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

impl Pod for StatFs64 {}

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
    canonicalize, do_bind_mount, do_mount, do_move_mount, do_umount, init_dev, init_procfs,
    init_rootfs, init_sysfs, inode_stat, linkat, linkat_with_flags, list_apps, lookup_inode,
    lookup_inode_follow, lookup_inode_follow_with_path, lookup_inode_from, mkdir_at,
    mkdir_at_with_inode, mount_cgroup2, mount_device, mount_is_readonly, mount_sysfs, mount_tmpfs,
    open_file, open_file_at, open_file_at_with_status, remount_path, rename_at, symlinkat,
    unlinkat, OSInode, OpenFlags, AT_EMPTY_PATH, AT_FDCWD, AT_REMOVEDIR, AT_SYMLINK_FOLLOW,
    AT_SYMLINK_NOFOLLOW,
};
pub use pipe::{make_pipe, Pipe};
pub use stdio::new_stdio_files;
pub use tty::{
    console_receive, console_tty, Termios, TtyCore, TtyDeviceKind, TtyDeviceNode, TtyFile, WinSize,
    CONSOLE_TTY,
};

/// Initialize the filesystem, including rootfs and devfs.
pub fn init() -> Result<(), ERRNO> {
    #[cfg(all(feature = "ext4", feature = "io_perf_counters"))]
    ::fs::ext4::set_perf_time_source(get_time_us);
    init_rootfs()?; // Virtual rootfs for booting system; meanwhile mount a real fs (e.g. ext4) to "/".
    init_dev(); // Initialize devfs, which provides device files (e.g. /dev/vda, /dev/vdb) for block devices.
    init_sysfs(); // Initialize sysfs for /sys/class/net entries.
    init_procfs(); // Initialize procfs for /proc entries.
    Ok(())
}
