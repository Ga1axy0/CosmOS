use super::{File, Stat, StatMode};
use super::rootfs::{VirtualDirNode, VIRT_ROOT};
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use crate::syscall::errno::ERRNO;
use crate::fs::devfs::BlockDevNode;
use crate::drivers::block::BLOCK_DEVICES;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::*;
use fs::vfs::VfsNode;
use fs::Inode;
use lazy_static::*;

// Compile-time check: exactly one filesystem backend must be selected.
#[cfg(not(any(feature = "ext4", feature = "easyfs", feature = "fat32")))]
compile_error!("Enable one of the cargo features: ext4 | easyfs | fat32");

/// inode in memory
pub struct OSInode {
    readable: bool,
    writable: bool,
    path: String,
    inner: UPSafeCell<OSInodeInner>,
}
/// inner of inode in memory
pub struct OSInodeInner {
    offset: usize,
    inode: Arc<Inode>,
}

impl OSInode {
    /// create a new inode in memory
    pub fn new(readable: bool, writable: bool, inode: Arc<Inode>, path: String) -> Self {
        trace!("kernel: OSInode::new");
        Self {
            readable,
            writable,
            path,
            inner: unsafe { UPSafeCell::new(OSInodeInner { offset: 0, inode }) },
        }
    }
    /// read all data from the inode in memory
    pub fn read_all(&self) -> Vec<u8> {
        trace!("kernel: OSInode::read_all");
        let mut inner = self.inner.exclusive_access();
        let mut buffer: Vec<u8> = alloc::vec![0; 512];
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

/// Special dirfd value meaning “use the caller's current working directory”.
pub const AT_FDCWD: isize = -100;
/// `unlinkat` flag for removing an empty directory instead of a non-directory.
pub const AT_REMOVEDIR: u32 = 0x200;

lazy_static! {
    /// Tracks virtual directories created by `do_mount` for sub-path mounts.
    ///
    /// Maps absolute path → `Arc<VirtualDirNode>` for every virtual directory
    /// inserted into the namespace during mount operations.  Used by
    /// `ensure_virtual_dir` (to avoid recreating existing dirs) and
    /// `do_umount` (to clean up the registry).
    static ref VIRT_DIRS: UPSafeCell<BTreeMap<String, Arc<VirtualDirNode>>> =
        // SAFETY: single-processor kernel.
        unsafe { UPSafeCell::new(BTreeMap::new()) };

    /// The kernel's global root inode, backed by the virtual rootfs.
    ///
    /// Call [`init_rootfs`] once after `mm::init()` to overlay a real
    /// filesystem and make the full directory tree accessible.
    pub static ref ROOT_INODE: Arc<Inode> =
        Arc::new(Inode::new(Arc::clone(&VIRT_ROOT) as Arc<dyn VfsNode>));
}

// ---------------------------------------------------------------------------
// Mount / unmount (kernel-internal API)
// ---------------------------------------------------------------------------

/// Split an absolute path into `(parent_path, leaf_name)`.
///
/// Examples:
/// - `"/mnt/fat32"` → `("/mnt", "fat32")`
/// - `"/mnt"` → `("/", "mnt")`
fn split_for_mount(abs_path: &str) -> (&str, &str) {
    match abs_path.rfind('/') {
        Some(0) => ("/", &abs_path[1..]),
        Some(idx) => (&abs_path[..idx], &abs_path[idx + 1..]),
        None => ("/", abs_path),
    }
}

/// Ensure a virtual directory exists at `abs_path`, creating intermediate
/// virtual directories as needed.
///
/// If the current overlay FS already has a physical directory at any
/// component of `abs_path`, the corresponding virtual dir will inherit that
/// physical dir as its own overlay so that files inside it remain accessible.
fn ensure_virtual_dir(abs_path: &str) -> Result<Arc<VirtualDirNode>, ERRNO> {
    if abs_path == "/" {
        return Ok(Arc::clone(&VIRT_ROOT));
    }

    // Fast path: already created.
    {
        let map = VIRT_DIRS.exclusive_access();
        if let Some(vdir) = map.get(abs_path) {
            return Ok(Arc::clone(vdir));
        }
    }

    // Create by ensuring the parent first (recursive, bounded by path depth).
    let (parent_path, name) = split_for_mount(abs_path);
    let parent_vdir = ensure_virtual_dir(parent_path)?;

    // If the backing FS has a directory at this name, use it as the overlay of
    // the new virtual dir so its contents remain visible.
    let child_overlay: Option<Arc<dyn VfsNode>> = parent_vdir.overlay_child_dir(name);

    let new_vdir = VirtualDirNode::new();
    if let Some(ov) = child_overlay {
        new_vdir.set_overlay(ov);
    }

    // Insert into the virtual namespace.
    parent_vdir.bind(name, Arc::clone(&new_vdir) as Arc<dyn VfsNode>);

    VIRT_DIRS
        .exclusive_access()
        .insert(String::from(abs_path), Arc::clone(&new_vdir));

    Ok(new_vdir)
}

/// Mount `fs_root` at the absolute path `path`.
///
/// - `path = "/"`: installs `fs_root` as the *overlay* of the virtual root
///   directory.  All on-disk paths become visible without any other changes.
/// - `path = "/mnt/foo"`: creates virtual intermediate directories as needed
///   and binds the FS root as a named child, making it accessible at that
///   path while leaving other parts of the namespace unaffected.
///
/// This function is intentionally synchronous and infallible for well-formed
/// inputs so it can be used during early boot before any processes exist.
/// Future `sys_mount` / `sys_umount2` syscalls should wrap it.
pub fn do_mount(path: &str, fs_root: Arc<Inode>) -> Result<(), ERRNO> {
    let abs = canonicalize("/", path);
    let vfs_node: Arc<dyn VfsNode> = fs_root.vfs_node();

    if abs == "/" {
        // Install as the overlay of the virtual root directory.
        VIRT_ROOT.set_overlay(vfs_node);
        info!("[kernel] mounted fs at /");
        return Ok(());
    }

    // For sub-paths: ensure parent virtual dir exists, then bind at leaf name.
    let (parent_path, name) = split_for_mount(&abs);
    let parent_vdir = ensure_virtual_dir(parent_path)?;
    parent_vdir.bind(name, vfs_node);
    info!("[kernel] mounted fs at {}", abs);
    Ok(())
}

/// Unmount the filesystem mounted at `path`.
///
/// For a mount point that was itself a [`VirtualDirNode`] (i.e. an
/// intermediate directory created by [`do_mount`]), it is also removed from
/// the internal registry.  Sub-mounts must be unmounted first; this function
/// does **not** cascade.
pub fn do_umount(path: &str) -> Result<(), ERRNO> {
    let abs = canonicalize("/", path);
    if abs == "/" {
        // Unmounting the root overlay is not supported (use pivot_root instead).
        return Err(ERRNO::EBUSY);
    }

    let (parent_path, name) = split_for_mount(&abs);

    let parent_vdir: Arc<VirtualDirNode> = if parent_path == "/" {
        Arc::clone(&VIRT_ROOT)
    } else {
        VIRT_DIRS
            .exclusive_access()
            .get(parent_path)
            .cloned()
            .ok_or(ERRNO::EINVAL)?
    };

    if !parent_vdir.unbind(name) {
        return Err(ERRNO::EINVAL);
    }

    // Clean up the registry entry (no-op if `abs` was a real-FS mount, not
    // a VirtualDirNode we created).
    VIRT_DIRS.exclusive_access().remove(&abs);

    info!("[kernel] unmounted {}", abs);
    Ok(())
}

/// Mount the compiled-in filesystem at `"/"` and log the result.
///
/// Must be called **after** `mm::init()` (heap allocator required for `Arc`
/// and filesystem initialisation) and before any file-system operations.
/// Invoked from `rust_main` in `main.rs`.
pub fn init_rootfs() {
    use crate::drivers::BLOCK_DEVICE;

    #[cfg(feature = "fat32")]
    {
        use fs::Fat32FileSystem;
        let vfs = Fat32FileSystem::open(BLOCK_DEVICE.clone());
        let root = Arc::new(Fat32FileSystem::root_inode(&vfs));
        do_mount("/", root).unwrap_or_else(|_| panic!("[kernel] failed to mount fat32 at /"));
    }
    #[cfg(feature = "easyfs")]
    {
        use fs::EasyFileSystem;
        let efs = EasyFileSystem::open(BLOCK_DEVICE.clone());
        let root = Arc::new(EasyFileSystem::root_inode(&efs));
        do_mount("/", root).unwrap_or_else(|_| panic!("[kernel] failed to mount easyfs at /"));
    }
    #[cfg(feature = "ext4")]
    {
        use fs::Ext4FileSystem;
        let efs = Ext4FileSystem::open(BLOCK_DEVICE.clone());
        let root = Arc::new(Ext4FileSystem::root_inode(&efs));
        do_mount("/", root.clone()).unwrap_or_else(|_| panic!("[kernel] failed to mount ext4 at /"));
        // do_mount("/mnt/vda", root).unwrap_or_else(|_| panic!("[kernel] failed to mount ext4 at /mnt/sda"));
    }

    info!("[kernel] rootfs initialised");
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
        Some(0) => ("/", &abs[1..]),
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
            Some(Arc::new(OSInode::new(
                readable,
                writable,
                existing,
                abs.clone(),
            )))
        } else {
            parent
                .create(&name)
                .map(|inode| Arc::new(OSInode::new(readable, writable, inode, abs.clone())))
        }
    } else {
        lookup_inode(&abs).map(|inode| {
            if flags.contains(OpenFlags::TRUNC) {
                debug!("open_file_at: truncating existing file at {}", abs);
                inode.clear();
            }
            Arc::new(OSInode::new(readable, writable, inode, abs.clone()))
        })
    }
}

