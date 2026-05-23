//! Minimal procfs implementation for `/proc`.
//!
//! Provides:
//! - `/proc/meminfo` — basic memory statistics.
//! - `/proc/mounts`  — current mount table.
//! - `/proc/self`    — symlink to current process directory.
//! - `/proc/<pid>/exe` — symlink to process executable path.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::Write;

use fs::errno::FS_ERRNO;
use fs::vfs::{VfsFileType, VfsNode};

use crate::config::PAGE_SIZE;
use crate::fs::inode::snapshot_mount_table;
use crate::fs::PAGE_CACHE_MANAGER;
use crate::mm::frame_allocator_stats;
use crate::sched::{list_pids, pid2process};
use crate::task::current_process;

fn parse_pid(name: &str) -> Option<usize> {
    if name.is_empty() || !name.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

fn build_meminfo() -> String {
    let stats = frame_allocator_stats();
    let cached_pages = PAGE_CACHE_MANAGER.lock().cached_pages;
    let page_kb = (PAGE_SIZE as u64) / 1024;
    let mem_total = stats.total_pages as u64 * page_kb;
    let mem_free = stats.free_pages as u64 * page_kb;
    let cached = cached_pages as u64 * page_kb;
    let mem_available = mem_free.saturating_add(cached);

    let mut out = String::new();
    let _ = writeln!(&mut out, "MemTotal:       {} kB", mem_total);
    let _ = writeln!(&mut out, "MemFree:        {} kB", mem_free);
    let _ = writeln!(&mut out, "MemAvailable:   {} kB", mem_available);
    let _ = writeln!(&mut out, "Cached:         {} kB", cached);
    out
}

fn escape_mount_field(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        match ch {
            ' ' => out.push_str("\\040"),
            '\t' => out.push_str("\\011"),
            '\n' => out.push_str("\\012"),
            '\\' => out.push_str("\\134"),
            _ => out.push(ch),
        }
    }
    out
}

fn build_mounts() -> String {
    let mut out = String::new();
    for mount in snapshot_mount_table() {
        let _ = writeln!(
            &mut out,
            "{} {} {} {} 0 0",
            escape_mount_field(&mount.source),
            escape_mount_field(&mount.target),
            escape_mount_field(&mount.fs_type),
            escape_mount_field(&mount.options),
        );
    }
    out
}

/// `/proc` root directory node.
#[derive(Default)]
pub struct ProcRootNode;

impl ProcRootNode {
    /// Create a new procfs root node.
    pub fn new() -> Self {
        Self
    }
}

impl VfsNode for ProcRootNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Directory
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        let mut entries = Vec::new();
        entries.push((String::from("self"), VfsFileType::Symlink));
        entries.push((String::from("meminfo"), VfsFileType::Regular));
        entries.push((String::from("mounts"), VfsFileType::Regular));
        for pid in list_pids() {
            entries.push((alloc::format!("{}", pid), VfsFileType::Directory));
        }
        entries
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        match name {
            "self" => Some(Arc::new(ProcSelfLinkNode::new()) as Arc<dyn VfsNode>),
            "meminfo" => Some(Arc::new(ProcMeminfoNode::new()) as Arc<dyn VfsNode>),
            "mounts" => Some(Arc::new(ProcMountsNode::new()) as Arc<dyn VfsNode>),
            _ => {
                let pid = parse_pid(name)?;
                if pid2process(pid).is_some() {
                    Some(Arc::new(ProcPidDirNode::new(pid)) as Arc<dyn VfsNode>)
                } else {
                    None
                }
            }
        }
    }

    fn create(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn mkdir(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn clear(&self) {}

    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        0
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/meminfo` node.
#[derive(Default)]
pub struct ProcMeminfoNode;

impl ProcMeminfoNode {
    /// Create a new meminfo node.
    pub fn new() -> Self {
        Self
    }
}

impl VfsNode for ProcMeminfoNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        build_meminfo().len()
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        let data = build_meminfo();
        let bytes = data.as_bytes();
        if offset >= bytes.len() {
            return 0;
        }
        let end = (offset + buf.len()).min(bytes.len());
        let len = end - offset;
        buf[..len].copy_from_slice(&bytes[offset..end]);
        len
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/mounts` node.
#[derive(Default)]
pub struct ProcMountsNode;

impl ProcMountsNode {
    /// Create a new mounts node.
    pub fn new() -> Self {
        Self
    }
}

impl VfsNode for ProcMountsNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        build_mounts().len()
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        let data = build_mounts();
        let bytes = data.as_bytes();
        if offset >= bytes.len() {
            return 0;
        }
        let end = (offset + buf.len()).min(bytes.len());
        let len = end - offset;
        buf[..len].copy_from_slice(&bytes[offset..end]);
        len
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/self` symlink node.
#[derive(Default)]
pub struct ProcSelfLinkNode;

impl ProcSelfLinkNode {
    /// Create a new self symlink node.
    pub fn new() -> Self {
        Self
    }

    fn link_target(&self) -> String {
        let pid = current_process().getpid();
        alloc::format!("/proc/{}", pid)
    }
}

impl VfsNode for ProcSelfLinkNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Symlink
    }

    fn size(&self) -> usize {
        self.link_target().len()
    }

    fn read_link(&self) -> Result<String, FS_ERRNO> {
        Ok(self.link_target())
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
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

    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        0
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/<pid>` directory node.
pub struct ProcPidDirNode {
    pid: usize,
}

impl ProcPidDirNode {
    /// Create a new `/proc/<pid>` node.
    pub fn new(pid: usize) -> Self {
        Self { pid }
    }
}

impl VfsNode for ProcPidDirNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Directory
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        if pid2process(self.pid).is_none() {
            return Vec::new();
        }
        alloc::vec![(String::from("exe"), VfsFileType::Symlink)]
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        if name != "exe" {
            return None;
        }
        if pid2process(self.pid).is_some() {
            Some(Arc::new(ProcPidExeLinkNode::new(self.pid)) as Arc<dyn VfsNode>)
        } else {
            None
        }
    }

    fn create(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn mkdir(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn clear(&self) {}

    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        0
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/<pid>/exe` symlink node.
pub struct ProcPidExeLinkNode {
    pid: usize,
}

impl ProcPidExeLinkNode {
    /// Create a new `/proc/<pid>/exe` symlink node.
    pub fn new(pid: usize) -> Self {
        Self { pid }
    }

    fn link_target(&self) -> Result<String, FS_ERRNO> {
        let process = pid2process(self.pid).ok_or(FS_ERRNO::ENOENT)?;
        let path = process.exec_path();
        if path.is_empty() {
            return Err(FS_ERRNO::ENOENT);
        }
        Ok(path)
    }
}

impl VfsNode for ProcPidExeLinkNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Symlink
    }

    fn size(&self) -> usize {
        self.link_target().map(|path| path.len()).unwrap_or(0)
    }

    fn read_link(&self) -> Result<String, FS_ERRNO> {
        self.link_target()
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
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

    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        0
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}
