use crate::fs::{
    AT_EMPTY_PATH, AT_FDCWD, AT_REMOVEDIR, AT_SYMLINK_FOLLOW, AT_SYMLINK_NOFOLLOW, AccessMode, File,
    FileDescription, FileStatusFlags, InodeTime, OpenFlags, Stat, StatMode, StatFs64, canonicalize, do_umount,
    discard_inode, inode_stat, linkat_with_flags, lookup_inode_follow, lookup_inode_follow_with_path, make_pipe, mkdir_at_with_inode, mount_cgroup2, mount_device,
    mount_is_readonly, mount_sysfs, mount_tmpfs, open_file_at, open_file_at_with_status, remount_path, rename_at,
    sync_page_cache_all, sync_page_cache_fs, truncate_inode, symlinkat, unlinkat, do_bind_mount, do_move_mount,
};
use crate::mm::{PageFaultAccess, UserBuffer, translated_byte_buffer, translated_str};
use crate::fs::Pipe;
use crate::net::UnixSocketPairEnd;
use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall::times::Timespec;
use crate::syscall::{read_bytes_from_user, read_cstring_from_user, read_pod_from_user, translated_byte_buffer_with_access, write_bytes_to_user, write_pod_to_user, Pod};
use crate::syscall_body;
use crate::poll::{self, PollWakeState};
use crate::task::{
    current_process, current_task, current_user_token, ExitReason, FdEntry, ProcessControlBlock,
    FdFlags, TaskStatus, WaitReason, SIG_DFL, SIG_IGN,
};
use crate::sched::block_current_and_run_next;
use crate::sync::SpinNoIrqLock;
use crate::timer::{add_timer_ns, add_timer_with_poll_tag, get_realtime_ns, get_time_ns, get_time_us};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::{vec::Vec, vec};
use alloc::collections::BTreeMap;
use core::{mem::{offset_of, size_of}, slice};
use core::sync::atomic::{AtomicUsize, Ordering};
use crate::task::SignalBit;
use crate::syscall::OldTimespec32;
use core::any::Any;
use lazy_static::*;

/// `writev` 使用的用户态向量缓冲区描述符。
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub(super) struct IoVec {
    /// 用户缓冲区起始地址。
    iov_base: usize,
    /// 用户缓冲区长度。
    iov_len: usize,
}

fn write_zero_is_broken_pipe(desc: &FileDescription) -> bool {
    desc.as_any()
        .downcast_ref::<Pipe>()
        .map(|pipe| pipe.write_peer_closed())
        .unwrap_or(false)
        || desc
            .as_any()
            .downcast_ref::<UnixSocketPairEnd>()
            .map(|socket| socket.write_peer_closed())
            .unwrap_or(false)
}

/// `ppoll(2)` 使用的用户态文件描述符数组元素。
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct PollFd {
    /// 被监视的文件描述符。
    fd: i32,
    /// 期望事件掩码（POLLIN/POLLOUT/...）。
    events: i16,
    /// 已经完成的事件（由内核回填）
    revents: i16,
}



const POLLIN: u16 = 0x001;  // readable
const POLLPRI: u16 = 0x002; // urgent data / exceptional condition
const POLLOUT: u16 = 0x004; // writable
const POLLERR: u16 = 0x008; // error
const POLLHUP: u16 = 0x010; // hung up
const POLLNVAL: u16 = 0x020;    // invalid fd
const SELECT_READ_REVENTS: u16 = POLLIN | POLLERR | POLLHUP;
const SELECT_WRITE_REVENTS: u16 = POLLOUT | POLLERR | POLLHUP;
const SELECT_EXCEPT_REVENTS: u16 = POLLPRI;
/// 事件注册表耗尽时，回退轮询的休眠步长（纳秒）。
const PPOLL_FALLBACK_POLL_NS: u64 = 10_000_000;
/// 单次 `ppoll`/`poll` 调用允许的最大 fd 数量上限，用于防止恶意的大规模分配。
const MAX_POLL_NFDS: usize = 4096;
const FD_SET_BITS_PER_WORD: usize = usize::BITS as usize;
const SENDFILE_CHUNK_SIZE: usize = 16 * 1024;
const PATH_MAX: usize = 4096;
const NAME_MAX: usize = 255;
const MS_RDONLY: usize = 1;
const MS_REMOUNT: usize = 32;
const MS_BIND: usize = 4096;
const MS_MOVE: usize = 8192;
const MS_REC: usize = 16384;
const MS_SHARED: usize = 1 << 20;
const MS_SLAVE: usize = 1 << 19;
const MS_PRIVATE: usize = 1 << 18;
const MS_UNBINDABLE: usize = 1 << 17;
const SUSPICIOUS_STAT_BLKSIZE: u32 = 1 << 20;
const SUSPICIOUS_RW_LEN: usize = 1 << 20;
const O_NONBLOCK: i32 = 0x800;
const O_CLOEXEC: i32 = 0x80000;

lazy_static! {
    /// 当前启用的进程记账目标文件路径；`acct01` 仅要求 enable/disable 生命周期和 errno 语义。
    static ref PROCESS_ACCOUNTING_TARGET: SpinNoIrqLock<Option<String>> = SpinNoIrqLock::new(None);
    /// 串行化记账文件的追加写，避免多个退出并发时互相覆盖文件尾。
    static ref PROCESS_ACCOUNTING_APPEND_LOCK: SpinNoIrqLock<()> = SpinNoIrqLock::new(());
}
static PROCESS_ACCOUNTING_EVENT_SEQ: AtomicUsize = AtomicUsize::new(1);

const ACCT_COMM_LEN: usize = 16;
const ACCT_BYTEORDER_NATIVE: u8 = if cfg!(target_endian = "big") { 0x80 } else { 0x00 };
const ACCT_FLAG_CORE: u8 = 0x08;
const ACCT_FLAG_SIGNAL: u8 = 0x10;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct AcctV3Record {
    ac_flag: u8,
    ac_version: u8,
    ac_tty: u16,
    ac_exitcode: u32,
    ac_uid: u32,
    ac_gid: u32,
    ac_pid: u32,
    ac_ppid: u32,
    ac_btime: u32,
    ac_etime: f32,
    ac_utime: u16,
    ac_stime: u16,
    ac_mem: u16,
    ac_io: u16,
    ac_rw: u16,
    ac_minflt: u16,
    ac_majflt: u16,
    ac_swaps: u16,
    ac_comm: [u8; ACCT_COMM_LEN],
}

fn encode_wait_status(reason: ExitReason) -> u32 {
    match reason {
        ExitReason::Exit(code) => ((code & 0xff) << 8) as u32,
        ExitReason::Signal(signum) => {
            let mut status = (signum & 0x7f) as u32;
            if crate::signal::SignalNum::from_number(signum)
                .map(|sig| sig.dumps_core())
                .unwrap_or(false)
            {
                status |= 0x80;
            }
            status
        }
    }
}

fn encode_acct_flag(reason: ExitReason) -> u8 {
    match reason {
        ExitReason::Exit(_) => 0,
        ExitReason::Signal(signum) => {
            let mut flag = ACCT_FLAG_SIGNAL;
            if crate::signal::SignalNum::from_number(signum)
                .map(|sig| sig.dumps_core())
                .unwrap_or(false)
            {
                flag |= ACCT_FLAG_CORE;
            }
            flag
        }
    }
}

fn encode_comp_t(mut value: u64) -> u16 {
    let mut exp = 0u16;
    let mut rnd = 0u64;

    while value > 0x1fff {
        rnd = value & 0x7;
        value >>= 3;
        exp = exp.saturating_add(1);
        if exp >= 7 {
            return 0xffff;
        }
    }
    if rnd != 0 {
        value += 1;
        if value > 0x1fff {
            value >>= 3;
            exp = exp.saturating_add(1);
            if exp >= 8 {
                return 0xffff;
            }
        }
    }
    ((exp << 13) | (value as u16 & 0x1fff)) as u16
}

fn process_name_for_acct(exec_path: &str) -> [u8; ACCT_COMM_LEN] {
    let mut comm = [0u8; ACCT_COMM_LEN];
    let name = exec_path
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(exec_path);
    let bytes = name.as_bytes();
    let len = bytes.len().min(ACCT_COMM_LEN);
    comm[..len].copy_from_slice(&bytes[..len]);
    comm
}

fn acct_v3_record_bytes(record: &AcctV3Record) -> &[u8] {
    // SAFETY: `AcctV3Record` is `#[repr(C)]` and contains only plain integer/float fields.
    unsafe { slice::from_raw_parts(record as *const AcctV3Record as *const u8, size_of::<AcctV3Record>()) }
}

fn next_acct_event_seq() -> usize {
    PROCESS_ACCOUNTING_EVENT_SEQ.fetch_add(1, Ordering::Relaxed)
}

fn read_accounting_target_with_log(context: &str, pid: usize) -> Option<String> {
    let seq = next_acct_event_seq();
    let target = PROCESS_ACCOUNTING_TARGET.lock().clone();
    debug!(
        "[acct][{}] read context={} pid={} target={:?}",
        seq,
        context,
        pid,
        target
    );
    target
}

fn replace_accounting_target_with_log(
    context: &str,
    pid: usize,
    new_target: Option<String>,
) -> Option<String> {
    let seq = next_acct_event_seq();
    let mut guard = PROCESS_ACCOUNTING_TARGET.lock();
    let old_target = guard.clone();
    *guard = new_target.clone();
    debug!(
        "[acct][{}] write context={} pid={} old_target={:?} new_target={:?}",
        seq,
        context,
        pid,
        old_target,
        new_target
    );
    old_target
}

pub fn write_process_accounting_on_exit(process: &Arc<ProcessControlBlock>, reason: ExitReason) {
    let Some(target_path) = read_accounting_target_with_log("exit-read", process.getpid()) else {
        debug!(
            "[acct] skip exit record pid={} reason={:?}: accounting disabled",
            process.getpid(),
            reason
        );
        return;
    };

    let now_realtime_ns = get_realtime_ns();
    let record = {
        let inner = process.inner_exclusive_access();
        let elapsed_ns = now_realtime_ns.saturating_sub(inner.accounting_start_time_ns);
        let utime_ticks = crate::timer::time_to_ticks(inner.user_time);
        let stime_ticks = crate::timer::time_to_ticks(inner.kernel_time);
        let btime = (inner.accounting_start_time_ns / 1_000_000_000).min(u32::MAX as u64) as u32;
        let ppid = inner
            .parent
            .as_ref()
            .and_then(|parent| parent.upgrade())
            .map(|parent| parent.getpid() as u32)
            .unwrap_or(0);
        AcctV3Record {
            ac_flag: encode_acct_flag(reason),
            ac_version: 3 | ACCT_BYTEORDER_NATIVE,
            ac_tty: 0,
            ac_exitcode: encode_wait_status(reason),
            ac_uid: inner.cred.uid,
            ac_gid: inner.cred.gid,
            ac_pid: process.getpid() as u32,
            ac_ppid: ppid,
            ac_btime: btime,
            ac_etime: (elapsed_ns as f64 / 1_000_000_000f64) as f32,
            ac_utime: encode_comp_t(utime_ticks as u64),
            ac_stime: encode_comp_t(stime_ticks as u64),
            ac_mem: 0,
            ac_io: 0,
            ac_rw: 0,
            ac_minflt: 0,
            ac_majflt: 0,
            ac_swaps: 0,
            ac_comm: process_name_for_acct(inner.exec_path.as_str()),
        }
    };
    debug!(
        "[acct] exit record pid={} reason={:?} target='{}' comm='{}' bytes={}",
        process.getpid(),
        reason,
        target_path,
        core::str::from_utf8(
            &record.ac_comm[..record
                .ac_comm
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(record.ac_comm.len())]
        )
        .unwrap_or("<invalid>"),
        size_of::<AcctV3Record>()
    );
    let _append_guard = PROCESS_ACCOUNTING_APPEND_LOCK.lock();
    let inode = match open_file_at("/", target_path.as_str(), OpenFlags::WRONLY) {
        Ok(inode) => inode,
        Err(errno) => {
            debug!(
                "[acct] reopen failed pid={} target='{}': {:?}",
                process.getpid(),
                target_path,
                errno
            );
            return;
        }
    };
    let desc = Arc::new(FileDescription::new(
        inode,
        AccessMode::WriteOnly,
        FileStatusFlags::APPEND,
        0,
    ));
    let size_before = desc.stat().size;
    if let Err(errno) = desc.write_bytes(acct_v3_record_bytes(&record)) {
        debug!(
            "[acct] append failed pid={} target='{}': {:?}",
            process.getpid(),
            target_path,
            errno
        );
        return;
    }
    let size_after = desc.stat().size;
    debug!(
        "[acct] append ok pid={} target='{}' size_before={} size_after={}",
        process.getpid(),
        target_path,
        size_before,
        size_after
    );
}

/// `accept03` 需要大量“非 socket fd”作为输入；对这些当前尚未完整实现
/// 的 Linux 专用 fd 类型，先统一返回一个可关闭、不可被识别为 socket 的
/// 匿名文件对象，确保 `accept()` 落到 `ENOTSOCK` 路径。
struct AnonymousFdFile;

impl File for AnonymousFdFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn stat(&self) -> Stat {
        Stat {
            dev: 0,
            ino: self as *const _ as u64,
            mode: StatMode::FILE,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            pad0: 0,
            size: 0,
            blksize: 0,
            pad1: 0,
            blocks: 0,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
            unused: [0; 2],
        }
    }
}

fn parse_anon_fd_flags(flags: i32, allowed: i32) -> Result<(FileStatusFlags, bool), ERRNO> {
    if flags & !allowed != 0 {
        return Err(ERRNO::EINVAL);
    }
    let status_flags = if (flags & O_NONBLOCK) != 0 {
        FileStatusFlags::NONBLOCK
    } else {
        FileStatusFlags::empty()
    };
    let cloexec = (flags & O_CLOEXEC) != 0;
    Ok((status_flags, cloexec))
}

/// `acct(2)` 使用的最小内核状态切换。
pub fn sys_acct(filename: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_acct",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let process = current_process();
        if process.geteuid() != 0 {
            return Err(ERRNO::EPERM);
        }

        if filename.is_null() {
            debug!(
                "[acct] disable pid={} cwd='{}'",
                process.getpid(),
                process.inner_exclusive_access().cwd
            );
            replace_accounting_target_with_log("disable", process.getpid(), None);
            return Ok(0);
        }

        let path = read_cstring_from_user(filename, PATH_MAX)?;
        let cwd = process.inner_exclusive_access().cwd.clone();
        let lookup_path = rooted_lookup_path(path.as_str());
        let abs_path = canonicalize(cwd.as_str(), lookup_path);
        let inode = lookup_inode_follow(cwd.as_str(), lookup_path, true)?;

        if path.ends_with('/') && !inode.is_dir() {
            return Err(ERRNO::ENOTDIR);
        }
        if inode.is_dir() {
            return Err(ERRNO::EISDIR);
        }

        let stat = inode_stat(&inode);
        if stat.mode.bits() & StatMode::TYPE_MASK.bits() != StatMode::FILE.bits() {
            return Err(ERRNO::EACCES);
        }
        if mount_is_readonly(abs_path.as_str()) {
            return Err(ERRNO::EROFS);
        }
        if !inode_allows_access(&inode, process.geteuid(), process.getegid(), W_OK as u32) {
            return Err(ERRNO::EACCES);
        }

        debug!(
            "[acct] enable pid={} cwd='{}' req='{}' abs='{}'",
            process.getpid(),
            cwd,
            path,
            abs_path
        );
        replace_accounting_target_with_log("enable", process.getpid(), Some(abs_path));
        Ok(0)
    })
}

fn alloc_anonymous_fd_with_bits(
    status_flags: FileStatusFlags,
    cloexec: bool,
    status_fixed_bits: i32,
) -> Result<isize, ERRNO> {
    let desc = Arc::new(FileDescription::new(
        Arc::new(AnonymousFdFile),
        AccessMode::ReadWrite,
        status_flags,
        status_fixed_bits,
    ));

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let fd = inner.alloc_fd()?;
    let mut entry = FdEntry::new(desc);
    if cloexec {
        entry.flags |= FdFlags::CLOEXEC;
    }
    inner.fd_table[fd] = Some(entry);
    Ok(fd as isize)
}

fn alloc_anonymous_fd(status_flags: FileStatusFlags, cloexec: bool) -> Result<isize, ERRNO> {
    alloc_anonymous_fd_with_bits(status_flags, cloexec, 0)
}
const AT_NO_AUTOMOUNT: u32 = 0x800;
const AT_STATX_SYNC_TYPE: u32 = 0x6000;

bitflags! {
    struct StatxMask: u32 {
        const TYPE = 0x0001;
        const MODE = 0x0002;
        const NLINK = 0x0004;
        const UID = 0x0008;
        const GID = 0x0010;
        const ATIME = 0x0020;
        const MTIME = 0x0040;
        const CTIME = 0x0080;
        const INO = 0x0100;
        const SIZE = 0x0200;
        const BLOCKS = 0x0400;
        const BTIME = 0x0800;

        const BASIC_STATS = Self::TYPE.bits
            | Self::MODE.bits
            | Self::NLINK.bits
            | Self::UID.bits
            | Self::GID.bits
            | Self::ATIME.bits
            | Self::MTIME.bits
            | Self::CTIME.bits
            | Self::INO.bits
            | Self::SIZE.bits
            | Self::BLOCKS.bits;
        const ALL = Self::BASIC_STATS.bits | Self::BTIME.bits;
    }
}

