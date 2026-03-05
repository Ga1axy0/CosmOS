use super::File;
use crate::drivers::BLOCK_DEVICE;
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::*;
use fs::Inode;
use fs::EasyFileSystem;
use fs::Fat32FileSystem;
use lazy_static::*;

/// inode in memory
pub struct OSInode {
    readable: bool,
    writable: bool,
    inner: UPSafeCell<OSInodeInner>,
}
/// inner of inode in memory
pub struct OSInodeInner {
    offset: usize,
    inode: Arc<Inode>,
}

impl OSInode {
    /// create a new inode in memory
    pub fn new(readable: bool, writable: bool, inode: Arc<Inode>) -> Self {
        trace!("kernel: OSInode::new");
        Self {
            readable,
            writable,
            inner: unsafe { UPSafeCell::new(OSInodeInner { offset: 0, inode }) },
        }
    }
    /// read all data from the inode in memory
    pub fn read_all(&self) -> Vec<u8> {
        trace!("kernel: OSInode::read_all");
        let mut inner = self.inner.exclusive_access();
        let mut buffer: Vec<u8> = Vec::with_capacity(512);
        buffer.resize(512, 0);
        let mut v: Vec<u8> = Vec::new();
        loop {
            let len = inner.inode.read_at(inner.offset, &mut buffer);
            if len == 0 {
                break;
            }
            inner.offset += len;
            v.extend_from_slice(&buffer[..len]);
        }
        v
    }
}

lazy_static! {
    pub static ref ROOT_INODE: Arc<Inode> = {
        #[cfg(feature = "fat32")]
        {
            let efs = Fat32FileSystem::open(BLOCK_DEVICE.clone());
            Arc::new(Fat32FileSystem::root_inode(&efs))
        }
        #[cfg(feature = "easyfs")]
        {
            let efs = EasyFileSystem::open(BLOCK_DEVICE.clone());
            Arc::new(EasyFileSystem::root_inode(&efs))
        }
        #[cfg(not(any(feature = "fat32", feature = "easyfs")))]
        {
            compile_error!("You must enable either 'fat32' or 'easyfs' feature!");
        }
    };
}

/// List all apps in the root directory
pub fn list_apps() {
    println!("/**** APPS ****");
    for app in ROOT_INODE.ls() {
        println!("{}", app);
    }
    println!("**************/");
}

/// Resolve `path` against `cwd` into an absolute canonical path string.
///
/// - If `path` starts with `/` it is used as-is (after component normalisation).
/// - Otherwise it is concatenated after `cwd`.
/// - `.` and `..` components are collapsed.
pub fn canonicalize(cwd: &str, path: &str) -> String {
    let base = if path.starts_with('/') {
        String::from(path)
    } else {
        let mut s = String::from(cwd);
        s.push('/');
        s.push_str(path);
        s
    };

    let mut stack: Vec<&str> = Vec::new();
    for component in base.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            c => stack.push(c),
        }
    }

    // debug!("stack={:?}", stack);

    if stack.is_empty() {
        String::from("/")
    } else {
        let mut result = String::new();
        for c in &stack {
            result.push('/');
            result.push_str(c);
        }
        result
    }
}

/// Walk the virtual filesystem from the root to the node at `abs_path`.
/// Returns `None` if any component along the path is not found.
pub fn lookup_inode(abs_path: &str) -> Option<Arc<Inode>> {
    let components: Vec<&str> = abs_path.split('/').filter(|s| !s.is_empty()).collect();
    if components.is_empty() {
        return Some(Arc::clone(&ROOT_INODE));
    }
    let mut cur: Arc<Inode> = Arc::clone(&ROOT_INODE);
    for component in components {
        cur = cur.find(component)?;
    }
    Some(cur)
}

/// Resolve `path` into (parent_directory_inode, filename).
/// Returns `None` if the parent directory does not exist.
fn resolve_parent(cwd: &str, path: &str) -> Option<(Arc<Inode>, String)> {
    let abs = canonicalize(cwd, path);
    if abs == "/" {
        return None; // cannot resolve parent of root
    }
    // Split into directory part and filename.
    let (parent_path, filename) = match abs.rfind('/') {
        Some(idx) if idx == 0 => ("/", &abs[1..]),
        Some(idx) => (&abs[..idx], &abs[idx + 1..]),
        None => ("/", abs.as_str()),
    };
    let parent = lookup_inode(parent_path)?;
    Some((parent, String::from(filename)))
}

/// Open (or optionally create) a file/directory at `path` relative to `cwd`.
pub fn open_file_at(cwd: &str, path: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    trace!("kernel: open_file_at: cwd={}, path={}, flags={:?}", cwd, path, flags);
    let abs = canonicalize(cwd, path);
    debug!("open_file_at: path = {} -> abs path = {}", path, abs);
    let (readable, writable) = flags.read_write();

    if flags.contains(OpenFlags::CREATE) {
        // Navigate to the parent directory and create the file there.
        let (parent, name) = resolve_parent(cwd, path)?;
        if let Some(existing) = parent.find(&name) {
            // File already exists: truncate if asked, then return it.
            existing.clear();
            Some(Arc::new(OSInode::new(readable, writable, existing)))
        } else {
            parent
                .create(&name)
                .map(|inode| Arc::new(OSInode::new(readable, writable, inode)))
        }
    } else {
        lookup_inode(&abs).map(|inode| {
            if flags.contains(OpenFlags::TRUNC) {
                inode.clear();
            }
            Arc::new(OSInode::new(readable, writable, inode))
        })
    }
}