/// Create a directory at `path` relative to `cwd`.
/// Returns `true` on success.
pub fn mkdir_at(cwd: &str, path: &str) -> Result<(), ERRNO> {
    if let Some((parent, name)) = resolve_parent(cwd, path) {
        // 已存在同名目录或文件
        if parent.find(&name).is_some() {
            return Err(ERRNO::EEXIST);
        }
        // 父节点不是目录
        if !parent.is_dir() {
            return Err(ERRNO::ENOTDIR);
        }
        // 创建失败
        if parent.mkdir(&name).is_none() {
            return Err(ERRNO::EIO);
        }
        Ok(())
    } else {
        Err(ERRNO::ENOENT)
    }
}

bitflags! {
    ///  The flags argument to the open() system call is constructed by ORing together zero or more of the following values:
    pub struct OpenFlags: i32 {
        /// readyonly
        /// TODO: fix the bug of bitflag.
        const RDONLY = 0x000;
        /// writeonly
        const WRONLY = 0x001;
        /// read and write
        const RDWR = 0x002;
        /// create new file
        const CREATE = 0x40;
        /// truncate file size to 0
        const TRUNC = 0x200;
        /// open directory
        const DIRECTORY = 0x10000;
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
    let abs = canonicalize("/", name);
    if flags.contains(OpenFlags::CREATE) {
        if let Some(inode) = ROOT_INODE.find(name) {
            // clear size
            inode.clear();
            Some(Arc::new(OSInode::new(readable, writable, inode, abs)))
        } else {
            // create file
            ROOT_INODE
                .create(name)
                .map(|inode| Arc::new(OSInode::new(readable, writable, inode, canonicalize("/", name))))
        }
    } else {
        ROOT_INODE.find(name).map(|inode| {
            if flags.contains(OpenFlags::TRUNC) {
                inode.clear();
            }
            Arc::new(OSInode::new(readable, writable, inode, abs))
        })
    }
}

/// Create a hard link from `old_path` to `new_path`.
pub fn linkat(old_cwd: &str, old_path: &str, new_cwd: &str, new_path: &str) -> Result<(), ERRNO> {
    let (_, old_name) = resolve_parent(old_cwd, old_path).ok_or(ERRNO::ENOENT)?;
    let (new_parent, new_name) = resolve_parent(new_cwd, new_path).ok_or(ERRNO::ENOENT)?;
    if old_name.is_empty() || new_name.is_empty() {
        return Err(ERRNO::ENOENT);
    }
    let (old_parent, old_name) = resolve_parent(old_cwd, old_path).ok_or(ERRNO::ENOENT)?;
    let old_inode = old_parent.find(old_name.as_str()).ok_or(ERRNO::ENOENT)?;
    if old_inode.is_dir() {
        return Err(ERRNO::EPERM);
    }
    if new_parent.find(new_name.as_str()).is_some() {
        return Err(ERRNO::EEXIST);
    }
    new_parent
        .link_inode(&old_inode, new_name.as_str())?;
    Ok(())
}

/// Remove a link at `path` relative to `cwd`.
pub fn unlinkat(cwd: &str, path: &str, flags: u32) -> Result<(), ERRNO> {
    if flags & !AT_REMOVEDIR != 0 {
        return Err(ERRNO::EINVAL);
    }
    let (parent, name) = resolve_parent(cwd, path).ok_or(ERRNO::ENOENT)?;
    if name.is_empty() {
        return Err(ERRNO::ENOENT);
    }
    let inode = parent.find(name.as_str()).ok_or(ERRNO::ENOENT)?;
    if inode.is_dir() {
        if flags & AT_REMOVEDIR == 0 {
            return Err(ERRNO::EISDIR);
        }
        if !inode.ls().is_empty() {
            return Err(ERRNO::ENOTEMPTY);
        }
        parent.rmdir(name.as_str())?
    } else {
        if flags & AT_REMOVEDIR != 0 {
            return Err(ERRNO::ENOTDIR);
        }
        parent.unlink(name.as_str())?
    }
    Ok(())
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
            debug!("OSInode::read: offset={}, slice_len={}", inner.offset, slice.len());
            let read_size = inner.inode.read_at(inner.offset, *slice);
            if read_size == 0 {
                break;
            }
            inner.offset += read_size;
            total_read_size += read_size;
        }
        total_read_size
    }
    fn read_at(&self, offset: usize, mut buf: UserBuffer) -> usize {
        let inner = self.inner.exclusive_access();
        let mut file_off = offset;
        let mut total_read_size = 0usize;
        for slice in buf.buffers.iter_mut() {
            let read_size = inner.inode.read_at(file_off, *slice);
            if read_size == 0 {
                break;
            }
            file_off += read_size;
            total_read_size += read_size;
            if read_size < slice.len() {
                break;
            }
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

    fn stat(&self) -> Stat {
        let inner = self.inner.exclusive_access();
        let mode = if inner.inode.is_dir() {
            StatMode::DIR
        } else {
            StatMode::FILE
        };
        Stat {
            dev: 0,
            ino: inner.inode.ino(),
            mode,
            nlink: inner.inode.nlink(),
            uid: 0,
            gid: 0,
            rdev: 0,
            pad0: 0,
            size: inner.inode.size() as i64,
            blksize: 512,
            pad1: 0,
            blocks: (inner.inode.size() as u64 + 511) / 512,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
            unused: [0; 2],
        }
    }

    fn path(&self) -> Option<String> {
        Some(self.path.clone())
    }
}

// ---------------------------------------------------------------------------
// Device-filesystem helpers
// ---------------------------------------------------------------------------

/// Populate `/dev` with one [`BlockDevNode`] per discovered block device.
///
/// Must be called **after** both [`probe_block_devices`](crate::drivers::block::probe_block_devices)
/// and [`init_rootfs`].  The `/dev` virtual directory is created if absent.
pub fn init_dev() {


    let dev_dir = ensure_virtual_dir("/dev")
        .unwrap_or_else(|_| panic!("[kernel] failed to create /dev"));

    let map = BLOCK_DEVICES.exclusive_access();
    for (dev_name, dev) in map.iter() {
        let node = Arc::new(BlockDevNode::new(Arc::clone(dev)));
        dev_dir.bind(dev_name, node as Arc<dyn VfsNode>);
        info!("[kernel] /dev/{} registered", dev_name);
    }
    info!("[kernel] /dev initialized");
}

/// Mount the filesystem on `dev_path` at the absolute path `abs_mnt`.
///
/// `dev_path` must resolve to a [`BlockDevNode`] in the VFS (e.g. `/dev/vda`).
/// `abs_mnt` must be an already-canonicalized absolute pathname.
/// `fs_type` is a filesystem type string: `"vfat"`, `"fat32"`, or `"ext4"`.
pub fn mount_device(dev_path: &str, abs_mnt: &str, fs_type: &str) -> Result<(), ERRNO> {
    debug!(
        "mount_device: dev_path={}, abs_mnt={}, fs_type={}",
        dev_path,
        abs_mnt,
        fs_type,
    );
    let dev_inode = lookup_inode(dev_path).ok_or(ERRNO::ENODEV)?;
    let vfs_node = dev_inode.vfs_node();
    let block_dev_node = vfs_node
        .as_any()
        .downcast_ref::<BlockDevNode>()
        .ok_or(ERRNO::ENOTBLK)?;
    let block_dev = Arc::clone(&block_dev_node.device);

    let fs_root: Arc<Inode> = match fs_type {
        "vfat" | "fat32" => {
            use fs::Fat32FileSystem;
            debug!("mount_device: opening FAT32 filesystem on {}", dev_path);
            let vfs = Fat32FileSystem::open(block_dev);
            Arc::new(Fat32FileSystem::root_inode(&vfs))
        }
        #[cfg(feature = "ext4")]
        "ext4" => {
            use fs::Ext4FileSystem;
            let vfs = Ext4FileSystem::open(block_dev);
            Arc::new(Ext4FileSystem::root_inode(&vfs))
        }
        _ => return Err(ERRNO::EINVAL),
    };

    do_mount(abs_mnt, fs_root)
}