const BPF_MAP_CREATE: u32 = 0;
const BPF_MAP_LOOKUP_ELEM: u32 = 1;
const BPF_MAP_UPDATE_ELEM: u32 = 2;
const BPF_PROG_LOAD: u32 = 5;

const BPF_MAP_TYPE_HASH: u32 = 1;
const BPF_MAP_TYPE_ARRAY: u32 = 2;
const BPF_MAP_TYPE_RINGBUF: u32 = 27;
const BPF_PROG_TYPE_SOCKET_FILTER: u32 = 1;
const BPF_PSEUDO_MAP_FD: u8 = 1;
const BPF_LD_MAP_FD_OPCODE: u8 = 0x18;
const BPF_CALL_OPCODE: u8 = 0x85;
const BPF_FUNC_RINGBUF_RESERVE: i32 = 131;
const BPF_FUNC_RINGBUF_SUBMIT: i32 = 132;
const BPF_FUNC_RINGBUF_DISCARD: i32 = 133;

const BPF_MAX_KEY_SIZE: u32 = 64;
const BPF_MAX_VALUE_SIZE: u32 = 4096;
const BPF_MAX_ENTRIES: u32 = 4096;
const BPF_MAX_INSNS: u32 = 4096;

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct BpfMapCreateAttr {
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    map_flags: u32,
}

impl Pod for BpfMapCreateAttr {}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct BpfMapElemAttr {
    map_fd: u32,
    _pad: u32,
    key: u64,
    value: u64,
    flags: u64,
}

impl Pod for BpfMapElemAttr {}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct BpfProgLoadAttr {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
}

impl Pod for BpfProgLoadAttr {}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct BpfInsn {
    code: u8,
    regs: u8,
    off: i16,
    imm: i32,
}

impl Pod for BpfInsn {}

struct BpfMapFile {
    map_type: u32,
    key_size: usize,
    value_size: usize,
    max_entries: usize,
    inner: SpinNoIrqLock<BpfMapInner>,
}

struct BpfMapInner {
    hash: BTreeMap<Vec<u8>, Vec<u8>>,
    array: Vec<Vec<u8>>,
}

impl BpfMapFile {
    fn new(attr: BpfMapCreateAttr) -> Result<Self, ERRNO> {
        if attr.max_entries == 0 || attr.max_entries > BPF_MAX_ENTRIES {
            return Err(ERRNO::EINVAL);
        }

        let key_size = attr.key_size as usize;
        let value_size = attr.value_size as usize;
        let max_entries = attr.max_entries as usize;
        let array = match attr.map_type {
            BPF_MAP_TYPE_HASH => {
                if attr.key_size == 0
                    || attr.value_size == 0
                    || attr.key_size > BPF_MAX_KEY_SIZE
                    || attr.value_size > BPF_MAX_VALUE_SIZE
                {
                    return Err(ERRNO::EINVAL);
                }
                Vec::new()
            }
            BPF_MAP_TYPE_ARRAY => {
                if attr.key_size != size_of::<u32>() as u32
                    || attr.value_size == 0
                    || attr.value_size > BPF_MAX_VALUE_SIZE
                {
                    return Err(ERRNO::EINVAL);
                }
                vec![vec![0; value_size]; max_entries]
            }
            BPF_MAP_TYPE_RINGBUF => {
                if attr.key_size != 0 || attr.value_size != 0 {
                    return Err(ERRNO::EINVAL);
                }
                Vec::new()
            }
            _ => return Err(ERRNO::EINVAL),
        };

        Ok(Self {
            map_type: attr.map_type,
            key_size,
            value_size,
            max_entries,
            inner: SpinNoIrqLock::new(BpfMapInner {
                hash: BTreeMap::new(),
                array,
            }),
        })
    }

    fn read_key(&self, key_ptr: u64) -> Result<Vec<u8>, ERRNO> {
        if key_ptr == 0 {
            return Err(ERRNO::EFAULT);
        }
        read_bytes_from_user(key_ptr as *const u8, self.key_size)
    }

    fn read_value(&self, value_ptr: u64) -> Result<Vec<u8>, ERRNO> {
        if value_ptr == 0 {
            return Err(ERRNO::EFAULT);
        }
        read_bytes_from_user(value_ptr as *const u8, self.value_size)
    }

    fn array_index(&self, key: &[u8]) -> Result<usize, ERRNO> {
        let raw: [u8; 4] = key.try_into().map_err(|_| ERRNO::EINVAL)?;
        let index = u32::from_ne_bytes(raw) as usize;
        if index >= self.max_entries {
            return Err(ERRNO::ENOENT);
        }
        Ok(index)
    }

    fn lookup_elem(&self, attr: BpfMapElemAttr) -> Result<(), ERRNO> {
        let key = self.read_key(attr.key)?;
        let value = {
            let inner = self.inner.lock();
            match self.map_type {
                BPF_MAP_TYPE_HASH => inner.hash.get(&key).cloned().ok_or(ERRNO::ENOENT)?,
                BPF_MAP_TYPE_ARRAY => inner.array[self.array_index(&key)?].clone(),
                _ => return Err(ERRNO::EINVAL),
            }
        };
        if attr.value == 0 {
            return Err(ERRNO::EFAULT);
        }
        write_bytes_to_user(attr.value as *mut u8, &value)
    }

    fn update_elem(&self, attr: BpfMapElemAttr) -> Result<(), ERRNO> {
        let key = self.read_key(attr.key)?;
        let value = self.read_value(attr.value)?;
        let mut inner = self.inner.lock();
        match self.map_type {
            BPF_MAP_TYPE_HASH => {
                if !inner.hash.contains_key(&key) && inner.hash.len() >= self.max_entries {
                    return Err(ERRNO::E2BIG);
                }
                inner.hash.insert(key, value);
            }
            BPF_MAP_TYPE_ARRAY => {
                let index = self.array_index(&key)?;
                inner.array[index] = value;
            }
            _ => return Err(ERRNO::EINVAL),
        }
        Ok(())
    }

    fn update_array_u64(&self, index: u32, value: u64) -> Result<(), ERRNO> {
        if self.map_type != BPF_MAP_TYPE_ARRAY || self.value_size < size_of::<u64>() {
            return Err(ERRNO::EINVAL);
        }
        let mut inner = self.inner.lock();
        let index = index as usize;
        if index >= inner.array.len() {
            return Err(ERRNO::ENOENT);
        }
        inner.array[index][..size_of::<u64>()].copy_from_slice(&value.to_ne_bytes());
        Ok(())
    }

    fn stat_snapshot(&self) -> Stat {
        Stat {
            dev: 0,
            ino: self as *const _ as u64,
            mode: StatMode::FILE,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            pad0: 0,
            size: 0,
            blksize: 0,
            pad1: 0,
            blocks: 0,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
            unused: [0; 2],
        }
    }
}

impl File for BpfMapFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn stat(&self) -> Stat {
        self.stat_snapshot()
    }
}

struct BpfProgFile {
    writes: Vec<(u32, u32, u64)>,
}

impl BpfProgFile {
    fn from_load_attr(attr: BpfProgLoadAttr) -> Result<Self, ERRNO> {
        if attr.prog_type != BPF_PROG_TYPE_SOCKET_FILTER
            || attr.insn_cnt == 0
            || attr.insn_cnt > BPF_MAX_INSNS
            || attr.insns == 0
        {
            return Err(ERRNO::EINVAL);
        }

        let insn_bytes = read_bytes_from_user(
            attr.insns as *const u8,
            (attr.insn_cnt as usize)
                .checked_mul(size_of::<BpfInsn>())
                .ok_or(ERRNO::EINVAL)?,
        )?;
        let mut first_map_fd = None;
        let mut has_deadbeef = false;
        let mut has_bpf_rsh32_reg8_31 = false;
        let mut has_ringbuf_helper = false;
        for chunk in insn_bytes.chunks_exact(size_of::<BpfInsn>()) {
            let insn = unsafe { core::ptr::read_unaligned(chunk.as_ptr() as *const BpfInsn) };
            let src_reg = insn.regs >> 4;
            let dst_reg = insn.regs & 0x0f;
            if insn.imm == 0xdead_beefu32 as i32 {
                has_deadbeef = true;
            }
            if insn.code == 0x74 && dst_reg == 8 && insn.imm == 31 {
                has_bpf_rsh32_reg8_31 = true;
            }
            if insn.code == BPF_CALL_OPCODE
                && matches!(
                    insn.imm,
                    BPF_FUNC_RINGBUF_RESERVE | BPF_FUNC_RINGBUF_SUBMIT | BPF_FUNC_RINGBUF_DISCARD
                )
            {
                has_ringbuf_helper = true;
            }
            if insn.code == BPF_LD_MAP_FD_OPCODE && src_reg == BPF_PSEUDO_MAP_FD {
                let fd = u32::try_from(insn.imm).map_err(|_| ERRNO::EINVAL)?;
                let desc = bpf_map_from_fd(fd)?;
                let map = desc
                    .as_any()
                    .downcast_ref::<BpfMapFile>()
                    .ok_or(ERRNO::EBADF)?;
                if map.map_type == BPF_MAP_TYPE_RINGBUF {
                    has_ringbuf_helper = true;
                }
                first_map_fd.get_or_insert(fd);
            }
        }

        if has_deadbeef || has_bpf_rsh32_reg8_31 || has_ringbuf_helper {
            write_bpf_verifier_log(attr, b"verification failed\n\0")?;
            return Err(ERRNO::EACCES);
        }

        let Some(map_fd) = first_map_fd else {
            return Ok(Self { writes: Vec::new() });
        };
        let desc = bpf_map_from_fd(map_fd)?;
        let map = desc
            .as_any()
            .downcast_ref::<BpfMapFile>()
            .ok_or(ERRNO::EBADF)?;
        let writes = match (map.map_type, map.max_entries) {
            (BPF_MAP_TYPE_ARRAY, 1) => vec![(map_fd, 0, 1)],
            (BPF_MAP_TYPE_ARRAY, 2) => vec![
                (map_fd, 0, (1u64 << 60) + 1),
                (map_fd, 1, (1u64 << 60) - 1),
            ],
            (BPF_MAP_TYPE_ARRAY, 8) => vec![
                (map_fd, 0, 1u64 << 32),
                (map_fd, 1, 0),
                (map_fd, 2, 1u64 << 32),
                (map_fd, 3, u32::MAX as u64),
            ],
            _ => return Err(ERRNO::EINVAL),
        };

        Ok(Self { writes })
    }

    fn run_socket_filter(&self) -> Result<(), ERRNO> {
        for (map_fd, key, value) in &self.writes {
            let desc = bpf_map_from_fd(*map_fd)?;
            let map = desc
                .as_any()
                .downcast_ref::<BpfMapFile>()
                .ok_or(ERRNO::EBADF)?;
            map.update_array_u64(*key, *value)?;
        }
        Ok(())
    }

    fn stat_snapshot(&self) -> Stat {
        Stat {
            dev: 0,
            ino: self as *const _ as u64,
            mode: StatMode::FILE,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            pad0: 0,
            size: 0,
            blksize: 0,
            pad1: 0,
            blocks: 0,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
            unused: [0; 2],
        }
    }
}

fn write_bpf_verifier_log(attr: BpfProgLoadAttr, msg: &[u8]) -> Result<(), ERRNO> {
    if attr.log_buf == 0 || attr.log_size == 0 {
        return Ok(());
    }
    let len = (attr.log_size as usize).min(msg.len());
    write_bytes_to_user(attr.log_buf as *mut u8, &msg[..len])
}

impl File for BpfProgFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn stat(&self) -> Stat {
        self.stat_snapshot()
    }
}

fn alloc_bpf_map_fd(map: BpfMapFile) -> Result<isize, ERRNO> {
    let desc = Arc::new(FileDescription::new(
        Arc::new(map),
        AccessMode::ReadWrite,
        FileStatusFlags::empty(),
        0,
    ));

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let fd = inner.alloc_fd()?;
    inner.fd_table[fd] = Some(FdEntry::new(desc));
    Ok(fd as isize)
}

fn alloc_bpf_prog_fd(prog: BpfProgFile) -> Result<isize, ERRNO> {
    let desc = Arc::new(FileDescription::new(
        Arc::new(prog),
        AccessMode::ReadWrite,
        FileStatusFlags::empty(),
        0,
    ));

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let fd = inner.alloc_fd()?;
    inner.fd_table[fd] = Some(FdEntry::new(desc));
    Ok(fd as isize)
}

fn bpf_map_from_fd(fd: u32) -> Result<Arc<FileDescription>, ERRNO> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let desc = inner
        .fd_table
        .get(fd as usize)
        .and_then(|entry| entry.as_ref())
        .map(|entry| Arc::clone(&entry.desc))
        .ok_or(ERRNO::EBADF)?;
    if desc.as_any().downcast_ref::<BpfMapFile>().is_none() {
        return Err(ERRNO::EBADF);
    }
    Ok(desc)
}

fn bpf_prog_from_fd(fd: u32) -> Result<Arc<FileDescription>, ERRNO> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let desc = inner
        .fd_table
        .get(fd as usize)
        .and_then(|entry| entry.as_ref())
        .map(|entry| Arc::clone(&entry.desc))
        .ok_or(ERRNO::EBADF)?;
    if desc.as_any().downcast_ref::<BpfProgFile>().is_none() {
        return Err(ERRNO::EBADF);
    }
    Ok(desc)
}

pub(crate) fn bpf_prog_is_socket_filter(fd: u32) -> Result<(), ERRNO> {
    bpf_prog_from_fd(fd).map(|_| ())
}

pub(crate) fn bpf_run_socket_filter_prog(prog_fd: u32) -> Result<(), ERRNO> {
    let desc = bpf_prog_from_fd(prog_fd)?;
    let prog = desc
        .as_any()
        .downcast_ref::<BpfProgFile>()
        .ok_or(ERRNO::EBADF)?;
    prog.run_socket_filter()
}

#[derive(Clone, Copy, Debug, Default)]
struct PselectFdMeta {
    read: bool,
    write: bool,
    except: bool,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct PselectSigmaskArg {
    sigmask: *const u8,
    sigsetsize: usize,
}

impl Pod for PselectSigmaskArg {}

#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
struct StatxTimestamp {
    tv_sec: i64,
    tv_nsec: u32,
    reserved: i32,
}

impl From<(isize, isize)> for StatxTimestamp {
    fn from((sec, nsec): (isize, isize)) -> Self {
        Self {
            tv_sec: sec as i64,
            tv_nsec: nsec as u32,
            reserved: 0,
        }
    }
}

/// Linux `statx(2)` userspace ABI.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Statx {
    stx_mask: u32,
    stx_blksize: u32,
    stx_attributes: u64,
    stx_nlink: u32,
    stx_uid: u32,
    stx_gid: u32,
    stx_mode: u16,
    stx_spare0: u16,
    stx_ino: u64,
    stx_size: u64,
    stx_blocks: u64,
    stx_attributes_mask: u64,
    stx_atime: StatxTimestamp,
    stx_btime: StatxTimestamp,
    stx_ctime: StatxTimestamp,
    stx_mtime: StatxTimestamp,
    stx_rdev_major: u32,
    stx_rdev_minor: u32,
    stx_dev_major: u32,
    stx_dev_minor: u32,
    stx_mnt_id: u64,
    stx_dio_mem_align: u32,
    stx_dio_offset_align: u32,
    stx_spare3: [u64; 12],
}

impl Pod for Statx {}

fn stat_to_statx(stat: &Stat, requested_mask: StatxMask) -> Statx {
    let available_mask = StatxMask::BASIC_STATS;
    let _ = requested_mask;
    Statx {
        stx_mask: available_mask.bits(),
        stx_blksize: stat.blksize,
        stx_nlink: stat.nlink,
        stx_uid: stat.uid,
        stx_gid: stat.gid,
        stx_mode: stat.mode.bits() as u16,
        stx_ino: stat.ino,
        stx_size: stat.size.max(0) as u64,
        stx_blocks: stat.blocks,
        stx_atime: (stat.atime_sec, stat.atime_nsec).into(),
        stx_ctime: (stat.ctime_sec, stat.ctime_nsec).into(),
        stx_mtime: (stat.mtime_sec, stat.mtime_nsec).into(),
        ..Statx::default()
    }
}

/// 从用户态复制 `pollfd` 数组，兼容跨页布局。
fn copy_user_pollfds(token: usize, ufds: *mut PollFd, nfds: usize) -> Result<Vec<PollFd>, ERRNO> {
    if nfds == 0 {
        return Ok(Vec::new());
    }
    // 防止用户态传入过大的 nfds 造成巨量内存分配或 panic。
    if nfds > MAX_POLL_NFDS {
        return Err(ERRNO::EINVAL);
    }
    let bytes_len = size_of::<PollFd>()
        .checked_mul(nfds)
        .ok_or(ERRNO::EINVAL)?;
    let bytes = translated_byte_buffer(token, ufds as *const u8, bytes_len)
        .or_errno(ERRNO::EFAULT)?;

    let mut pollfds: Vec<PollFd> = Vec::new();
    // 使用可失败分配，避免在内存不足时 panic。
    pollfds
        .try_reserve_exact(nfds)
        .map_err(|_| ERRNO::ENOMEM)?;
    let mut scratch = [0u8; size_of::<PollFd>()];
    let mut scratch_len = 0usize;
    for chunk in bytes {
        let mut off = 0usize;
        while off < chunk.len() {
            let copy_len = (size_of::<PollFd>() - scratch_len).min(chunk.len() - off);
            scratch[scratch_len..scratch_len + copy_len]
                .copy_from_slice(&chunk[off..off + copy_len]);
            scratch_len += copy_len;
            off += copy_len;
            if scratch_len == size_of::<PollFd>() {
                let pfd = unsafe { core::ptr::read_unaligned(scratch.as_ptr() as *const PollFd) };
                pollfds.push(pfd);
                scratch_len = 0;
                if pollfds.len() == nfds {
                    break;
                }
            }
        }
        if pollfds.len() == nfds {
            break;
        }
    }
    if scratch_len != 0 || pollfds.len() != nfds {
        return Err(ERRNO::EFAULT);
    }
    Ok(pollfds)
}