/// Create a directory at `path` relative to `cwd`.
/// Returns `true` on success.
pub fn mkdir_at(cwd: &str, path: &str) -> bool {
    if let Some((parent, name)) = resolve_parent(cwd, path) {
        parent.mkdir(&name).is_some()
    } else {
        false
    }
}

bitflags! {
    ///  The flags argument to the open() system call is constructed by ORing together zero or more of the following values:
    pub struct OpenFlags: u32 {
        /// readyonly
        const RDONLY = 0;
        /// writeonly
        const WRONLY = 1 << 0;
        /// read and write
        const RDWR = 1 << 1;
        /// create new file
        const CREATE = 1 << 9;
        /// truncate file size to 0
        const TRUNC = 1 << 10;
    }
}

impl OpenFlags {
    /// Do not check validity for simplicity
    /// Return (readable, writable)
    pub fn read_write(&self) -> (bool, bool) {
        if self.is_empty() {
            (true, false)
        } else if self.contains(Self::WRONLY) {
            (false, true)
        } else {
            (true, true)
        }
    }
}

/// Open a file
pub fn open_file(name: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    trace!("kernel: open_file: name = {}, flags = {:?}", name, flags);
    let (readable, writable) = flags.read_write();
    if flags.contains(OpenFlags::CREATE) {
        if let Some(inode) = ROOT_INODE.find(name) {
            // clear size
            inode.clear();
            Some(Arc::new(OSInode::new(readable, writable, inode)))
        } else {
            // create file
            ROOT_INODE
                .create(name)
                .map(|inode| Arc::new(OSInode::new(readable, writable, inode)))
        }
    } else {
        ROOT_INODE.find(name).map(|inode| {
            if flags.contains(OpenFlags::TRUNC) {
                inode.clear();
            }
            Arc::new(OSInode::new(readable, writable, inode))
        })
    }
}

impl File for OSInode {
    /// file readable?
    fn readable(&self) -> bool {
        self.readable
    }
    /// file writable?
    fn writable(&self) -> bool {
        self.writable
    }
    fn is_dir(&self) -> bool {
        self.inner.exclusive_access().inode.is_dir()
    }
    /// read file data into buffer
    fn read(&self, mut buf: UserBuffer) -> usize {
        trace!("kernel: OSInode::read");
        let mut inner = self.inner.exclusive_access();
        let mut total_read_size = 0usize;
        for slice in buf.buffers.iter_mut() {
            let read_size = inner.inode.read_at(inner.offset, *slice);
            if read_size == 0 {
                break;
            }
            inner.offset += read_size;
            total_read_size += read_size;
        }
        total_read_size
    }
    /// write buffer data into file
    fn write(&self, buf: UserBuffer) -> usize {
        trace!("kernel: OSInode::write");
        let mut inner = self.inner.exclusive_access();
        let mut total_write_size = 0usize;
        for slice in buf.buffers.iter() {
            let write_size = inner.inode.write_at(inner.offset, *slice);
            assert_eq!(write_size, slice.len());
            inner.offset += write_size;
            total_write_size += write_size;
        }
        total_write_size
    }
    /// Fill `buf` with `linux_dirent64` records from the directory.
    ///
    /// `inner.offset` is used as an **entry index** (not a byte offset) so that
    /// successive calls pick up where the previous call left off.
    ///
    /// Each record layout (`linux_dirent64`):
    /// ```text
    ///   +0   d_ino    u64  (entry index)
    ///   +8   d_off    i64  (index of next entry, −1 for last)
    ///   +16  d_reclen u16  (total length of this record, aligned to 8 B)
    ///   +18  d_type   u8   (DT_DIR=4, DT_REG=8, DT_UNKNOWN=0)
    ///   +19  d_name[] null-terminated name, padded to make reclen a multiple of 8
    /// ```
    fn getdents64(&self, buf: &mut [u8]) -> usize {
        let mut inner = self.inner.exclusive_access();
        if !inner.inode.is_dir() {
            return 0;
        }
        let inode = Arc::clone(&inner.inode);
        let entries = inode.ls();
        let start_idx = inner.offset; // entry index, not byte offset
        let mut written = 0usize;
        let mut new_idx = start_idx;

        for (i, name) in entries.iter().enumerate().skip(start_idx) {
            let name_bytes = name.as_bytes();
            // reclen must be a multiple of 8
            let reclen = (19 + name_bytes.len() + 1 + 7) & !7usize;
            if written + reclen > buf.len() {
                break;
            }
            // d_ino (u64)
            buf[written..written + 8].copy_from_slice(&(i as u64).to_le_bytes());
            // d_off (i64): offset of *next* entry
            let next_off = (i + 1) as i64;
            buf[written + 8..written + 16].copy_from_slice(&next_off.to_le_bytes());
            // d_reclen (u16)
            buf[written + 16..written + 18].copy_from_slice(&(reclen as u16).to_le_bytes());
            // d_type (u8): check with find() to determine DIR or regular file
            let dtype: u8 = if let Some(child) = inode.find(name) {
                if child.is_dir() { 4 } else { 8 }
            } else {
                0
            };
            buf[written + 18] = dtype;
            // d_name: null-terminated, zero-padded to reclen
            buf[written + 19..written + 19 + name_bytes.len()].copy_from_slice(name_bytes);
            buf[written + 19 + name_bytes.len()] = 0;
            for b in &mut buf[written + 19 + name_bytes.len() + 1..written + reclen] {
                *b = 0;
            }
            written += reclen;
            new_idx = i + 1;
        }
        inner.offset = new_idx;
        written
    }
}