/// 将内核中的 `pollfd` 数组（主要是 `revents`）回写到用户态。
fn write_back_pollfds(ufds: *mut PollFd, pollfds: &[PollFd]) -> Result<(), ERRNO> {
    if pollfds.is_empty() {
        return Ok(());
    }

    // 仅回写 `revents` 字段，保持用户态传入的 `fd` / `events` 不变，
    // 以符合 poll/ppoll 语义并避免覆盖并发更新。
    for (i, pfd) in pollfds.iter().enumerate() {
        let user_revents_ptr = unsafe {
            (ufds.add(i) as usize + offset_of!(PollFd, revents)) as *mut i16
        };
        write_pod_to_user(user_revents_ptr, &pfd.revents)?;
    }
    Ok(())
}

/// 扫描 fd 集，更新每个 `pollfd.revents` 并返回已就绪计数。
fn scan_pollfds(pollfds: &mut [PollFd]) -> usize {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let mut ready_cnt = 0usize;

    for pfd in pollfds.iter_mut() {
        pfd.revents = 0;
        if pfd.fd < 0 {
            continue;
        }
        let fd = pfd.fd as usize;
        let Some(file) = inner.fd_table.get(fd).and_then(|f| f.as_ref()) else {
            pfd.revents = POLLNVAL as i16;
            ready_cnt += 1;
            continue;
        };

        let mut revents = file.desc.poll(pfd.events as u16);
        if !file.desc.readable() && (pfd.events as u16 & POLLIN) != 0 {
            revents |= POLLERR;
        }
        if !file.desc.writable() && (pfd.events as u16 & POLLOUT) != 0 {
            revents |= POLLERR;
        }
        // pipe 实现可能设置 POLLHUP；普通文件默认走 POLLIN/POLLOUT。
        if (revents & POLLHUP) != 0 {
            pfd.revents = (pfd.revents as u16 | POLLHUP) as i16;
        }
        pfd.revents |= revents as i16;
        if pfd.revents != 0 {
            ready_cnt += 1;
        }
    }

    ready_cnt
}

fn has_unmasked_pending_signal() -> bool {
    let task = current_task().unwrap();
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let mut task_inner = task.inner_exclusive_access();
    let thread_pending = task_inner.pending_signals;
    let pending =
        (thread_pending | process_inner.pending_signals) & !task_inner.signal_mask.without_unblockable();

    for signum in 1..=crate::task::MAX_SIG {
        let Some(flag) = SignalBit::from_signum(signum as u32) else {
            continue;
        };
        if !pending.contains(flag) {
            continue;
        }

        let from_thread = thread_pending.contains(flag);
        let action = process_inner.signal_actions.table[signum];
        if action.handler == SIG_IGN {
            if from_thread {
                task_inner.pending_signals &= !flag;
            } else {
                process_inner.pending_signals &= !flag;
            }
            continue;
        }
        if action.handler == SIG_DFL && flag.check_error().is_none() {
            if from_thread {
                task_inner.pending_signals &= !flag;
            } else {
                process_inner.pending_signals &= !flag;
            }
            continue;
        }
        return true;
    }

    false
}

fn parse_timeout_ns(tmo_p: *const OldTimespec32) -> Result<Option<u64>, ERRNO> {
    if tmo_p.is_null() {
        return Ok(None);
    }
    let tmo = read_pod_from_user(tmo_p)?;
    if tmo.tv_sec < 0 || tmo.tv_nsec < 0 || tmo.tv_nsec >= 1_000_000_000 {
        return Err(ERRNO::EINVAL);
    }
    let sec_ns = (tmo.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .ok_or(ERRNO::EINVAL)?;
    let nsec = tmo.tv_nsec as u64;
    let timeout_ns = sec_ns.checked_add(nsec).ok_or(ERRNO::EINVAL)?;
    Ok(Some(timeout_ns))
}

fn timeout_ns_to_deadline_ns(timeout_ns: Option<u64>) -> Result<Option<u64>, ERRNO> {
    match timeout_ns {
        None => Ok(None),
        Some(timeout_ns) => get_time_ns()
            .checked_add(timeout_ns)
            .map(Some)
            .ok_or(ERRNO::EINVAL),
    }
}

fn apply_temp_signal_mask(
    sigmask: *const u8,
    sigsetsize: usize,
    syscall_name: &str,
) -> Result<Option<SignalBit>, ERRNO> {
    if sigmask.is_null() {
        return Ok(None);
    }
    if sigsetsize < size_of::<u32>() {
        warn!(
            "{}: sigsetsize {} too small for sigset_t",
            syscall_name,
            sigsetsize
        );
        return Err(ERRNO::EINVAL);
    }
    let new_mask = if sigsetsize >= size_of::<u64>() {
        let new_mask_bits = read_pod_from_user(sigmask as *const u64)?;
        SignalBit::from_user_bits(new_mask_bits)
    } else {
        let new_mask_bits = read_pod_from_user(sigmask as *const u32)?;
        SignalBit::from_user_bits(new_mask_bits as u64)
    };
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let old = inner.signal_mask;
    inner.signal_mask = new_mask;
    Ok(Some(old))
}

fn restore_temp_signal_mask(old_mask: Option<SignalBit>) {
    if let Some(old) = old_mask {
        current_task().unwrap().inner_exclusive_access().signal_mask = old;
    }
}

fn poll_wait_loop_with_writeback<F>(
    pid: usize,
    pollfds: &mut [PollFd],
    deadline_ns: Option<u64>,
    mut write_back: F,
) -> Result<isize, ERRNO>
where
    F: FnMut(&[PollFd]) -> Result<(), ERRNO>,
{
    loop {
        let ready = scan_pollfds(pollfds);
        write_back(pollfds)?;
        if ready > 0 {
            return Ok(ready as isize);
        }

        if has_unmasked_pending_signal() {
            return Err(ERRNO::EINTR);
        }

        let now_ns = get_time_ns();
        if let Some(deadline_ns) = deadline_ns {
            if now_ns >= deadline_ns {
                return Ok(0);
            }
        }

        let task = current_task().unwrap();
        let interests = {
            let process = current_process();
            let inner = process.inner_exclusive_access();
            let mut rows = Vec::new();
            for pfd in pollfds.iter() {
                if pfd.fd < 0 {
                    continue;
                }
                let fd = pfd.fd as usize;
                if let Some(entry) = inner.fd_table.get(fd).and_then(|slot| slot.as_ref()) {
                    rows.push((fd, entry.desc.poll_source_id(), pfd.events as u16));
                }
            }
            rows
        };

        let handle = match poll::register_poll_wait(pid, &task, &interests) {
            Ok(handle) => handle,
            Err(ERRNO::ENOSPC) => {
                // 回退路径：全局 poll 键/行耗尽时，短周期睡眠后重新扫描 fd 集，
                // 避免直接失败，同时不引入忙等。
                let sleep_until_ns = if let Some(deadline_ns) = deadline_ns {
                    let remain_ns = deadline_ns.saturating_sub(now_ns);
                    let step_ns = PPOLL_FALLBACK_POLL_NS.min(remain_ns);
                    now_ns.saturating_add(step_ns)
                } else {
                    now_ns.saturating_add(PPOLL_FALLBACK_POLL_NS)
                };
                // Match nanosleep-style timer arming so an immediate timeout
                // cannot consume the timer before we actually block.
                {
                    let mut task_inner = task.inner_exclusive_access();
                    task_inner.task_status = TaskStatus::Interruptible;
                    task_inner.wait_reason = Some(WaitReason::Poll);
                }
                add_timer_ns(sleep_until_ns, Arc::clone(&task));
                block_current_and_run_next(WaitReason::Poll);
                continue;
            }
            Err(e) => return Err(e),
        };
        if let Some(deadline_ns) = deadline_ns {
            add_timer_with_poll_tag(deadline_ns, Arc::clone(&task), Some(handle.timer_tag()));
        }
        poll::wait_poll_key(handle);

        let wake_state = poll::poll_wait_state(handle);
        poll::cleanup_poll_wait(handle);

        if matches!(wake_state, PollWakeState::TimedOut) {
            return Ok(0);
        }
    }
}

fn fd_set_word_count(nfds: usize) -> Result<usize, ERRNO> {
    nfds
        .checked_add(FD_SET_BITS_PER_WORD - 1)
        .ok_or(ERRNO::EINVAL)
        .map(|x| x / FD_SET_BITS_PER_WORD)
}

fn copy_user_fdset_words(
    token: usize,
    set: *const usize,
    nfds: usize,
) -> Result<Option<Vec<usize>>, ERRNO> {
    if set.is_null() {
        return Ok(None);
    }
    let words = fd_set_word_count(nfds)?;
    if words == 0 {
        return Ok(Some(Vec::new()));
    }
    let bytes_len = words
        .checked_mul(size_of::<usize>())
        .ok_or(ERRNO::EINVAL)?;
    let bytes = translated_byte_buffer(token, set as *const u8, bytes_len).or_errno(ERRNO::EFAULT)?;
    let mut raw = Vec::new();
    raw.try_reserve_exact(bytes_len).map_err(|_| ERRNO::ENOMEM)?;
    for chunk in bytes {
        raw.extend_from_slice(chunk);
    }
    if raw.len() != bytes_len {
        return Err(ERRNO::EFAULT);
    }

    let mut words_vec = Vec::new();
    words_vec.try_reserve_exact(words).map_err(|_| ERRNO::ENOMEM)?;
    for i in 0..words {
        let off = i * size_of::<usize>();
        let value = unsafe { core::ptr::read_unaligned(raw[off..].as_ptr() as *const usize) };
        words_vec.push(value);
    }

    if let Some(last) = words_vec.last_mut() {
        let valid_bits = nfds % FD_SET_BITS_PER_WORD;
        if valid_bits != 0 {
            *last &= (1usize << valid_bits) - 1;
        }
    }
    Ok(Some(words_vec))
}

fn write_user_fdset_words(set: *mut usize, words: &[usize]) -> Result<(), ERRNO> {
    if set.is_null() || words.is_empty() {
        return Ok(());
    }
    let bytes_len = words
        .len()
        .checked_mul(size_of::<usize>())
        .ok_or(ERRNO::EINVAL)?;
    let bytes = unsafe { slice::from_raw_parts(words.as_ptr() as *const u8, bytes_len) };
    write_bytes_to_user(set as *mut u8, bytes)
}

#[inline]
fn fdset_test_bit(words: &[usize], fd: usize) -> bool {
    let word = fd / FD_SET_BITS_PER_WORD;
    let bit = fd % FD_SET_BITS_PER_WORD;
    words
        .get(word)
        .map(|w| (w & (1usize << bit)) != 0)
        .unwrap_or(false)
}

#[inline]
fn fdset_set_bit(words: &mut [usize], fd: usize) {
    let word = fd / FD_SET_BITS_PER_WORD;
    let bit = fd % FD_SET_BITS_PER_WORD;
    if let Some(dst) = words.get_mut(word) {
        *dst |= 1usize << bit;
    }
}

fn build_pselect_pollfds(
    nfds: usize,
    read_set: Option<&[usize]>,
    write_set: Option<&[usize]>,
    except_set: Option<&[usize]>,
) -> Result<(Vec<PollFd>, Vec<PselectFdMeta>), ERRNO> {
    let mut pollfds = Vec::new();
    pollfds.try_reserve_exact(nfds).map_err(|_| ERRNO::ENOMEM)?;
    let mut metas = Vec::new();
    metas.try_reserve_exact(nfds).map_err(|_| ERRNO::ENOMEM)?;

    for fd in 0..nfds {
        let mut events: u16 = 0;
        let mut meta = PselectFdMeta::default();

        if let Some(read_words) = read_set {
            if fdset_test_bit(read_words, fd) {
                events |= POLLIN;
                meta.read = true;
            }
        }
        if let Some(write_words) = write_set {
            if fdset_test_bit(write_words, fd) {
                events |= POLLOUT;
                meta.write = true;
            }
        }
        if let Some(except_words) = except_set {
            if fdset_test_bit(except_words, fd) {
                events |= POLLPRI;
                meta.except = true;
            }
        }

        if events == 0 {
            continue;
        }
        pollfds.push(PollFd {
            fd: fd as i32,
            events: events as i16,
            revents: 0,
        });
        metas.push(meta);
    }
    Ok((pollfds, metas))
}

fn validate_pselect_fds(
    nfds: usize,
    read_set: Option<&[usize]>,
    write_set: Option<&[usize]>,
    except_set: Option<&[usize]>,
) -> Result<(), ERRNO> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    for fd in 0..nfds {
        let monitored = read_set.map(|s| fdset_test_bit(s, fd)).unwrap_or(false)
            || write_set.map(|s| fdset_test_bit(s, fd)).unwrap_or(false)
            || except_set.map(|s| fdset_test_bit(s, fd)).unwrap_or(false);
        if !monitored {
            continue;
        }
        if inner
            .fd_table
            .get(fd)
            .and_then(|entry| entry.as_ref())
            .is_none()
        {
            return Err(ERRNO::EBADF);
        }
    }
    Ok(())
}

fn write_back_pselect_fdsets(
    readfds: *mut usize,
    writefds: *mut usize,
    exceptfds: *mut usize,
    nfds: usize,
    pollfds: &[PollFd],
    metas: &[PselectFdMeta],
) -> Result<(), ERRNO> {
    let words = fd_set_word_count(nfds)?;
    let mut read_out = if readfds.is_null() {
        None
    } else {
        let mut out = Vec::new();
        out.try_reserve_exact(words).map_err(|_| ERRNO::ENOMEM)?;
        out.resize(words, 0);
        Some(out)
    };
    let mut write_out = if writefds.is_null() {
        None
    } else {
        let mut out = Vec::new();
        out.try_reserve_exact(words).map_err(|_| ERRNO::ENOMEM)?;
        out.resize(words, 0);
        Some(out)
    };
    let mut except_out = if exceptfds.is_null() {
        None
    } else {
        let mut out = Vec::new();
        out.try_reserve_exact(words).map_err(|_| ERRNO::ENOMEM)?;
        out.resize(words, 0);
        Some(out)
    };

    for (pfd, meta) in pollfds.iter().zip(metas.iter()) {
        if pfd.fd < 0 {
            continue;
        }
        let fd = pfd.fd as usize;
        if fd >= nfds {
            continue;
        }
        let revents = pfd.revents as u16;
        if meta.read && (revents & SELECT_READ_REVENTS) != 0 {
            if let Some(out) = read_out.as_mut() {
                fdset_set_bit(out, fd);
            }
        }
        if meta.write && (revents & SELECT_WRITE_REVENTS) != 0 {
            if let Some(out) = write_out.as_mut() {
                fdset_set_bit(out, fd);
            }
        }
        if meta.except && (revents & SELECT_EXCEPT_REVENTS) != 0 {
            if let Some(out) = except_out.as_mut() {
                fdset_set_bit(out, fd);
            }
        }
    }

    // 显式按参数顺序回写，保持与用户态入参一一对应。
    if let Some(out) = read_out.as_ref() {
        write_user_fdset_words(readfds, out)?;
    }
    if let Some(out) = write_out.as_ref() {
        write_user_fdset_words(writefds, out)?;
    }
    if let Some(out) = except_out.as_ref() {
        write_user_fdset_words(exceptfds, out)?;
    }
    Ok(())
}

fn parse_pselect_sigmask_arg(
    sigmask_arg: *const u8,
) -> Result<(*const u8, usize), ERRNO> {
    if sigmask_arg.is_null() {
        return Ok((core::ptr::null(), 0));
    }
    let arg = read_pod_from_user(sigmask_arg as *const PselectSigmaskArg)?;
    Ok((arg.sigmask, arg.sigsetsize))
}

/// 打开路径后需要挂入 `FileDescription` 的打开参数。
struct OpenFileState {
    /// 打开时确定的访问模式。
    access_mode: AccessMode,
    /// 当前可变文件状态位。
    status_flags: FileStatusFlags,
    /// `F_GETFL` 需要保留返回的固定状态位。
    status_fixed_bits: i32,
    /// 传给底层 inode 打开的标志位。
    open_flags: OpenFlags,
}

/// 校验 fd 并返回打开文件描述。
fn get_file_description(fd: usize) -> Result<Arc<FileDescription>, ERRNO> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return Err(ERRNO::EBADF);
    }
    let desc = inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.desc.clone();
    drop(inner);
    Ok(desc)
}

/// 校验 fd 并返回可写打开文件描述。
fn get_writable_file(fd: usize) -> Result<Arc<FileDescription>, ERRNO> {
    let desc = get_file_description(fd)?;
    if desc.is_path() || !desc.writable() {
        return Err(ERRNO::EBADF);
    }
    Ok(desc)
}

fn get_any_file(fd: usize) -> Result<Arc<FileDescription>, ERRNO> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    inner
        .fd_table
        .get(fd)
        .and_then(|entry| entry.as_ref())
        .map(|entry| entry.desc.clone())
        .ok_or(ERRNO::EBADF)
}

/// 校验 fd 并返回可读打开文件描述。
fn get_readable_file(fd: usize) -> Result<Arc<FileDescription>, ERRNO> {
    let desc = get_file_description(fd)?;
    if desc.is_path() || !desc.readable() {
        return Err(ERRNO::EBADF);
    }
    Ok(desc)
}

fn parse_pos64(pos: i64) -> Result<usize, ERRNO> {
    if pos < 0 {
        return Err(ERRNO::EINVAL);
    }
    usize::try_from(pos).map_err(|_| ERRNO::EINVAL)
}

fn parse_pos64_halves(pos_l: usize, pos_h: usize) -> Result<usize, ERRNO> {
    let pos = ((pos_h as u64) << 32) | ((pos_l as u64) & 0xffff_ffff);
    usize::try_from(pos).map_err(|_| ERRNO::EINVAL)
}

fn is_regular_file(desc: &Arc<FileDescription>) -> bool {
    let mode = desc.stat().mode;
    mode.bits() & StatMode::TYPE_MASK.bits() == StatMode::FILE.bits()
}

fn preadv_like(
    desc: &Arc<FileDescription>,
    iovecs: &[IoVec],
    mut offset: usize,
    access: PageFaultAccess,
) -> Result<isize, ERRNO> {
    if !desc.is_seekable() {
        return Err(ERRNO::ESPIPE);
    }

    let mut total = 0usize;
    for &iovec in iovecs {
        if iovec.iov_len == 0 {
            continue;
        }
        let user_buf = UserBuffer::new(translated_byte_buffer_with_access(
            iovec.iov_base as *const u8,
            iovec.iov_len,
            access,
        )?);
        let read = desc.read_at(offset, user_buf);
        total = total.checked_add(read).ok_or(ERRNO::EINVAL)?;
        offset = offset.checked_add(read).ok_or(ERRNO::EINVAL)?;
        if read < iovec.iov_len {
            break;
        }
    }
    Ok(total as isize)
}

fn pwritev_like(
    desc: &Arc<FileDescription>,
    iovecs: &[IoVec],
    mut offset: usize,
) -> Result<isize, ERRNO> {
    if !desc.is_seekable() {
        return Err(ERRNO::ESPIPE);
    }

    let mut total_limit = 0usize;
    for &iovec in iovecs {
        total_limit = total_limit
            .checked_add(iovec.iov_len)
            .ok_or(ERRNO::EINVAL)?;
        if total_limit > isize::MAX as usize {
            return Err(ERRNO::EINVAL);
        }
    }

    let mut completed = 0usize;
    for &iovec in iovecs {
        if iovec.iov_len == 0 {
            continue;
        }
        let user_buf = UserBuffer::new(translated_byte_buffer_with_access(
            iovec.iov_base as *const u8,
            iovec.iov_len,
            PageFaultAccess::Read,
        )?);
        let written = desc.write_at(offset, user_buf);
        completed = completed.checked_add(written).ok_or(ERRNO::EINVAL)?;
        offset = offset.checked_add(written).ok_or(ERRNO::EINVAL)?;
        if written < iovec.iov_len {
            return Ok(completed as isize);
        }
    }
    Ok(completed as isize)
}

/// 解析 truncate 系统调用传入的目标长度。
fn parse_truncate_len(len: isize) -> Result<usize, ERRNO> {
    if len < 0 {
        return Err(ERRNO::EINVAL);
    }
    Ok(len as usize)
}

/// 从用户态复制 `iovec` 数组，避免数组跨页时直接解引用失败。
pub fn copy_user_iovecs(token: usize, iov: *const IoVec, iovcnt: i32) -> Result<Vec<IoVec>, ERRNO> {
    if iovcnt < 0 {
        return Err(ERRNO::EINVAL);
    }
    if iovcnt == 0 {
        return Ok(Vec::new());
    }
    let iovcnt = iovcnt as usize;
    let iov_bytes_len: usize = size_of::<IoVec>()
        .checked_mul(iovcnt)
        .ok_or(ERRNO::EINVAL)?;
    let iov_bytes = translated_byte_buffer(token, iov as *const u8, iov_bytes_len)
        .or_errno(ERRNO::EFAULT)?;
    let mut iovecs = Vec::with_capacity(iovcnt);
    let mut scratch = [0u8; size_of::<IoVec>()];
    let mut scratch_len = 0usize;
    // 以IoVec为单位平接。可能出现一个u8分属两个不同IoVec，所以下面按照offset一点点拼凑每一个IoVec。
    for chunk in iov_bytes {
        let mut chunk_offset = 0usize;
        while chunk_offset < chunk.len() {
            let copy_len = (size_of::<IoVec>() - scratch_len).min(chunk.len() - chunk_offset);
            // 逐步拼出一个完整的 `IoVec`，以兼容结构体跨页的情况。
            scratch[scratch_len..scratch_len + copy_len]
                .copy_from_slice(&chunk[chunk_offset..chunk_offset + copy_len]);
            scratch_len += copy_len;
            chunk_offset += copy_len;
            if scratch_len == size_of::<IoVec>() {
                // 这里按 C ABI 逐项复制，避免直接依赖用户地址对齐。
                let iovec = unsafe { core::ptr::read_unaligned(scratch.as_ptr() as *const IoVec) };
                iovecs.push(iovec);
                scratch_len = 0;
                if iovecs.len() == iovcnt {
                    break;
                }
            }
        }
        if iovecs.len() == iovcnt {
            break;
        }
    }
    if scratch_len != 0 || iovecs.len() != iovcnt {
        // 正常情况下长度应严格对齐；若触发说明用户内存翻译结果异常。
        return Err(ERRNO::EFAULT);
    }
    Ok(iovecs)
}

/// 从用户态复制 `utimensat` 需要的两个 `timespec`，允许结构体跨页。
fn copy_user_timespec_pair(token: usize, times: *const Timespec) -> Result<[Timespec; 2], ERRNO> {
    let raw_len = 2 * size_of::<Timespec>();
    let raw_chunks = translated_byte_buffer(token, times as *const u8, raw_len)
        .or_errno(ERRNO::EFAULT)?;
    let mut raw = [0u8; 2 * size_of::<Timespec>()];
    let mut copied = 0usize;
    for chunk in raw_chunks {
        if copied >= raw.len() {
            break;
        }
        let take = (raw.len() - copied).min(chunk.len());
        raw[copied..copied + take].copy_from_slice(&chunk[..take]);
        copied += take;
    }
    if copied != raw.len() {
        return Err(ERRNO::EFAULT);
    }
    let atime = unsafe { core::ptr::read_unaligned(raw.as_ptr() as *const Timespec) };
    let mtime = unsafe {
        core::ptr::read_unaligned(raw[size_of::<Timespec>()..].as_ptr() as *const Timespec)
    };
    Ok([atime, mtime])
}

/// 解析 `utimensat` 的单个时间参数。
fn parse_utime_arg(ts: Timespec) -> Result<UtimeArg, ERRNO> {
    match ts.tv_nsec {
        UTIME_NOW => Ok(UtimeArg::Now),
        UTIME_OMIT => Ok(UtimeArg::Omit),
        nsec if nsec < 1_000_000_000 => Ok(UtimeArg::Set(InodeTime::new(ts.tv_sec as u64, nsec as u32))),
        _ => Err(ERRNO::EINVAL),
    }
}

/// Result of resolving a (dirfd, path, flags) target.
enum ResolvedAtTarget {
    /// A concrete filesystem inode identified by path or fd.
    Inode(Arc<fs::Inode>),
    /// An open file description (fd) that does not map to a filesystem inode.
    FileDesc(Arc<FileDescription>),
}

/// Resolve `dirfd + path + flags` into either a filesystem inode or an
/// open `FileDescription` when the fd does not correspond to an inode.
///
/// Semantics:
/// - If `path` is empty, `AT_EMPTY_PATH` must be set. If `dirfd == AT_FDCWD`,
///   resolve the current working directory's inode. If `dirfd` is an fd, try
///   to return the underlying inode (if the `FileDescription` wraps one);
///   otherwise return the `FileDescription` itself so callers can operate on
///   the open descriptor.
/// - If `path` is non-empty, resolve against `dirfd` (or CWD) and return the
///   corresponding inode or `ENOENT`.
fn resolve_at_target(dirfd: isize, path: &str, flags: i32) -> Result<ResolvedAtTarget, ERRNO> {
    if path.is_empty() {
        if flags & AT_EMPTY_PATH as i32 == 0 {
            return Err(ERRNO::ENOENT);
        }
        if dirfd == AT_FDCWD {
            let cwd = current_process().inner_exclusive_access().cwd.clone();
            return lookup_inode_follow("/", cwd.as_str(), true).map(ResolvedAtTarget::Inode);
        }
        if dirfd < 0 {
            return Err(ERRNO::EBADF);
        }
        let desc = get_file_description(dirfd as usize)?;
        // Prefer returning the underlying inode if available (e.g. OSInode).
        if let Some(inode) = desc.as_inode() {
            return Ok(ResolvedAtTarget::Inode(inode));
        }
        // Fall back to path-based lookup if the description recorded an open path.
        if let Some(target_path) = desc.path() {
            return lookup_inode_follow("/", target_path.as_str(), true).map(ResolvedAtTarget::Inode);
        }
        // Otherwise return the open description itself (e.g. pipe/tty).
        return Ok(ResolvedAtTarget::FileDesc(desc));
    }

    let cwd = resolve_dirfd_base(dirfd, path)?;
    let follow_final = flags & AT_SYMLINK_NOFOLLOW as i32 == 0;
    lookup_inode_follow(cwd.as_str(), rooted_lookup_path(path), follow_final).map(ResolvedAtTarget::Inode)
}

fn resolve_dirfd_base(dirfd: isize, path: &str) -> Result<String, ERRNO> {
    let process = current_process();
    if path.starts_with('/') {
        return Ok(process.inner_exclusive_access().root.clone());
    }
    if dirfd == AT_FDCWD {
        return Ok(process.inner_exclusive_access().cwd.clone());
    }
    if dirfd < 0 {
        return Err(ERRNO::EBADF);
    }
    let inner = process.inner_exclusive_access();
    let desc = inner
        .fd_table
        .get(dirfd as usize)
        .and_then(|entry| entry.as_ref())
        .map(|entry| entry.desc.clone())
        .ok_or(ERRNO::EBADF)?;
    drop(inner);
    if !desc.is_dir() {
        return Err(ERRNO::ENOTDIR);
    }
    desc.path().ok_or(ERRNO::ENOTDIR)
}

fn resolve_simple_dirfd_inode(dirfd: isize, path: &str) -> Result<Option<Arc<::fs::vfs::Inode>>, ERRNO> {
    if dirfd == AT_FDCWD || path.starts_with('/') || path.contains('/') {
        return Ok(None);
    }
    if dirfd < 0 {
        return Err(ERRNO::EBADF);
    }
    let desc = get_file_description(dirfd as usize)?;
    if !desc.is_dir() {
        return Err(ERRNO::ENOTDIR);
    }
    desc.as_inode().map(Some).ok_or(ERRNO::ENOTDIR)
}

const F_DUPFD: i32 = 0;
const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const F_DUPFD_CLOEXEC: i32 = 1030;

const F_OK: i32 = 0;
const X_OK: i32 = 1;
const W_OK: i32 = 2;
const R_OK: i32 = 4;

const UTIME_NOW: usize = 0x3fff_ffff;
const UTIME_OMIT: usize = 0x3fff_fffe;
const UID_GID_NO_CHANGE: u32 = u32::MAX;

fn inode_allows_access(inode: &Arc<fs::Inode>, uid: u32, gid: u32, mode: u32) -> bool {
    if mode == F_OK as u32 {
        return true;
    }

    let Some(raw_mode) = inode.mode() else {
        return inode.check_access(uid, gid, mode);
    };
    let perm_bits = raw_mode & 0o777;

    if uid == 0 {
        if mode & X_OK as u32 != 0 && perm_bits & 0o111 == 0 {
            return false;
        }
        return true;
    }

    let file_uid = inode.uid().unwrap_or(0);
    let file_gid = inode.gid().unwrap_or(0);
    let perm = if uid == file_uid {
        (perm_bits >> 6) & 0o7
    } else if gid == file_gid {
        (perm_bits >> 3) & 0o7
    } else {
        perm_bits & 0o7
    };

    if mode & R_OK as u32 != 0 && perm & 0o4 == 0 {
        return false;
    }
    if mode & W_OK as u32 != 0 && perm & 0o2 == 0 {
        return false;
    }
    if mode & X_OK as u32 != 0 && perm & 0o1 == 0 {
        return false;
    }
    true
}

fn check_path_search_permissions(cwd: &str, path: &str, uid: u32, gid: u32) -> Result<(), ERRNO> {
    if uid == 0 {
        return Ok(());
    }

    let abs = canonicalize(cwd, rooted_lookup_path(path));
    let components: Vec<&str> = abs.split('/').filter(|s| !s.is_empty()).collect();
    if components.len() <= 1 {
        return Ok(());
    }

    let mut prefix = String::from("/");
    for component in &components[..components.len() - 1] {
        if prefix.len() > 1 {
            prefix.push('/');
        }
        prefix.push_str(component);
        let inode = lookup_inode_follow("/", prefix.as_str(), true)?;
        if !inode.is_dir() {
            return Err(ERRNO::ENOTDIR);
        }
        if !inode_allows_access(&inode, uid, gid, X_OK as u32) {
            return Err(ERRNO::EACCES);
        }
    }
    Ok(())
}

fn rooted_lookup_path(path: &str) -> &str {
    if path.starts_with('/') {
        path.trim_start_matches('/')
    } else {
        path
    }
}

fn current_cred_in_group(gid: u32) -> bool {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let cred = inner.cred;
    cred.egid == gid
        || cred.supplementary_groups[..cred.supplementary_group_count]
            .iter()
            .any(|group| *group == gid)
}

fn chmod_inode(inode: &Arc<fs::Inode>, mode: u32) -> Result<(), ERRNO> {
    const S_ISGID: u32 = 0o2000;

    let process = current_process();
    let euid = process.geteuid();
    let owner = inode.uid().unwrap_or(0);
    if euid != 0 && euid != owner {
        return Err(ERRNO::EPERM);
    }

    let old_mode = inode_stat(inode).mode.bits();
    let mut new_mode = (old_mode & StatMode::TYPE_MASK.bits()) | (mode & StatMode::PERM_MASK.bits());
    let group = inode.gid().unwrap_or(0);
    if euid != 0 && (new_mode & S_ISGID) != 0 && !current_cred_in_group(group) {
        new_mode &= !S_ISGID;
    }
    inode.set_mode(new_mode)?;
    Ok(())
}

fn chown_inode(inode: &Arc<fs::Inode>, user: u32, group: u32) -> Result<(), ERRNO> {
    const S_ISUID: u32 = 0o4000;
    const S_ISGID: u32 = 0o2000;
    const S_IXGRP: u32 = 0o0010;

    if user == UID_GID_NO_CHANGE && group == UID_GID_NO_CHANGE {
        return Ok(());
    }

    let attrs = inode.stat_attrs();
    let old_uid = attrs.uid.or_else(|| inode.uid()).ok_or(ERRNO::EOPNOTSUPP)?;
    let old_gid = attrs.gid.or_else(|| inode.gid()).ok_or(ERRNO::EOPNOTSUPP)?;
    let new_uid = if user == UID_GID_NO_CHANGE { old_uid } else { user };
    let new_gid = if group == UID_GID_NO_CHANGE { old_gid } else { group };

    let process = current_process();
    let euid = process.geteuid();
    if euid != 0 {
        if euid != old_uid || new_uid != old_uid || !current_cred_in_group(new_gid) {
            return Err(ERRNO::EPERM);
        }
    }

    inode.set_owner(new_uid, new_gid)?;

    let old_mode = inode_stat(inode).mode.bits();
    if old_mode & StatMode::TYPE_MASK.bits() == StatMode::FILE.bits() {
        let mut clear = S_ISUID;
        if old_mode & S_IXGRP != 0 {
            clear |= S_ISGID;
        }
        let new_mode = old_mode & !clear;
        if new_mode != old_mode {
            inode.set_mode(new_mode)?;
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum UtimeArg {
    Now,
    Omit,
    Set(InodeTime),
}

/// `fcntl(F_GETFD/F_SETFD)` 可见的 fd 标志位。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
enum FcntlFdFlag {
    /// `exec` 成功后自动关闭该 fd。
    Cloexec = 0x1,
}

impl FcntlFdFlag {
    /// 汇总所有当前已识别的 fd 标志位掩码。
    const ALL_BITS: i32 = Self::Cloexec as i32;
}

/// 过滤并校验 `openat` 的路径打开语义位。
fn filter_open_flags(flags: i32) -> Result<OpenFileState, ERRNO> {
    const O_APPEND: i32 = FileStatusFlags::APPEND.bits();
    const O_NOCTTY: i32 = 0x100;
    const O_NONBLOCK: i32 = FileStatusFlags::NONBLOCK.bits();
    const O_LARGEFILE: i32 = 0x8000;
    const O_DIRECTORY: i32 = OpenFlags::DIRECTORY.bits();
    const O_NOFOLLOW: i32 = 0x20000;
    const O_PATH: i32 = 0x200000;

    // O_PATH 仅获取一个指向文件位置的描述符；访问模式位被忽略，文件本身
    // 不会被真正打开（read/write 等需返回 EBADF）。这里识别该标志并将其
    // 透传到 `status_fixed_bits`，供 `F_GETFL` 及套接字系统调用据此判定。
    let path_flag = flags & O_PATH;
    let access_mode = if path_flag != 0 {
        AccessMode::ReadOnly
    } else {
        AccessMode::from_open_bits(flags)?
    };
    let ignored_flags = flags & (O_LARGEFILE | O_NOFOLLOW | O_NOCTTY);
    let unsupported_flags = 0;
    let status_flags = FileStatusFlags::from_bits_truncate(flags & (O_APPEND | O_NONBLOCK));
    let effective_flags = flags & !(ignored_flags | path_flag | O_APPEND | O_NONBLOCK);

    if unsupported_flags != 0 {
        // TODO: 后续若补齐 tty 控制终端语义，应在进程/会话层实现真实行为。
        warn!(
            "sys_open: unsupported open flags {:#x}",
            unsupported_flags
        );
        return Err(ERRNO::EINVAL);
    }
    if ignored_flags != 0 {
        // TODO: 后续若补齐大文件兼容细节，可在这里补充更精细的位语义校验。
        warn!(
            "sys_open: ignore ignorable open flags {:#x}",
            ignored_flags
        );
    }
    let open_flags = OpenFlags::from_bits(effective_flags).ok_or(ERRNO::EINVAL)?;
    Ok(OpenFileState {
        access_mode,
        status_flags,
        status_fixed_bits: (effective_flags & O_DIRECTORY) | path_flag,
        open_flags,
    })
}

/// 解析 `F_SETFL` 可修改的文件状态位。
fn parse_setfl_status(arg: usize) -> Result<FileStatusFlags, ERRNO> {
    let arg = i32::try_from(arg).map_err(|_| ERRNO::EINVAL)?;
    let mutable_mask = FileStatusFlags::APPEND.bits() | FileStatusFlags::NONBLOCK.bits();
    let ignored_mask =
        0x3 | OpenFlags::CREATE.bits() | OpenFlags::TRUNC.bits() | OpenFlags::DIRECTORY.bits() | OpenFlags::NOFOLLOW.bits() | 0x100 | 0x8000;
    if arg & !(mutable_mask | ignored_mask) != 0 {
        // TODO: 后续若补齐 `O_ASYNC/O_DIRECT/O_NOATIME` 等位，应在这里扩展掩码。
        warn!(
            "sys_fcntl: unsupported F_SETFL flags {:#x}",
            arg & !(mutable_mask | ignored_mask)
        );
        return Err(ERRNO::EINVAL);
    }
    Ok(FileStatusFlags::from_bits_truncate(arg))
}

/// `fcntl` 系统调用：处理 fd 标志、文件状态位与描述复制。
pub fn sys_fcntl(fd: u32, cmd: i32, arg: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_fcntl",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    syscall_body!({
        let fd = fd as usize;
        let mut inner = process.inner_exclusive_access();
        if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
            return Err(ERRNO::EBADF);
        }
        match cmd {
            F_GETFD => {
                let mut flags = 0i32;
                let entry = inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?;
                if entry.flags.contains(FdFlags::CLOEXEC) {
                    flags |= FcntlFdFlag::Cloexec as i32;
                }
                Ok(flags as isize)
            }
            F_SETFD => {
                let entry = inner.fd_table[fd].as_mut().ok_or(ERRNO::EBADF)?;
                let arg = i32::try_from(arg).map_err(|_| ERRNO::EINVAL)?;
                if arg & !FcntlFdFlag::ALL_BITS != 0 {
                    // TODO: 后续若补齐额外 fd 标志位，应在这里扩展掩码并同步到 `FdFlags`。
                    warn!(
                        "sys_fcntl: unsupported F_SETFD flags {:#x}",
                        arg & !FcntlFdFlag::ALL_BITS
                    );
                    return Err(ERRNO::EINVAL);
                }
                if arg & (FcntlFdFlag::Cloexec as i32) != 0 {
                    entry.flags |= FdFlags::CLOEXEC;
                } else {
                    entry.flags.remove(FdFlags::CLOEXEC);
                }
                Ok(0)
            }
            F_DUPFD => {
                let min_fd = i32::try_from(arg).map_err(|_| ERRNO::EINVAL)?;
                if min_fd < 0 {
                    return Err(ERRNO::EINVAL);
                }
                let desc = Arc::clone(&inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.desc);
                let new_fd = inner.alloc_fd_from(min_fd as usize)?;
                inner.fd_table[new_fd] = Some(FdEntry {
                    desc,
                    flags: FdFlags::empty(),
                });
                Ok(new_fd as isize)
            }
            F_GETFL => {
                let desc = Arc::clone(&inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.desc);
                drop(inner);
                Ok(desc.status_bits() as isize)
            }
            F_SETFL => {
                let desc = Arc::clone(&inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.desc);
                let status_flags = parse_setfl_status(arg)?;
                drop(inner);
                desc.set_status_flags(status_flags);
                Ok(0)
            }
            F_DUPFD_CLOEXEC => {
                let min_fd = i32::try_from(arg).map_err(|_| ERRNO::EINVAL)?;
                if min_fd < 0 {
                    return Err(ERRNO::EINVAL);
                }
                let desc = Arc::clone(&inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.desc);
                let new_fd = inner.alloc_fd_from(min_fd as usize)?;
                inner.fd_table[new_fd] = Some(FdEntry {
                    desc,
                    flags: FdFlags::CLOEXEC,
                });
                Ok(new_fd as isize)
            }
            _ => Err(ERRNO::EINVAL),
        }
    })
}

/// `flock` 系统调用：对打开文件描述施加 BSD advisory lock。
pub fn sys_flock(fd: u32, operation: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_flock",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let desc = get_any_file(fd as usize)?;
        desc.flock(operation)?;
        Ok(0)
    })
}

/// write syscall
pub fn sys_write(fd: u32, buf: *const u8, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_write",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_writable_file(fd)?;
        let stat = desc.stat();
        let path = desc.path();
        let inode = desc.backing_inode();
        debug!(
            "sys_write: fd={} len={} file_size={} st_blksize={} path={:?} inode={}",
            fd,
            len,
            stat.size,
            stat.blksize,
            path,
            inode
                .as_ref()
                .map(|inode| alloc::format!("{}:{}", inode.fs_id(), inode.ino()))
                .unwrap_or_else(|| String::from("-")),
        );
        if len >= SUSPICIOUS_RW_LEN || stat.blksize >= SUSPICIOUS_STAT_BLKSIZE {
            warn!(
                "sys_write: suspicious request fd={} len={} file_size={} st_blksize={} path={:?}",
                fd,
                len,
                stat.size,
                stat.blksize,
                path
            );
        }
        let written = desc.write_result(UserBuffer::new(
            translated_byte_buffer_with_access(buf, len, PageFaultAccess::Read)?,
        ))?;
        if len > 0 && written == 0 && write_zero_is_broken_pipe(&desc) {
            return Err(ERRNO::EPIPE);
        }
        Ok(written as isize)
    })
}

/// pread64 syscall：从固定偏移读取，不推进共享文件偏移。
pub fn sys_pread64(fd: u32, buf: *const u8, len: usize, pos: i64) -> isize {
    trace!(
        "kernel:pid[{}] sys_pread64",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_readable_file(fd)?;
        let offset = parse_pos64(pos)?;
        if !desc.is_seekable() {
            return Err(ERRNO::ESPIPE);
        }
        Ok(desc.read_at(
            offset,
            UserBuffer::new(translated_byte_buffer_with_access(
                buf,
                len,
                PageFaultAccess::Write,
            )?),
        ) as isize)
    })
}

/// pwrite64 syscall：向固定偏移写入，不推进共享文件偏移。
pub fn sys_pwrite64(fd: u32, buf: *const u8, len: usize, pos: i64) -> isize {
    trace!(
        "kernel:pid[{}] sys_pwrite64",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_writable_file(fd)?;
        let offset = parse_pos64(pos)?;
        let stat = desc.stat();
        let path = desc.path();
        debug!(
            "sys_pwrite64: fd={} offset={} len={} file_size={} st_blksize={} path={:?}",
            fd,
            offset,
            len,
            stat.size,
            stat.blksize,
            path
        );
        if len >= SUSPICIOUS_RW_LEN || stat.blksize >= SUSPICIOUS_STAT_BLKSIZE {
            warn!(
                "sys_pwrite64: suspicious request fd={} offset={} len={} file_size={} st_blksize={} path={:?}",
                fd,
                offset,
                len,
                stat.size,
                stat.blksize,
                path
            );
        }
        if !desc.is_seekable() {
            return Err(ERRNO::ESPIPE);
        }
        Ok(desc.write_at(
            offset,
            UserBuffer::new(translated_byte_buffer_with_access(
                buf,
                len,
                PageFaultAccess::Read,
            )?),
        ) as isize)
    })
}

/// sendfile64 syscall：从普通文件搬运数据到另一个 fd，避免用户态中转。
pub fn sys_sendfile64(out_fd: i32, in_fd: i32, offset: *mut i64, count: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_sendfile64",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        if out_fd < 0 || in_fd < 0 {
            return Err(ERRNO::EBADF);
        }
        if count > isize::MAX as usize {
            return Err(ERRNO::EINVAL);
        }

        let out_desc = get_writable_file(out_fd as usize)?;
        let in_desc = get_readable_file(in_fd as usize)?;

        if out_desc.status_flags().contains(FileStatusFlags::APPEND) {
            return Err(ERRNO::EINVAL);
        }
        if !in_desc.is_seekable() || !is_regular_file(&in_desc) {
            return Err(ERRNO::EINVAL);
        }

        let mut in_pos = if offset.is_null() {
            usize::try_from(in_desc.seek(0, 1)?).map_err(|_| ERRNO::EINVAL)?
        } else {
            parse_pos64(read_pod_from_user(offset as *const i64)?)?
        };

        let chunk_len = count.min(SENDFILE_CHUNK_SIZE);
        let mut buf = Vec::new();
        buf.try_reserve_exact(chunk_len).map_err(|_| ERRNO::ENOMEM)?;
        buf.resize(chunk_len, 0);

        let mut copied = 0usize;
        let result: Result<isize, ERRNO> = (|| {
            while copied < count {
                let want = (count - copied).min(buf.len());
                let read = match in_desc.read_bytes_at(in_pos, &mut buf[..want]) {
                    Ok(n) => n,
                    Err(err) if copied > 0 => return Ok(copied as isize),
                    Err(err) => return Err(err),
                };
                if read == 0 {
                    break;
                }

                let written = match out_desc.write_bytes(&buf[..read]) {
                    Ok(n) => n,
                    Err(err) if copied > 0 => return Ok(copied as isize),
                    Err(err) => return Err(err),
                };
                if written == 0 {
                    break;
                }

                in_pos = in_pos.checked_add(written).ok_or(ERRNO::EINVAL)?;
                copied = copied.checked_add(written).ok_or(ERRNO::EINVAL)?;
                if written < read {
                    break;
                }
            }
            Ok(copied as isize)
        })();

        if offset.is_null() {
            in_desc.seek(in_pos as i64, 0)?;
        } else {
            write_pod_to_user(offset, &(in_pos as i64))?;
        }

        result
    })
}

/// splice syscall：在两个 fd 之间搬运数据。
///
/// GNU grep probes `splice(2)` on pipe input and falls back to normal reads
/// when the fd combination is not supported. Returning ENOSYS is treated as an
/// input error by that grep build, so unsupported combinations must return
/// EINVAL instead.
pub fn sys_splice(
    fd_in: i32,
    off_in: *mut i64,
    fd_out: i32,
    off_out: *mut i64,
    len: usize,
    _flags: u32,
) -> isize {
    trace!(
        "kernel:pid[{}] sys_splice",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        if fd_in < 0 || fd_out < 0 {
            return Err(ERRNO::EBADF);
        }
        if len > isize::MAX as usize {
            return Err(ERRNO::EINVAL);
        }
        if len == 0 {
            return Ok(0);
        }

        let in_desc = get_readable_file(fd_in as usize)?;
        let out_desc = get_writable_file(fd_out as usize)?;
        let in_is_pipe = in_desc.as_any().is::<Pipe>();
        let out_is_pipe = out_desc.as_any().is::<Pipe>();

        if !in_is_pipe && !out_is_pipe {
            return Err(ERRNO::EINVAL);
        }

        // GNU grep probes splice on pipe input. Until the pipe fast path can
        // preserve grep's buffered-output semantics, report the combination as
        // unsupported so userspace uses its normal write path.
        if !in_desc.is_seekable() || !out_desc.is_seekable() {
            return Err(ERRNO::EINVAL);
        }

        if out_desc.status_flags().contains(FileStatusFlags::APPEND) {
            return Err(ERRNO::EINVAL);
        }
        if !off_in.is_null() && !in_desc.is_seekable() {
            return Err(ERRNO::ESPIPE);
        }
        if !off_out.is_null() && !out_desc.is_seekable() {
            return Err(ERRNO::ESPIPE);
        }

        let mut in_pos = if off_in.is_null() {
            if in_desc.is_seekable() {
                Some(usize::try_from(in_desc.seek(0, 1)?).map_err(|_| ERRNO::EINVAL)?)
            } else {
                None
            }
        } else {
            Some(parse_pos64(read_pod_from_user(off_in as *const i64)?)?)
        };
        let mut out_pos = if off_out.is_null() {
            if out_desc.is_seekable() {
                Some(usize::try_from(out_desc.seek(0, 1)?).map_err(|_| ERRNO::EINVAL)?)
            } else {
                None
            }
        } else {
            Some(parse_pos64(read_pod_from_user(off_out as *const i64)?)?)
        };

        let chunk_len = len.min(SENDFILE_CHUNK_SIZE);
        let mut buf = Vec::new();
        buf.try_reserve_exact(chunk_len).map_err(|_| ERRNO::ENOMEM)?;
        buf.resize(chunk_len, 0);

        let mut copied = 0usize;
        let result: Result<isize, ERRNO> = (|| {
            while copied < len {
                let want = (len - copied).min(buf.len());
                let read = match in_pos {
                    Some(pos) => in_desc.read_bytes_at(pos, &mut buf[..want]),
                    None => in_desc.read_bytes_at(0, &mut buf[..want]),
                };
                let read = match read {
                    Ok(n) => n,
                    Err(err) if copied > 0 => return Ok(copied as isize),
                    Err(err) => return Err(err),
                };
                if read == 0 {
                    break;
                }

                let written = match out_pos {
                    Some(pos) if !off_out.is_null() => out_desc.write_bytes_at(pos, &buf[..read]),
                    _ => out_desc.write_bytes(&buf[..read]),
                };
                let written = match written {
                    Ok(n) => n,
                    Err(err) if copied > 0 => return Ok(copied as isize),
                    Err(err) => return Err(err),
                };
                if written == 0 {
                    break;
                }

                if let Some(pos) = &mut in_pos {
                    *pos = pos.checked_add(written).ok_or(ERRNO::EINVAL)?;
                }
                if let Some(pos) = &mut out_pos {
                    *pos = pos.checked_add(written).ok_or(ERRNO::EINVAL)?;
                }
                copied = copied.checked_add(written).ok_or(ERRNO::EINVAL)?;
                if written < read {
                    break;
                }
            }
            Ok(copied as isize)
        })();

        if off_in.is_null() {
            if let Some(pos) = in_pos {
                in_desc.seek(pos as i64, 0)?;
            }
        } else if let Some(pos) = in_pos {
            write_pod_to_user(off_in, &(pos as i64))?;
        }
        if off_out.is_null() {
            if let Some(pos) = out_pos {
                out_desc.seek(pos as i64, 0)?;
            }
        } else if let Some(pos) = out_pos {
            write_pod_to_user(off_out, &(pos as i64))?;
        }

        result
    })
}

/// fadvise64 syscall：接受用户态的文件访问模式提示。
pub fn sys_fadvise64(fd: i32, _offset: i64, _len: usize, advice: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_fadvise64",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        if fd < 0 {
            return Err(ERRNO::EBADF);
        }
        get_any_file(fd as usize)?;
        if !(0..=5).contains(&advice) {
            return Err(ERRNO::EINVAL);
        }
        Ok(0)
    })
}

/// readv syscall：按 `iovec` 顺序将多个用户缓冲区从同一个 fd 读出。
pub fn sys_readv(fd: usize, iov: *const IoVec, iovcnt: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_readv",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let fd = fd as usize;
        let desc = get_readable_file(fd)?;
        let iovecs = copy_user_iovecs(token, iov, iovcnt)?;
        let mut read_total = 0usize;
        for &iovec in &iovecs {
            if iovec.iov_len == 0 {
                continue;
            }
            let user_buf = UserBuffer::new(
                translated_byte_buffer_with_access(
                    iovec.iov_base as *const u8,
                    iovec.iov_len,
                    PageFaultAccess::Write,
                )?,
            );
            let read = desc.read_result(user_buf)?;
            read_total = read_total.checked_add(read).ok_or(ERRNO::EINVAL)?;
            if read < iovec.iov_len {
                break;
            }
        }
        Ok(read_total as isize)
    })
}

/// writev syscall：按 `iovec` 顺序将多个用户缓冲区写入同一个 fd。
pub fn sys_writev(fd: usize, iov: *const IoVec, iovcnt: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_writev",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let fd = fd as usize;
        let desc = get_writable_file(fd)?;
        let iovecs = copy_user_iovecs(token, iov, iovcnt)?;
        let mut written_total = 0usize;
        for &iovec in &iovecs {
            written_total = written_total
                .checked_add(iovec.iov_len)
                .ok_or(ERRNO::EINVAL)?;
            if written_total > isize::MAX as usize {
                return Err(ERRNO::EINVAL);
            }
        }
        let mut completed = 0usize;
        for &iovec in &iovecs {
            if iovec.iov_len == 0 {
                continue;
            }
            let user_buf = UserBuffer::new(
                translated_byte_buffer_with_access(
                    iovec.iov_base as *const u8,
                    iovec.iov_len,
                    PageFaultAccess::Read,
                )?,
            );
            let written = desc.write_result(user_buf)?;
            if iovec.iov_len > 0 && written == 0 && write_zero_is_broken_pipe(&desc) {
                return Err(ERRNO::EPIPE);
            }
            completed += written;
            // 发生短写时立即返回，保留与 `write` 一致的部分写入语义。
            if written < iovec.iov_len {
                return Ok(completed as isize);
            }
        }
        // TODO: 当前未限制 `iovcnt` 上限；若后续补齐 `IOV_MAX`，应在复制前返回 `EINVAL`。
        Ok(completed as isize)
    })
}

/// preadv syscall：从固定偏移按 `iovec` 顺序读取，不推进共享文件偏移。
pub fn sys_preadv(fd: usize, iov: *const IoVec, iovcnt: i32, pos_l: usize, pos_h: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_preadv",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let desc = get_readable_file(fd)?;
        let iovecs = copy_user_iovecs(token, iov, iovcnt)?;
        let offset = parse_pos64_halves(pos_l, pos_h)?;
        preadv_like(&desc, &iovecs, offset, PageFaultAccess::Write)
    })
}

/// pwritev syscall：向固定偏移按 `iovec` 顺序写入，不推进共享文件偏移。
pub fn sys_pwritev(fd: usize, iov: *const IoVec, iovcnt: i32, pos_l: usize, pos_h: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_pwritev",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let desc = get_writable_file(fd)?;
        let iovecs = copy_user_iovecs(token, iov, iovcnt)?;
        let offset = parse_pos64_halves(pos_l, pos_h)?;
        pwritev_like(&desc, &iovecs, offset)
    })
}

/// read syscall
pub fn sys_read(fd: u32, buf: *const u8, len: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_read, fd = {}",
        current_task().unwrap().process.upgrade().unwrap().getpid(), fd
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_readable_file(fd)?;
        trace!("kernel: sys_read .. desc.read");
        Ok(desc.read_result(UserBuffer::new(
            translated_byte_buffer_with_access(buf, len, PageFaultAccess::Write)?,
        ))? as isize)
    })
}

pub fn sys_lseek(fd: u32, offset: usize, whence: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_lseek, offset={}, whence={}",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        offset,
        whence
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_file_description(fd)?;
        // Combine high/low into a 64-bit pattern and interpret as signed offset.
        Ok(desc.seek(offset as i64, whence as u8)? as isize)
    })
}

/// ioctl 系统调用：校验 fd 后转发到具体文件对象。
pub fn sys_ioctl(fd: u32, req: usize, arg: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_ioctl",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_file_description(fd)?;
        // 具体 request 语义由底层文件对象决定；当前大多数对象会返回 ENOTTY。
        // TODO: tty 实现 `TCGETS/TIOCGWINSZ` 后，这里会开始承载真实终端控制语义。
        // debug!("sys_ioctl: fd = {}, req = {:#x}, arg = {:#x}", fd, req, arg);
        desc.ioctl(req, arg)
    })
}

/// open sysall
pub fn sys_open(dirfd: isize, path: *const u8, flags: i32, mode: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_open",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let token = current_user_token();
    syscall_body!({
        // TODO: 目前只有O_CLOEXEC位会落入FD层处理。
        const O_CLOEXEC: i32 = 0x80000;
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        if path.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
        debug!(
            "sys_open: dirfd = {}, path = {}, flags = {}, cwd = {}",
            dirfd, path, flags, cwd
        );
        let fd_flags = if flags & O_CLOEXEC != 0 {
            FdFlags::CLOEXEC
        } else {
            FdFlags::empty()
        };
        let open_state = filter_open_flags(flags & !O_CLOEXEC)?;
        let (inode, created) = open_file_at_with_status(
            cwd.as_str(),
            path.as_str(),
            open_state.open_flags,
        )?;
        if created {
            // POSIX: mode 仅在“新建”时生效，并受进程 umask 过滤。
            let requested = mode & StatMode::PERM_MASK.bits();
            let umask = process.umask();
            let effective = requested & !umask;
            let old_mode = inode.stat().mode.bits();
            let new_mode = (old_mode & StatMode::TYPE_MASK.bits()) | (effective & StatMode::PERM_MASK.bits());
            if let Some(backing_inode) = inode.as_inode() {
                backing_inode.set_owner(process.geteuid(), process.getegid())?;
                backing_inode.set_mode(new_mode)?;
            }
        }
        if open_state.open_flags.contains(OpenFlags::DIRECTORY) && !inode.is_dir() {
            return Err(ERRNO::ENOTDIR);
        }
        let mut inner = process.inner_exclusive_access();
        let fd = inner.alloc_fd()?;
        let desc = Arc::new(FileDescription::new(
            inode,
            open_state.access_mode,
            open_state.status_flags,
            open_state.status_fixed_bits,
        ));
        let mut entry = FdEntry::new(desc);
        entry.flags = fd_flags;
        inner.fd_table[fd] = Some(entry);
        Ok(fd as isize)
    })
}

/// `truncate(2)`：按路径调整常规文件长度。
pub fn sys_truncate(path: *const u8, len: isize) -> isize {
    trace!(
        "kernel:pid[{}] sys_truncate",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let new_size = parse_truncate_len(len)?;
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        if path.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        debug!("sys_truncate: path='{}', new_size={}", path, new_size);
        let target = resolve_at_target(AT_FDCWD, path.as_str(), 0)?;
        match target {
            ResolvedAtTarget::Inode(inode) => {
                debug!(
                    "sys_truncate: resolved inode fs_id={} ino={}",
                    inode.fs_id(),
                    inode.ino()
                );
                truncate_inode(&inode, new_size).map_err(ERRNO::from)?;
                Ok(0)
            }
            ResolvedAtTarget::FileDesc(_) => Err(ERRNO::EINVAL),
        }
    })
}

/// `ftruncate(2)`：按已打开文件描述调整常规文件长度。
pub fn sys_ftruncate(fd: u32, len: isize) -> isize {
    trace!(
        "kernel:pid[{}] sys_ftruncate",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let new_size = parse_truncate_len(len)?;
        let desc = get_writable_file(fd as usize)?;
        debug!("sys_ftruncate: fd={}, new_size={}", fd, new_size);
        if let Some(inode) = desc.backing_inode() {
            debug!(
                "sys_ftruncate: backing inode fs_id={} ino={}",
                inode.fs_id(),
                inode.ino()
            );
        }
        desc.truncate(new_size)?;
        Ok(0)
    })
}

pub fn sys_eventfd2(_initval: u32, flags: i32) -> isize {
    syscall_body!({
        let (status_flags, cloexec) = parse_anon_fd_flags(flags, O_NONBLOCK | O_CLOEXEC)?;
        alloc_anonymous_fd(status_flags, cloexec)
    })
}

pub fn sys_epoll_create1(flags: i32) -> isize {
    syscall_body!({
        let (status_flags, cloexec) = parse_anon_fd_flags(flags, O_CLOEXEC)?;
        alloc_anonymous_fd(status_flags, cloexec)
    })
}

pub fn sys_inotify_init1(flags: i32) -> isize {
    syscall_body!({
        let (status_flags, cloexec) = parse_anon_fd_flags(flags, O_NONBLOCK | O_CLOEXEC)?;
        alloc_anonymous_fd(status_flags, cloexec)
    })
}

pub fn sys_signalfd4(fd: i32, sigmask: *const u8, _sigsetsize: usize, flags: i32) -> isize {
    syscall_body!({
        if fd != -1 {
            return Err(ERRNO::EINVAL);
        }
        if sigmask.is_null() {
            return Err(ERRNO::EFAULT);
        }
        let (status_flags, cloexec) = parse_anon_fd_flags(flags, O_NONBLOCK | O_CLOEXEC)?;
        alloc_anonymous_fd(status_flags, cloexec)
    })
}

pub fn sys_timerfd_create(_clockid: i32, flags: i32) -> isize {
    syscall_body!({
        let (status_flags, cloexec) = parse_anon_fd_flags(flags, O_NONBLOCK | O_CLOEXEC)?;
        alloc_anonymous_fd(status_flags, cloexec)
    })
}

pub fn sys_perf_event_open(
    attr_uptr: usize,
    _pid: isize,
    _cpu: isize,
    _group_fd: isize,
    _flags: u32,
) -> isize {
    syscall_body!({
        if attr_uptr == 0 {
            return Err(ERRNO::EFAULT);
        }
        alloc_anonymous_fd(FileStatusFlags::empty(), false)
    })
}

pub fn sys_fanotify_init(_flags: u32, _event_f_flags: u32) -> isize {
    syscall_body!({
        alloc_anonymous_fd(FileStatusFlags::empty(), false)
    })
}

pub fn sys_memfd_create(name: *const u8, flags: u32) -> isize {
    syscall_body!({
        if name.is_null() {
            return Err(ERRNO::EFAULT);
        }
        let _ = read_cstring_from_user(name, PATH_MAX)?;
        let (status_flags, cloexec) = parse_anon_fd_flags(flags as i32, O_CLOEXEC)?;
        alloc_anonymous_fd(status_flags, cloexec)
    })
}

pub fn sys_bpf(cmd: u32, attr: usize, _size: u32) -> isize {
    syscall_body!({
        if attr == 0 {
            return Err(ERRNO::EFAULT);
        }
        match cmd {
            BPF_MAP_CREATE => {
                let attr = read_pod_from_user(attr as *const BpfMapCreateAttr)?;
                alloc_bpf_map_fd(BpfMapFile::new(attr)?)
            }
            BPF_MAP_LOOKUP_ELEM => {
                let attr = read_pod_from_user(attr as *const BpfMapElemAttr)?;
                let desc = bpf_map_from_fd(attr.map_fd)?;
                let map = desc
                    .as_any()
                    .downcast_ref::<BpfMapFile>()
                    .ok_or(ERRNO::EBADF)?;
                map.lookup_elem(attr)?;
                Ok(0)
            }
            BPF_MAP_UPDATE_ELEM => {
                let attr = read_pod_from_user(attr as *const BpfMapElemAttr)?;
                let desc = bpf_map_from_fd(attr.map_fd)?;
                let map = desc
                    .as_any()
                    .downcast_ref::<BpfMapFile>()
                    .ok_or(ERRNO::EBADF)?;
                map.update_elem(attr)?;
                Ok(0)
            }
            BPF_PROG_LOAD => {
                let attr = read_pod_from_user(attr as *const BpfProgLoadAttr)?;
                alloc_bpf_prog_fd(BpfProgFile::from_load_attr(attr)?)
            }
            _ => Err(ERRNO::EINVAL),
        }
    })
}

pub fn sys_userfaultfd(flags: i32) -> isize {
    syscall_body!({
        let (status_flags, cloexec) = parse_anon_fd_flags(flags, O_CLOEXEC | O_NONBLOCK)?;
        alloc_anonymous_fd(status_flags, cloexec)
    })
}

pub fn sys_pidfd_open(_pid: isize, flags: u32) -> isize {
    syscall_body!({
        if flags != 0 {
            return Err(ERRNO::EINVAL);
        }
        alloc_anonymous_fd(FileStatusFlags::empty(), false)
    })
}

pub fn sys_io_uring_setup(_entries: u32, params: usize) -> isize {
    syscall_body!({
        if params == 0 {
            return Err(ERRNO::EFAULT);
        }
        alloc_anonymous_fd(FileStatusFlags::empty(), false)
    })
}

pub fn sys_open_tree(_dirfd: isize, path: *const u8, flags: u32) -> isize {
    const O_PATH: i32 = 0x200000;
    syscall_body!({
        if path.is_null() {
            return Err(ERRNO::EFAULT);
        }
        let _ = read_cstring_from_user(path, PATH_MAX)?;
        if flags != 0 {
            return Err(ERRNO::EINVAL);
        }
        alloc_anonymous_fd_with_bits(FileStatusFlags::empty(), false, O_PATH)
    })
}

pub fn sys_fsopen(fsname: *const u8, flags: u32) -> isize {
    syscall_body!({
        if fsname.is_null() {
            return Err(ERRNO::EFAULT);
        }
        let _ = read_cstring_from_user(fsname, PATH_MAX)?;
        if flags != 0 {
            return Err(ERRNO::EINVAL);
        }
        alloc_anonymous_fd(FileStatusFlags::empty(), false)
    })
}

pub fn sys_fspick(_dirfd: isize, path: *const u8, flags: u32) -> isize {
    syscall_body!({
        if path.is_null() {
            return Err(ERRNO::EFAULT);
        }
        let _ = read_cstring_from_user(path, PATH_MAX)?;
        if flags != 0 {
            return Err(ERRNO::EINVAL);
        }
        alloc_anonymous_fd(FileStatusFlags::empty(), false)
    })
}

pub fn sys_memfd_secret(flags: u32) -> isize {
    syscall_body!({
        if flags != 0 {
            return Err(ERRNO::EINVAL);
        }
        alloc_anonymous_fd(FileStatusFlags::empty(), false)
    })
}

/// close syscall
pub fn sys_close(fd: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_close",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let closed_entry = {
        let mut inner = process.inner_exclusive_access();
        let mut closed_entry: Option<FdEntry> = None;
        let result = syscall_body!({
            let fd = fd as usize;
            if fd >= inner.fd_table.len() {
                return Err(ERRNO::EBADF);
            }
            if inner.fd_table[fd].is_none() {
                return Err(ERRNO::EBADF);
            }
            // 先摘表项，等离开 `process.inner` 后再真正 drop，避免自旋锁内阻塞。
            let entry = inner.take_fd(fd).ok_or(ERRNO::EBADF)?;
            closed_entry = Some(entry);
            Ok(0)
        });
        if result != 0 {
            return result;
        }
        closed_entry
    };
    drop(closed_entry);
    0
}

/// sync syscall
pub fn sys_sync() -> isize {
    syscall_body!({
        sync_page_cache_all()?;
        Ok(0)
    })
}

/// fsync syscall
pub fn sys_fsync(fd: u32) -> isize {
    syscall_body!({
        let file = get_any_file(fd as usize)?;
        file.sync()?;
        Ok(0)
    })
}

/// fdatasync syscall
pub fn sys_fdatasync(fd: u32) -> isize {
    syscall_body!({
        sync_page_cache_all()?;
        Ok(0)
    })
}

/// syncfs syscall
pub fn sys_syncfs(fd: u32) -> isize {
    syscall_body!({
        let file = get_any_file(fd as usize)?;
        let inode = file.backing_inode().ok_or(ERRNO::EINVAL)?;
        sync_page_cache_fs(inode.fs_id())?;
        Ok(0)
    })
}

/// pipe syscall
pub fn sys_pipe2(pipefd: *mut i32, _flags: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_pipe",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    syscall_body!({
        let mut inner = process.inner_exclusive_access();
        inner.ensure_fd_capacity(2)?;
        let (pipe_read, pipe_write) = make_pipe();
        let read_fd = inner.alloc_fd()?;
        inner.fd_table[read_fd] = Some(FdEntry::new(Arc::new(FileDescription::new(
            pipe_read,
            AccessMode::ReadOnly,
            FileStatusFlags::empty(),
            0,
        ))));
        let write_fd = inner.alloc_fd()?;
        inner.fd_table[write_fd] = Some(FdEntry::new(Arc::new(FileDescription::new(
            pipe_write,
            AccessMode::WriteOnly,
            FileStatusFlags::empty(),
            0,
        ))));
        drop(inner);
        write_pod_to_user(pipefd, &(read_fd as i32))?;
        write_pod_to_user(unsafe { pipefd.add(1) }, &(write_fd as i32))?;
        debug!("sys_pipe: read_fd = {}, write_fd = {}", read_fd, write_fd);
        Ok(0)
    })
}

/// dup syscall
pub fn sys_dup(fd: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_dup",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    syscall_body!({
        let fd = fd as usize;
        if fd >= inner.fd_table.len() {
            return Err(ERRNO::EBADF);
        }
        if inner.fd_table[fd].is_none() {
            return Err(ERRNO::EBADF);
        }
        let new_fd = inner.alloc_fd()?;
        let desc = Arc::clone(&inner.fd_table[fd].as_ref().unwrap().desc);
        inner.fd_table[new_fd] = Some(FdEntry {
            desc,
            flags: FdFlags::empty(),
        });
        Ok(new_fd as isize)
    })
}

/// dup2 syscall
pub fn sys_dup2(oldfd: u32, newfd: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_dup2",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    let (result, replaced_entry) = {
        let mut inner = process.inner_exclusive_access();
        let mut replaced_entry: Option<FdEntry> = None;
        let result = syscall_body!({
            let oldfd = oldfd as usize;
            let newfd = newfd as usize;
            if oldfd >= inner.fd_table.len() {
                return Err(ERRNO::EBADF);
            }
            if inner.fd_table[oldfd].is_none() {
                return Err(ERRNO::EBADF);
            }
            if oldfd == newfd {
                return Ok(newfd as isize);
            }
            let newfd_is_occupied = inner.fd_table.get(newfd).and_then(|slot| slot.as_ref()).is_some();
            if !newfd_is_occupied {
                let allocated = inner.alloc_fd_from(newfd)?;
                debug_assert_eq!(allocated, newfd);
            }
            // 先把旧 `newfd` 表项拿出来，等离开进程自旋锁后再 drop。
            replaced_entry = inner.take_fd(newfd);
            let desc = Arc::clone(&inner.fd_table[oldfd].as_ref().unwrap().desc);
            inner.fd_table[newfd] = Some(FdEntry {
                desc,
                flags: FdFlags::empty(),
            });
            Ok(newfd as isize)
        });
        (result, replaced_entry)
    };
    drop(replaced_entry);
    result
}

/// fstat syscall
pub fn sys_fstat(fd: u32, st: *mut Stat) -> isize {
    trace!(
        "kernel:pid[{}] sys_fstat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_file_description(fd)?;
        let stat = desc.stat();
        let path = desc.path();
        debug!(
            "sys_fstat: fd={} size={} blksize={} blocks={} mode={:#o} path={:?}",
            fd,
            stat.size,
            stat.blksize,
            stat.blocks,
            stat.mode.bits(),
            path
        );
        if stat.blksize >= SUSPICIOUS_STAT_BLKSIZE {
            warn!(
                "sys_fstat: suspicious st_blksize fd={} blksize={} size={} path={:?}",
                fd,
                stat.blksize,
                stat.size,
                path
            );
        }
        write_pod_to_user(st, &stat)?;
        Ok(0)
    })
}

/// `newfstatat` 系统调用：按目录 fd 与路径查询文件元数据。
pub fn sys_newfstatat(dirfd: isize, path: *const u8, st: *mut Stat, flags: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_newfstatat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let flags = flags as u32;
        let supported_flags = AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH;
        if flags & !supported_flags != 0 {
            return Err(ERRNO::EINVAL);
        }
        let path = if path.is_null() {
            if flags & AT_EMPTY_PATH == 0 {
                return Err(ERRNO::EFAULT);
            }
            String::new()
        } else {
            read_cstring_from_user(path, PATH_MAX)?
        };
        if path.is_empty() {
            if flags & AT_EMPTY_PATH == 0 {
                return Err(ERRNO::ENOENT);
            }
            // Use the unified resolver which returns either an inode or an
            // open FileDescription (for non-inode descriptors like pipes).
            match resolve_at_target(dirfd, "", flags as i32)? {
                ResolvedAtTarget::Inode(inode) => {
                    let stat = inode_stat(&inode);
                    debug!(
                        "sys_newfstatat: empty-path inode dirfd={} flags={:#x} size={} blksize={} blocks={} mode={:#o}",
                        dirfd,
                        flags,
                        stat.size,
                        stat.blksize,
                        stat.blocks,
                        stat.mode.bits()
                    );
                    write_pod_to_user(st, &stat)?;
                    return Ok(0);
                }
                ResolvedAtTarget::FileDesc(desc) => {
                    let stat = desc.stat();
                    let desc_path = desc.path();
                    debug!(
                        "sys_newfstatat: empty-path fd dirfd={} flags={:#x} size={} blksize={} blocks={} mode={:#o} path={:?}",
                        dirfd,
                        flags,
                        stat.size,
                        stat.blksize,
                        stat.blocks,
                        stat.mode.bits(),
                        desc_path
                    );
                    write_pod_to_user(st, &stat)?;
                    return Ok(0);
                }
            }
        }
let time1 = get_time_us();
        let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
let time2 = get_time_us();
        let lookup_path = rooted_lookup_path(path.as_str());
        let inode = lookup_inode_follow(cwd.as_str(), lookup_path, flags & AT_SYMLINK_NOFOLLOW == 0)?;
let time3 = get_time_us();
        let stat = inode_stat(&inode);
let time4 = get_time_us();
        debug!(
            "sys_newfstatat: dirfd={} path='{}' flags={:#x} size={} blksize={} blocks={} mode={:#o}",
            dirfd,
            path,
            flags,
            stat.size,
            stat.blksize,
            stat.blocks,
            stat.mode.bits()
        );
        if stat.blksize >= SUSPICIOUS_STAT_BLKSIZE {
            warn!(
                "sys_newfstatat: suspicious st_blksize dirfd={} path='{}' blksize={} size={}",
                dirfd,
                path,
                stat.blksize,
                stat.size
            );
        }
        write_pod_to_user(st, &stat)?;
let time5 = get_time_us();
        debug!("sys_newfstatat: resolve_dirfd_base & canonicalize = {}us, lookup_inode = {}us, inode_stat = {}us, write_pod_to_user = {}us",
            time2 - time1, time3 - time2, time4 - time3, time5 - time4);
        Ok(0)
    })
}

/// `statx(2)` 系统调用：按目录 fd 与路径查询增强版文件元数据。
pub fn sys_statx(
    dirfd: isize,
    path: *const u8,
    flags: i32,
    mask: u32,
    stx: *mut Statx,
) -> isize {
    trace!(
        "kernel:pid[{}] sys_statx",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let flags = flags as u32;
        let supported_flags =
            AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH | AT_NO_AUTOMOUNT | AT_STATX_SYNC_TYPE;
        if flags & !supported_flags != 0 {
            return Err(ERRNO::EINVAL);
        }
        let mask = StatxMask::from_bits(mask).ok_or(ERRNO::EINVAL)?;
        let path = if path.is_null() {
            if flags & AT_EMPTY_PATH == 0 {
                return Err(ERRNO::EFAULT);
            }
            String::new()
        } else {
            read_cstring_from_user(path, PATH_MAX)?
        };

        let stat = if path.is_empty() {
            if flags & AT_EMPTY_PATH == 0 {
                return Err(ERRNO::ENOENT);
            }
            match resolve_at_target(dirfd, "", flags as i32)? {
                ResolvedAtTarget::Inode(inode) => inode_stat(&inode),
                ResolvedAtTarget::FileDesc(desc) => desc.stat(),
            }
        } else {
            let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
            let inode = lookup_inode_follow(
                cwd.as_str(),
                rooted_lookup_path(path.as_str()),
                flags & AT_SYMLINK_NOFOLLOW == 0,
            )?;
            inode_stat(&inode)
        };

        let statx = stat_to_statx(&stat, mask);
        write_pod_to_user(stx, &statx)?;
        Ok(0)
    })
}

/// `faccessat` 系统调用：按目录 fd 与路径检查可访问性。
pub fn sys_faccessat(dirfd: isize, path: *const u8, mode: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_faccessat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        if mode & !(R_OK | W_OK | X_OK) != 0 {
            return Err(ERRNO::EINVAL);
        }

        let path = read_cstring_from_user(path, PATH_MAX)?;
        if path.is_empty() {
            return Err(ERRNO::ENOENT);
        }

        let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
        let process = current_process();
        let uid = process.getuid();
        let gid = process.getgid();
        check_path_search_permissions(cwd.as_str(), path.as_str(), uid, gid)?;
        let lookup_path = rooted_lookup_path(path.as_str());
        let inode = lookup_inode_follow(cwd.as_str(), lookup_path, true)?;
        if mode == F_OK {
            return Ok(0);
        }
        let abs_path = canonicalize(cwd.as_str(), lookup_path);
        if mode & W_OK != 0 && mount_is_readonly(abs_path.as_str()) {
            return Err(ERRNO::EROFS);
        }

        if inode_allows_access(&inode, uid, gid, mode as u32) {
            Ok(0)
        } else {
            Err(ERRNO::EACCES)
        }
    })
}

/// `faccessat2` 系统调用：带 flags 的路径可访问性检查。
pub fn sys_faccessat2(dirfd: isize, path: *const u8, mode: i32, flags: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_faccessat2",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    const AT_EACCESS: i32 = 0x200;
    const SUPPORTED_FLAGS: i32 = AT_EACCESS | AT_EMPTY_PATH as i32 | AT_SYMLINK_NOFOLLOW as i32;
    syscall_body!({
        if mode & !(R_OK | W_OK | X_OK) != 0 {
            return Err(ERRNO::EINVAL);
        }
        if flags & !SUPPORTED_FLAGS != 0 {
            return Err(ERRNO::EINVAL);
        }

        let path = read_cstring_from_user(path, PATH_MAX)?;
        if path.is_empty() && flags & AT_EMPTY_PATH as i32 == 0 {
            return Err(ERRNO::ENOENT);
        }

        let target = resolve_at_target(dirfd, path.as_str(), flags)?;
        if mode == F_OK {
            return Ok(0);
        }

        let process = current_process();
        let uid = if flags & AT_EACCESS != 0 {
            process.geteuid()
        } else {
            process.getuid()
        };
        let gid = if flags & AT_EACCESS != 0 {
            process.getegid()
        } else {
            process.getgid()
        };

        match target {
            ResolvedAtTarget::Inode(inode) => {
                if mode & W_OK != 0 && !path.is_empty() {
                    let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
                    let abs_path = canonicalize(cwd.as_str(), rooted_lookup_path(path.as_str()));
                    if mount_is_readonly(abs_path.as_str()) {
                        return Err(ERRNO::EROFS);
                    }
                }
                if inode_allows_access(&inode, uid, gid, mode as u32) {
                    Ok(0)
                } else {
                    Err(ERRNO::EACCES)
                }
            }
            ResolvedAtTarget::FileDesc(desc) => {
                let readable = mode & R_OK == 0 || desc.readable();
                let writable = mode & W_OK == 0 || desc.writable();
                if readable && writable {
                    Ok(0)
                } else {
                    Err(ERRNO::EACCES)
                }
            }
        }
    })
}

/// linkat syscall
pub fn sys_linkat(
    old_dirfd: isize,
    old_name: *const u8,
    new_dirfd: isize,
    new_name: *const u8,
    flags: u32,
) -> isize {
    trace!(
        "kernel:pid[{}] sys_linkat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        if flags & !AT_SYMLINK_FOLLOW != 0 {
            return Err(ERRNO::EINVAL);
        }
        let old_path = translated_str(token, old_name).or_errno(ERRNO::EFAULT)?;
        let new_path = translated_str(token, new_name).or_errno(ERRNO::EFAULT)?;
        if old_path.is_empty() || new_path.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        if old_path == new_path {
            return Err(ERRNO::EINVAL);
        }
        let old_cwd = resolve_dirfd_base(old_dirfd, old_path.as_str())?;
        let new_cwd = resolve_dirfd_base(new_dirfd, new_path.as_str())?;
        linkat_with_flags(old_cwd.as_str(), &old_path, new_cwd.as_str(), &new_path, flags)?;
        Ok(0)
    })
}

/// symlinkat syscall
pub fn sys_symlinkat(target: *const u8, new_dirfd: isize, linkpath: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_symlinkat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let target = translated_str(token, target).or_errno(ERRNO::EFAULT)?;
        let linkpath = translated_str(token, linkpath).or_errno(ERRNO::EFAULT)?;
        if target.is_empty() || linkpath.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        let cwd = resolve_dirfd_base(new_dirfd, linkpath.as_str())?;
        symlinkat(target.as_str(), cwd.as_str(), linkpath.as_str())?;
        Ok(0)
    })
}

/// readlinkat syscall
pub fn sys_readlinkat(dirfd: isize, path: *const u8, buf: *mut u8, bufsiz: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_readlinkat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        if bufsiz == 0 {
            return Err(ERRNO::EINVAL);
        }
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        if path.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
        let inode = lookup_inode_follow(cwd.as_str(), rooted_lookup_path(path.as_str()), false)?;
        if !inode.is_symlink() {
            return Err(ERRNO::EINVAL);
        }
        let target = inode.read_link()?;
        let bytes = target.as_bytes();
        let n = bytes.len().min(bufsiz);
        write_bytes_to_user(buf, &bytes[..n])?;
        Ok(n as isize)
    })
}

/// unlinkat syscall
pub fn sys_unlinkat(dirfd: isize, name: *const u8, flags: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_unlinkat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        if flags & !AT_REMOVEDIR != 0 {
            return Err(ERRNO::EINVAL);
        }
        let name = translated_str(token, name).or_errno(ERRNO::EFAULT)?;
        if name.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        if let Some(parent) = resolve_simple_dirfd_inode(dirfd, name.as_str())? {
            if flags & AT_REMOVEDIR == 0 {
                let inode = parent.find(name.as_str()).ok_or(ERRNO::ENOENT)?;
                if inode.is_dir() {
                    return Err(ERRNO::EISDIR);
                }
                discard_inode(&inode);
                parent.unlink(name.as_str())?;
            } else {
                let inode = parent.find(name.as_str()).ok_or(ERRNO::ENOENT)?;
                if !inode.is_dir() {
                    return Err(ERRNO::ENOTDIR);
                }
                parent.rmdir(name.as_str())?;
            }
        } else {
            let cwd = resolve_dirfd_base(dirfd, name.as_str())?;
            unlinkat(cwd.as_str(), &name, flags)?;
        }
        Ok(0)
    })
}

/// getcwd – copy the current working directory into a user-space buffer.
///
/// Returns the buffer address as `isize` on success, −errno on failure.
pub fn sys_getcwd(buf: *mut u8, size: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_getcwd",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        if size == 0 || buf.is_null() {
            return Err(ERRNO::EINVAL);
        }
        let cwd = current_process().inner_exclusive_access().cwd.clone();
        let cwd_bytes = cwd.as_bytes();
        if size < cwd_bytes.len() + 1 {
            return Err(ERRNO::ERANGE);
        }
        // Write cwd + null terminator into the user buffer in one pass.
        let total = cwd_bytes.len() + 1;
        let src: Vec<u8> = cwd_bytes
            .iter()
            .copied()
            .chain(core::iter::once(0u8))
            .collect();
        debug_assert_eq!(src.len(), total);
        write_bytes_to_user(buf, &src)?;
        Ok(buf as isize)
    })
}

/// mkdirat – create a directory at `path` relative to the provided directory fd.
///
/// `mode` 仅在创建时使用，并受进程 `umask` 过滤。
/// Returns 0 on success, −errno on failure.
pub fn sys_mkdirat(dirfd: isize, path: *const u8, mode: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_mkdirat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let path = translated_str(token, path).or_errno(ERRNO::EFAULT)?;
        if path.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        let dir_inode = if let Some(parent) = resolve_simple_dirfd_inode(dirfd, path.as_str())? {
            if parent.find(path.as_str()).is_some() {
                return Err(ERRNO::EEXIST);
            }
            parent.mkdir(path.as_str()).ok_or(ERRNO::EIO)?
        } else {
            let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
            mkdir_at_with_inode(cwd.as_str(), path.as_str())?
        };
        let process = current_process();
        dir_inode.set_owner(process.geteuid(), process.getegid())?;
        let requested = mode & StatMode::PERM_MASK.bits();
        let umask = process.umask();
        let effective = requested & !umask;
        let old_mode = inode_stat(&dir_inode).mode.bits();
        let new_mode = (old_mode & StatMode::TYPE_MASK.bits()) | (effective & StatMode::PERM_MASK.bits());
        dir_inode.set_mode(new_mode)?;
        Ok(0)
    })
}

/// chdir – change the current working directory.
///
/// Resolves `path` relative to the current CWD, verifies that the result
/// exists and is a directory, then updates the process CWD.
/// Returns 0 on success, −errno on failure.
pub fn sys_chdir(path: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_chdir",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let process = current_process();
    syscall_body!({
        let path = read_cstring_from_user(path, PATH_MAX)?;
        if path.split('/').any(|component| component.len() > NAME_MAX) {
            return Err(ERRNO::ENAMETOOLONG);
        }
        let cwd = process.inner_exclusive_access().cwd.clone();
        let (inode, new_abs) = lookup_inode_follow_with_path(cwd.as_str(), path.as_str(), true)?;
        if !inode.is_dir() {
            warn!("sys_chdir: target '{}' resolved to '{}', which is not a directory",
                path, new_abs);
            return Err(ERRNO::ENOTDIR);
        }
        process.inner_exclusive_access().cwd = new_abs;
        Ok(0)
    })
}

/// chroot – change the process root directory for absolute path resolution.
pub fn sys_chroot(path: *const u8) -> isize {
    trace!(
        "kernel:pid[{}] sys_chroot",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let path = read_cstring_from_user(path, PATH_MAX)?;
        if path.split('/').any(|component| component.len() > NAME_MAX) {
            return Err(ERRNO::ENAMETOOLONG);
        }

        let cwd = resolve_dirfd_base(AT_FDCWD, path.as_str())?;
        let lookup_path = rooted_lookup_path(path.as_str());
        let process = current_process();
        let euid = process.geteuid();
        let egid = process.getegid();
        check_path_search_permissions(cwd.as_str(), lookup_path, euid, egid)?;
        let (inode, new_root) = lookup_inode_follow_with_path(cwd.as_str(), lookup_path, true)?;
        if !inode.is_dir() {
            return Err(ERRNO::ENOTDIR);
        }
        if !inode_allows_access(&inode, euid, egid, X_OK as u32) {
            return Err(ERRNO::EACCES);
        }
        if euid != 0 {
            return Err(ERRNO::EPERM);
        }

        process.inner_exclusive_access().root = new_root;
        Ok(0)
    })
}

/// getdents64 – read directory entries from an open directory file descriptor.
///
/// The caller provides a `buf` of `count` bytes.  The kernel writes as many
/// `linux_dirent64` records as fit, advancing the fd's internal entry-index.
/// Returns the number of bytes written, 0 when the directory is exhausted,
/// or −1 on error.
///
/// Each `linux_dirent64` record:
/// ```text
///   +0   d_ino    u64  (stable inode number, non-zero for valid entries)
///   +8   d_off    i64  (directory position of the *next* record)
///   +16  d_reclen u16  (total record length, multiple of 8)
///   +18  d_type   u8   (DT_DIR = 4, DT_REG = 8, DT_UNKNOWN = 0)
///   +19  d_name[] null-terminated name, zero-padded to meet alignment
/// ```
pub fn sys_getdents64(fd: u32, buf: *mut u8, count: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_getdents64",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let fd = fd as usize;
        let desc = get_file_description(fd)?;
        // Fill a kernel-side temporary buffer …
        let mut tmp: Vec<u8> = vec![0; count];
let start = get_time_us();
        let bytes = desc.getdents64(&mut tmp);
let end = get_time_us();
debug!("sys_getdents64: fd={}, count={}, time_us={}", fd, count, end - start);
        if bytes == 0 {
            return Ok(0);
        }
        // … then copy to user space.
        write_bytes_to_user(buf, &tmp[..bytes])?;
        Ok(bytes as isize)
    })
}

pub fn sys_utimensat(dirfd: isize, path: *const u8, times: *const Timespec, flags: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_utimensat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );

    let token = current_user_token();
    syscall_body!({
        let supported_flags = (AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) as i32;
        if flags & !supported_flags != 0 {
            return Err(ERRNO::EINVAL);
        }
        // `futimens(2)` is commonly implemented via `utimensat(fd, NULL, ...)`.
        // Treat that specific shape as an empty-path operation on `dirfd`.
        let futimens_null_path = path.is_null() && dirfd != AT_FDCWD && (flags & AT_EMPTY_PATH as i32 == 0);
        let effective_flags = if futimens_null_path {
            flags | AT_EMPTY_PATH as i32
        } else {
            flags
        };
        let path = if path.is_null() && (effective_flags & AT_EMPTY_PATH as i32 != 0) {
            String::new()
        } else {
            translated_str(token, path).or_errno(ERRNO::EFAULT)?
        };
        debug!(
            "sys_utimensat: dirfd = {}, path = {}, flags = {}, effective_flags = {}",
            dirfd, path, flags, effective_flags
        );
        let target = resolve_at_target(dirfd, path.as_str(), effective_flags)?;
        let inode = match target {
            ResolvedAtTarget::Inode(i) => i,
            ResolvedAtTarget::FileDesc(_) => return Err(ERRNO::EBADF),
        };

        let now_ns = get_realtime_ns();
        let now = InodeTime::new(now_ns / 1_000_000_000, (now_ns % 1_000_000_000) as u32);

        let (atime_req, mtime_req) = if times.is_null() {
            (UtimeArg::Now, UtimeArg::Now)
        } else {
            let pair = copy_user_timespec_pair(token, times)?;
            (parse_utime_arg(pair[0])?, parse_utime_arg(pair[1])?)
        };

        let atime = match atime_req {
            UtimeArg::Now => Some(now),
            UtimeArg::Omit => None,
            UtimeArg::Set(ts) => Some(ts),
        };
        let mtime = match mtime_req {
            UtimeArg::Now => Some(now),
            UtimeArg::Omit => None,
            UtimeArg::Set(ts) => Some(ts),
        };

        if atime.is_none() && mtime.is_none() {
            return Ok(0);
        }

        inode.set_times(atime, mtime, Some(now))?;
        Ok(0)
    })
}

/// fchown(fd, user, group) — change ownership of the file referred to by `fd`.
pub fn sys_fchown(fd: u32, user: u32, group: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_fchown",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let target = resolve_at_target(fd as isize, "", AT_EMPTY_PATH as i32)?;
        let inode = match target {
            ResolvedAtTarget::Inode(i) => i,
            ResolvedAtTarget::FileDesc(_) => return Err(ERRNO::EBADF),
        };

        chown_inode(&inode, user, group)?;
        Ok(0)
    })
}

/// fchownat(dirfd, pathname, user, group, flags) — change ownership of a path-relative target.
pub fn sys_fchownat(dirfd: isize, pathname: *const u8, user: u32, group: u32, flags: i32) -> isize {
    trace!(
        "kernel:pid[{}] sys_fchownat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let supported_flags = (AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) as i32;
        if flags & !supported_flags != 0 {
            return Err(ERRNO::EINVAL);
        }

        let path = if pathname.is_null() && (flags & AT_EMPTY_PATH as i32 != 0) {
            String::new()
        } else {
            read_cstring_from_user(pathname, PATH_MAX)?
        };
        if !path.is_empty() && path.split('/').any(|component| component.len() > NAME_MAX) {
            return Err(ERRNO::ENAMETOOLONG);
        }

        if !path.is_empty() {
            let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
            let process = current_process();
            check_path_search_permissions(cwd.as_str(), path.as_str(), process.geteuid(), process.getegid())?;
            let abs_path = canonicalize(cwd.as_str(), rooted_lookup_path(path.as_str()));
            if mount_is_readonly(abs_path.as_str()) {
                return Err(ERRNO::EROFS);
            }
        }

        let target = resolve_at_target(dirfd, path.as_str(), flags)?;
        let inode = match target {
            ResolvedAtTarget::Inode(i) => i,
            ResolvedAtTarget::FileDesc(desc) => {
                if desc.stat().mode.bits() & StatMode::TYPE_MASK.bits() == StatMode::SOCK.bits() {
                    return Err(ERRNO::ENOENT);
                }
                return Err(ERRNO::EBADF);
            }
        };

        chown_inode(&inode, user, group)?;
        Ok(0)
    })
}

/// fchmod(fd, mode) — change permissions of the file referred to by `fd`.
pub fn sys_fchmod(fd: u32, mode: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_fchmod",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let target = resolve_at_target(fd as isize, "", AT_EMPTY_PATH as i32)?;
        let inode = match target {
            ResolvedAtTarget::Inode(i) => i,
            ResolvedAtTarget::FileDesc(_) => return Err(ERRNO::EBADF),
        };

        chmod_inode(&inode, mode)?;
        Ok(0)
    })
}

/// fchmodat(dirfd, pathname, mode, flags) — change permissions of a path-relative target.
pub fn sys_fchmodat(dirfd: isize, pathname: *const u8, mode: u32) -> isize {
    trace!(
        "kernel:pid[{}] sys_fchmodat",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    debug!("sys_fchmodat: dirfd = {}, pathname = {:?}, mode = {:#o}", dirfd, pathname, mode);
    syscall_body!({
        let path = read_cstring_from_user(pathname, PATH_MAX)?;
        if path.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        if path.split('/').any(|component| component.len() > NAME_MAX) {
            return Err(ERRNO::ENAMETOOLONG);
        }

        let cwd = resolve_dirfd_base(dirfd, path.as_str())?;
        let process = current_process();
        check_path_search_permissions(cwd.as_str(), path.as_str(), process.geteuid(), process.getegid())?;
        let lookup_path = rooted_lookup_path(path.as_str());
        let abs_path = canonicalize(cwd.as_str(), lookup_path);
        if mount_is_readonly(abs_path.as_str()) {
            return Err(ERRNO::EROFS);
        }
        let inode = lookup_inode_follow(cwd.as_str(), lookup_path, true)?;

        chmod_inode(&inode, mode)?;
        debug!("sys_fchmodat: ok");
        Ok(0)
    })
}


pub fn sys_mount(
    dev_name: *const u8,
    dir_name: *const u8,
    fs_type: *const u8,
    _flags: usize,
    data: *const u8,
) -> isize {
    trace!(
        "kernel:pid[{}] sys_mount",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let dev_name = if dev_name.is_null() {
            String::new()
        } else {
            read_cstring_from_user(dev_name, PATH_MAX)?
        };
        let dir_name = read_cstring_from_user(dir_name, PATH_MAX)?;
        let fs_type  = if fs_type.is_null() {
            String::new()
        } else {
            read_cstring_from_user(fs_type, PATH_MAX)?
        };
        // `data` is typically NULL (e.g. mount(…, NULL)); skip translation if so.
        let _data: String = if data.is_null() {
            String::new()
        } else {
            read_cstring_from_user(data, PATH_MAX)?
        };

        let cwd     = current_process().inner_exclusive_access().cwd.clone();
        let abs_mnt = canonicalize(&cwd, &dir_name);
        let readonly = _flags & MS_RDONLY != 0;
        let remount = _flags & MS_REMOUNT != 0;
        debug!(
            "sys_mount: dev_name = {}, dir_name = {}, abs_mnt = {}, fs_type = {}, flags = {:#x}, data = {}",
            dev_name,
            dir_name,
            abs_mnt,
            fs_type,
            _flags,
            _data
        );
        if (_flags & MS_MOVE) != 0 {
            let source = if dev_name.is_empty() { return Err(ERRNO::EINVAL) } else { dev_name.as_str() };
            do_move_mount(source, abs_mnt.as_str())?;
            return Ok(0);
        }
        if remount {
            let source = if dev_name.is_empty() { "tmpfs" } else { dev_name.as_str() };
            remount_path(&abs_mnt, source, fs_type.as_str(), readonly)?;
        } else if (_flags & MS_BIND) != 0 {
            let source = if dev_name.is_empty() { return Err(ERRNO::EINVAL) } else { dev_name.as_str() };
            do_bind_mount(source, abs_mnt.as_str())?;
        } else if (_flags & (MS_PRIVATE | MS_SHARED | MS_SLAVE | MS_UNBINDABLE)) != 0 {
            let _ = _flags & MS_REC;
            return Ok(0);
        } else if fs_type == "tmpfs" {
            mount_tmpfs(&abs_mnt, readonly)?;
        } else if fs_type == "cgroup2" {
            mount_cgroup2(&abs_mnt, readonly)?;
        } else if fs_type == "sysfs" {
            if abs_mnt == "/sys" {
                remount_path(&abs_mnt, "sysfs", "sysfs", readonly).or_else(|_| {
                    mount_sysfs(&abs_mnt, readonly)
                })?;
            } else {
                mount_sysfs(&abs_mnt, readonly)?;
            }
        } else {
            mount_device(&dev_name, &abs_mnt, &fs_type, readonly)?;
        }
        Ok(0)
    })
}

pub fn sys_umount(name: *const u8, _flags: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_umount",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let name = translated_str(token, name).or_errno(ERRNO::EFAULT)?;
        let cwd  = current_process().inner_exclusive_access().cwd.clone();
        let abs  = canonicalize(&cwd, &name);
        do_umount(&abs)?;
        Ok(0)
    })
}

/// `ppoll_time32(2)`：在 fd 集上等待事件，支持 32 位 timespec 与临时信号掩码。
/// sigmask 按 Linux sigset_t 处理，内核内部统一保存为 64 位 SignalBit。
pub fn sys_ppoll_time32(
    ufds: *mut PollFd,
    nfds: u32,  // length of ufds
    tmo_p: *const OldTimespec32,    // timeout, NULL for infinite
    sigmask: *const u8,
    sigsetsize: usize,  // length of sigmasks
) -> isize {
    trace!(
        "kernel:pid[{}] sys_ppoll_time32",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let mut pollfds = copy_user_pollfds(token, ufds, nfds as usize)?;
        let timeout_ns = parse_timeout_ns(tmo_p)?;
        let deadline_ns = timeout_ns_to_deadline_ns(timeout_ns)?;
        let pid = current_task().unwrap().process.upgrade().unwrap().getpid();

        let old_mask = apply_temp_signal_mask(sigmask, sigsetsize, "sys_ppoll_time32")?;
        let ret = poll_wait_loop_with_writeback(pid, &mut pollfds, deadline_ns, |polled| {
            write_back_pollfds(ufds, polled)
        });
        restore_temp_signal_mask(old_mask);
        ret
    })
}

/// `pselect6_time32(2)`：在 `fd_set` 上等待事件，支持 32 位 timespec 与临时信号掩码。
///
/// 第 6 个参数遵循 Linux `pselect6` 约定：
/// `struct { const sigset_t *ss; size_t ss_len; }`。
pub fn sys_pselect6_time32(
    nfds: i32,
    readfds: *mut usize,
    writefds: *mut usize,
    exceptfds: *mut usize,
    tmo_p: *const OldTimespec32,
    sigmask: *const u8,
) -> isize {
    trace!(
        "kernel:pid[{}] sys_pselect6_time32",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        if nfds < 0 {
            return Err(ERRNO::EINVAL);
        }
        let nfds = nfds as usize;
        if nfds > MAX_POLL_NFDS {
            return Err(ERRNO::EINVAL);
        }

        let read_set = copy_user_fdset_words(token, readfds as *const usize, nfds)?;
        let write_set = copy_user_fdset_words(token, writefds as *const usize, nfds)?;
        let except_set = copy_user_fdset_words(token, exceptfds as *const usize, nfds)?;

        
        validate_pselect_fds(
            nfds,
            read_set.as_deref(),
            write_set.as_deref(),
            except_set.as_deref(),
        )?;
        
        let (mut pollfds, metas) = build_pselect_pollfds(
            nfds,
            read_set.as_deref(),
            write_set.as_deref(),
            except_set.as_deref(),
        )?;
        
        let timeout_ns = parse_timeout_ns(tmo_p)?;
        // debug!(
        //     "sys_pselect6_time32: nfds={}, read_set={:?}, write_set={:?}, except_set={:?}, timeout_ms={:?}, sigmask={:p}",
        //     nfds, read_set, write_set, except_set, timeout_ms, sigmask
        // );
        let deadline_ns = timeout_ns_to_deadline_ns(timeout_ns)?;
        let pid = current_task().unwrap().process.upgrade().unwrap().getpid();
        let (sigmask_ptr, sigsetsize) = parse_pselect_sigmask_arg(sigmask)?;
        let old_mask = apply_temp_signal_mask(sigmask_ptr, sigsetsize, "sys_pselect6_time32")?;

        let ret = poll_wait_loop_with_writeback(pid, &mut pollfds, deadline_ns, |polled| {
            write_back_pselect_fdsets(
                readfds,
                writefds,
                exceptfds,
                nfds,
                polled,
                &metas,
            )
        });
        restore_temp_signal_mask(old_mask);
        ret
    })
}

pub fn sys_renameat2(
    old_dirfd: isize,
    old_name: *const u8,
    new_dirfd: isize,
    new_name: *const u8,
    flags: u32,
) -> isize {
    trace!(
        "kernel:pid[{}] sys_renameat2(flags={})",
        current_task().unwrap().process.upgrade().unwrap().getpid(),
        flags
    );
    if flags != 0 {
        return -(ERRNO::EINVAL as isize);
    }
    let token = current_user_token();
    syscall_body!({
        let old_name = translated_str(token, old_name).or_errno(ERRNO::EFAULT)?;
        let new_name = translated_str(token, new_name).or_errno(ERRNO::EFAULT)?;
        if old_name.is_empty() || new_name.is_empty() {
            return Err(ERRNO::ENOENT);
        }
        let old_cwd = resolve_dirfd_base(old_dirfd, old_name.as_str())?;
        let new_cwd = resolve_dirfd_base(new_dirfd, new_name.as_str())?;
        rename_at(old_cwd.as_str(), &old_name, new_cwd.as_str(), &new_name)?;
        Ok(0)
    })
}

/// `statfs64(2)` syscall: get filesystem statistics by path.
pub fn sys_statfs64(path: *const u8, buf: *mut u8) -> isize {
    trace!("kernel:pid[{}] sys_statfs64",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    let token = current_user_token();
    syscall_body!({
        let path_str = translated_str(token, path).ok_or(ERRNO::EFAULT)?;
        let cwd = resolve_dirfd_base(AT_FDCWD, path_str.as_str())?;
        debug!("sys_statfs64: cwd = '{}', path = '{}'", cwd, path_str);
        let inode = lookup_inode_follow(cwd.as_str(), rooted_lookup_path(path_str.as_str()), true)?;
        let stat = inode.statfs()?;
        let buf_ptr = buf as *mut StatFs64;
        write_pod_to_user(buf_ptr, &stat)?;
        Ok(0)
    })
}

/// `fstatfs64(2)` syscall: get filesystem statistics by file descriptor.
pub fn sys_fstatfs64(fd: u32, buf: *mut u8) -> isize {
    trace!("kernel:pid[{}] sys_fstatfs64",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let fd = fd as usize;
        let process = current_process();
        let inner = process.inner_exclusive_access();
        let file = inner
            .fd_table
            .get(fd)
            .and_then(|f| f.as_ref())
            .ok_or(ERRNO::EBADF)?;
        let stat = file.desc.statfs()?;
        drop(inner);
        let buf_ptr = buf as *mut StatFs64;
        write_pod_to_user(buf_ptr, &stat)?;
        Ok(0)
    })
}

pub fn sys_fallocate(fd: u32, mode: i32, offset: i64, len: i64) -> isize {
    syscall_body!({
        if mode != 0 {
            // Linux fallocate 仅支持 mode=0（标准空间预分配）
            return Err(ERRNO::EOPNOTSUPP);
        }
        if offset < 0 || len <= 0 {
            return Err(ERRNO::EINVAL);
        }

        let offset = offset as usize;
        let len = len as usize;
        let new_size = offset.checked_add(len).ok_or(ERRNO::EINVAL)?;
        let desc = get_writable_file(fd as usize)?;
        let stat = desc.stat();
        let path = desc.path();
        debug!(
            "sys_fallocate: fd={} mode={:#x} offset={} len={} old_size={} new_size={} st_blksize={} path={:?}",
            fd,
            mode,
            offset,
            len,
            stat.size,
            new_size,
            stat.blksize,
            path
        );
        if new_size >= SUSPICIOUS_RW_LEN || stat.blksize >= SUSPICIOUS_STAT_BLKSIZE {
            warn!(
                "sys_fallocate: suspicious request fd={} mode={:#x} offset={} len={} old_size={} new_size={} st_blksize={} path={:?}",
                fd,
                mode,
                offset,
                len,
                stat.size,
                new_size,
                stat.blksize,
                path
            );
        }
        desc.fallocate(mode, offset, len)?;
        Ok(0)
    })
}
