use super::{discard_inode, page_cache, File, Stat, StatFs64, StatMode};
use super::devfs::{CpuDmaLatencyNode, NullDevNode, UrandomDevNode};
use super::rootfs::{VirtualDirNode, VIRT_ROOT};
use super::tmpfs::new_tmpfs_root;
use super::cgroupfs::{new_cgroup2_root, CgroupDirNode};
use crate::mm::UserBuffer;
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::timer::get_realtime_ns;
use crate::fs::devfs::{
    ensure_ltp_scratch_device, BlockDevNode, DevRootNode, RtcDevNode, ZeroDevNode,
};
use crate::fs::tty::TtyDeviceNode;
use crate::fs::procfs::ProcRootNode;
use crate::fs::sysfs::SysRootNode;
use crate::drivers::block::{block_device_name, BLOCK_DEVICES};
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::*;
use fs::vfs::{VfsFileType, VfsNode};
use fs::Inode;
use lazy_static::*;
use core::any::Any;

// Compile-time check: exactly one filesystem backend must be selected.
#[cfg(not(any(feature = "ext4", feature = "easyfs", feature = "fat32")))]
compile_error!("Enable one of the cargo features: ext4 | easyfs | fat32");

/// inode in memory
pub struct OSInode {
    path: String,
    inode: Arc<Inode>,
}

impl OSInode {
    /// create a new inode in memory
    pub fn new(inode: Arc<Inode>, path: String) -> Self {
        trace!("kernel: OSInode::new");
        Self { path, inode }
    }

    /// Add `pid` to this inode when it is a cgroup v2 directory.
    pub fn add_pid_to_cgroup(&self, pid: usize) -> Result<(), ERRNO> {
        let node = self.inode.vfs_node();
        let cgroup = node
            .as_any()
            .downcast_ref::<CgroupDirNode>()
            .ok_or(ERRNO::EINVAL)?;
        cgroup.add_proc(pid);
        Ok(())
    }

    /// 返回当前普通文件对应的 page mapping；目录或不可缓存对象返回 `None`。
    fn page_mapping(&self) -> Option<page_cache::PageMappingHandle> {
        page_cache::mapping_for_inode(&self.inode)
    }

    /// read all data from the inode in memory
    pub fn read_all(&self) -> Vec<u8> {
        trace!("kernel: OSInode::read_all");
        let mut buffer: Vec<u8> = alloc::vec![0; 8192];
        let mut v: Vec<u8> = Vec::new();
        let mut offset = 0usize;
        let mapping = self.page_mapping();
        loop {
            let len = if let Some(mapping) = mapping.as_ref() {
                mapping.read(offset, &mut buffer)
            } else {
                self.inode.read_at(offset, &mut buffer)
            };
            if len == 0 {
                break;
            }
            offset += len;
            trace!("OSInode::read_all: read {} bytes, offset now {}", len, offset);
            v.extend_from_slice(&buffer[..len]);
        }
        v
    }

    /// 读取文件首行，返回 `(首行字节, 是否在限制内完整读到首行)`。
    pub fn read_first_line_limited(&self, max_len: usize) -> (Vec<u8>, bool) {
        trace!("kernel: OSInode::read_first_line_limited, max_len={}", max_len);
        let mut buf = alloc::vec![0; max_len];
        let read_len = if let Some(mapping) = self.page_mapping() {
            mapping.read(0, &mut buf)
        } else {
            self.inode.read_at(0, &mut buf)
        };
        buf.truncate(read_len);

        if let Some(line_end) = buf.iter().position(|&ch| ch == b'\n') {
            buf.truncate(line_end + 1);
            return (buf, true);
        }

        // 未读满上限说明已经到达 EOF，此时首行虽然没有换行符，也视为完整。
        let is_complete = read_len < max_len;
        (buf, is_complete)
    }
}

/// Special dirfd value meaning “use the caller's current working directory”.
pub const AT_FDCWD: isize = -100;
/// `unlinkat` flag for removing an empty directory instead of a non-directory.
pub const AT_REMOVEDIR: u32 = 0x200;
/// `newfstatat` 标志：返回符号链接自身状态而非目标状态。
pub const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
/// `newfstatat` 标志：允许空路径并直接作用于 `dirfd`。
pub const AT_EMPTY_PATH: u32 = 0x1000;
/// `linkat` flag: follow the old path if it is a symbolic link.
pub const AT_SYMLINK_FOLLOW: u32 = 0x400;

const MAX_SYMLINK_DEPTH: usize = 40;

#[inline]
fn inode_now() -> fs::vfs::InodeTime {
    let now_ns = get_realtime_ns();
    fs::vfs::InodeTime::new(now_ns / 1_000_000_000, (now_ns % 1_000_000_000) as u32)
}

lazy_static! {
    /// Tracks virtual directories created by `do_mount` for sub-path mounts.
    ///
    /// Maps absolute path → `Arc<VirtualDirNode>` for every virtual directory
    /// inserted into the namespace during mount operations.  Used by
    /// `ensure_virtual_dir` (to avoid recreating existing dirs) and
    /// `do_umount` (to clean up the registry).
    static ref VIRT_DIRS: SpinNoIrqLock<BTreeMap<String, Arc<VirtualDirNode>>> = SpinNoIrqLock::new(BTreeMap::new());

    /// The kernel's global root inode, backed by the virtual rootfs.
    ///
    /// Call [`init_rootfs`] once after `mm::init()` to overlay a real
    /// filesystem and make the full directory tree accessible.
    pub static ref ROOT_INODE: Arc<Inode> =
        Inode::from_vfs_node(Arc::clone(&VIRT_ROOT) as Arc<dyn VfsNode>);
}

/// A single mount-table entry exposed via `/proc/mounts`.
#[derive(Clone, Debug)]
pub(crate) struct MountRecord {
    pub(crate) source: String,
    pub(crate) target: String,
    pub(crate) fs_type: String,
    pub(crate) options: String,
}

impl MountRecord {
    fn is_readonly(&self) -> bool {
        self.options.split(',').any(|opt| opt == "ro")
    }
}

lazy_static! {
    /// Current mount table used by procfs.
    static ref MOUNT_TABLE: SpinNoIrqLock<BTreeMap<String, MountRecord>> =
        SpinNoIrqLock::new(BTreeMap::new());
}

pub(crate) fn snapshot_mount_table() -> Vec<MountRecord> {
    MOUNT_TABLE.lock().values().cloned().collect()
}

fn record_mount(target: &str, source: &str, fs_type: &str, options: &str) {
    MOUNT_TABLE.lock().insert(
        String::from(target),
        MountRecord {
            source: String::from(source),
            target: String::from(target),
            fs_type: String::from(fs_type),
            options: String::from(options),
        },
    );
}

fn remove_mount_record(target: &str) {
    MOUNT_TABLE.lock().remove(target);
}

fn update_mount_record(target: &str, source: &str, fs_type: &str, options: &str) -> Result<(), ERRNO> {
    let mut table = MOUNT_TABLE.lock();
    let record = table.get_mut(target).ok_or(ERRNO::EINVAL)?;
    record.source = String::from(source);
    record.fs_type = String::from(fs_type);
    record.options = String::from(options);
    Ok(())
}

/// Return whether the mounted filesystem covering `abs_path` is read-only.
///
/// The lookup uses the longest mounted-path prefix so sub-mounts override
/// their parents, matching Linux mount-namespace resolution.
pub fn mount_is_readonly(abs_path: &str) -> bool {
    let table = MOUNT_TABLE.lock();
    let mut best_match_len = 0usize;
    let mut readonly = false;

    for (target, record) in table.iter() {
        if !abs_path.starts_with(target) {
            continue;
        }
        if abs_path.len() != target.len()
            && !target.ends_with('/')
            && abs_path.as_bytes().get(target.len()) != Some(&b'/')
        {
            continue;
        }
        if target.len() >= best_match_len {
            best_match_len = target.len();
            readonly = record.is_readonly();
        }
    }

    readonly
}

fn prune_unused_virtual_dirs(start_path: &str) {
    let mut current = String::from(start_path);

    while current != "/" {
        let vdir = {
            let map = VIRT_DIRS.lock();
            map.get(current.as_str()).cloned()
        };
        let Some(vdir) = vdir else {
            break;
        };

        if vdir.mount_count() != 0 || vdir.keep_bound_without_children() {
            break;
        }

        let (parent_path, name) = split_for_mount(current.as_str());
        let parent_vdir = if parent_path == "/" {
            Arc::clone(&VIRT_ROOT)
        } else {
            let map = VIRT_DIRS.lock();
            match map.get(parent_path).cloned() {
                Some(parent) => parent,
                None => break,
            }
        };

        if !parent_vdir.unbind(name) {
            break;
        }
        VIRT_DIRS.lock().remove(current.as_str());
        current = String::from(parent_path);
    }
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
        let map = VIRT_DIRS.lock();
        if let Some(vdir) = map.get(abs_path) {
            return Ok(Arc::clone(vdir));
        }
    }

    // Create by ensuring the parent first (recursive, bounded by path depth).
    let (parent_path, name) = split_for_mount(abs_path);
    let parent_vdir = ensure_virtual_dir(parent_path)?;

    // If the current namespace already has a directory at this name (whether
    // from an explicit mount or from the backing overlay), preserve it as the
    // overlay of the new virtual dir so mount wrappers do not hide existing
    // mount points like `/tmp`.
    let preserve_mount_binding = parent_vdir.has_mount(name);
    let child_overlay: Option<Arc<dyn VfsNode>> = parent_vdir.namespace_child_dir(name);

    let new_vdir = VirtualDirNode::new();
    if let Some(ov) = child_overlay {
        new_vdir.set_overlay(ov);
    }
    if preserve_mount_binding {
        new_vdir.mark_persistent_mount_wrapper();
    }

    // Insert into the virtual namespace.
    parent_vdir.bind(name, Arc::clone(&new_vdir) as Arc<dyn VfsNode>);

    VIRT_DIRS
        .lock()
        .insert(String::from(abs_path), Arc::clone(&new_vdir));

    Ok(new_vdir)
}

/// Install a mount wrapper at `abs_path` and make `overlay` reachable through it.
///
/// The wrapper is the actual namespace node bound at `abs_path`; the mounted
/// filesystem becomes its overlay.  This lets later bind mounts alias the same
/// wrapper so that nested submounts remain visible through every alias.
fn install_mount_wrapper(
    abs_path: &str,
    overlay: Arc<dyn VfsNode>,
    persistent: bool,
) -> Result<Arc<VirtualDirNode>, ERRNO> {
    if abs_path == "/" {
        return Err(ERRNO::EINVAL);
    }

    let (parent_path, name) = split_for_mount(abs_path);
    let parent_vdir = ensure_virtual_dir(parent_path)?;
    let new_vdir = VirtualDirNode::new();
    new_vdir.set_overlay(overlay);
    if persistent {
        new_vdir.mark_persistent_mount_wrapper();
    }

    parent_vdir.bind(name, Arc::clone(&new_vdir) as Arc<dyn VfsNode>);
    VIRT_DIRS
        .lock()
        .insert(String::from(abs_path), Arc::clone(&new_vdir));
    Ok(new_vdir)
}

/// Ensure `abs_path` is backed by a mount wrapper and return that wrapper.
///
/// This is used to turn a plain directory into a bind-mount source so that
/// later recursive mounts can propagate through the same namespace node.
fn ensure_bind_source_wrapper(abs_path: &str) -> Result<Arc<VirtualDirNode>, ERRNO> {
    let abs = canonicalize("/", abs_path);
    if abs == "/" {
        return Err(ERRNO::EBUSY);
    }

    if let Some(existing) = VIRT_DIRS.lock().get(abs.as_str()).cloned() {
        return Ok(existing);
    }

    let inode = lookup_inode_follow("/", abs.as_str(), true)?;
    let overlay = inode.vfs_node();
    install_mount_wrapper(abs.as_str(), overlay, true)
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

    // For sub-paths, expose the mount through a wrapper so later bind mounts
    // can alias the same namespace node and share nested submounts.
    install_mount_wrapper(abs.as_str(), vfs_node, true)?;
    info!("[kernel] mounted fs at {}", abs);
    Ok(())
}

/// Bind the namespace node at `source_path` onto `target_path`.
pub fn do_bind_mount(source_path: &str, target_path: &str) -> Result<(), ERRNO> {
    let src_abs = canonicalize("/", source_path);
    let dst_abs = canonicalize("/", target_path);

    if src_abs == "/" || dst_abs == "/" {
        return Err(ERRNO::EBUSY);
    }

    let src_wrapper = ensure_bind_source_wrapper(src_abs.as_str())?;
    let dst_vfs_node: Arc<dyn VfsNode> = Arc::clone(&src_wrapper) as Arc<dyn VfsNode>;
    let (parent_path, name) = split_for_mount(dst_abs.as_str());
    let parent_vdir = ensure_virtual_dir(parent_path)?;
    parent_vdir.bind(name, dst_vfs_node);
    VIRT_DIRS
        .lock()
        .insert(dst_abs.clone(), Arc::clone(&src_wrapper));
    info!("[kernel] bind-mounted {} at {}", src_abs, dst_abs);
    Ok(())
}

/// Move an existing mount wrapper from `source_path` to `target_path`.
pub fn do_move_mount(source_path: &str, target_path: &str) -> Result<(), ERRNO> {
    let src_abs = canonicalize("/", source_path);
    let dst_abs = canonicalize("/", target_path);

    if src_abs == "/" || dst_abs == "/" {
        return Err(ERRNO::EBUSY);
    }
    if src_abs == dst_abs {
        return Ok(());
    }
    if dst_abs.starts_with(&(src_abs.clone() + "/")) {
        return Err(ERRNO::EINVAL);
    }

    let src_wrapper = {
        let map = VIRT_DIRS.lock();
        map.get(src_abs.as_str()).cloned().ok_or(ERRNO::EINVAL)?
    };

    let (src_parent_path, src_name) = split_for_mount(src_abs.as_str());
    let src_parent = if src_parent_path == "/" {
        Arc::clone(&VIRT_ROOT)
    } else {
        VIRT_DIRS
            .lock()
            .get(src_parent_path)
            .cloned()
            .ok_or(ERRNO::EINVAL)?
    };
    if !src_parent.unbind(src_name) {
        return Err(ERRNO::EINVAL);
    }

    let (dst_parent_path, dst_name) = split_for_mount(dst_abs.as_str());
    let dst_parent = ensure_virtual_dir(dst_parent_path)?;
    dst_parent.bind(dst_name, Arc::clone(&src_wrapper) as Arc<dyn VfsNode>);

    {
        let mut map = VIRT_DIRS.lock();
        map.remove(src_abs.as_str());
        map.insert(dst_abs.clone(), Arc::clone(&src_wrapper));
    }

    prune_unused_virtual_dirs(src_parent_path);
    info!("[kernel] moved mount {} -> {}", src_abs, dst_abs);
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
            .lock()
            .get(parent_path)
            .cloned()
            .ok_or(ERRNO::EINVAL)?
    };

    if !parent_vdir.unbind(name) {
        return Err(ERRNO::EINVAL);
    }

    remove_mount_record(&abs);

    // Clean up the registry entry (no-op if `abs` was a real-FS mount, not
    // a VirtualDirNode we created).
    VIRT_DIRS.lock().remove(&abs);
    prune_unused_virtual_dirs(parent_path);

    info!("[kernel] unmounted {}", abs);
    Ok(())
}

/// Mount the compiled-in filesystem at `"/"` and log the result.
///
/// Must be called **after** `mm::init()` (heap allocator required for `Arc`
/// and filesystem initialisation) and before any file-system operations.
/// Invoked from `rust_main` in `main.rs`.
pub fn init_rootfs() -> Result<(), ERRNO> {
    let (root_dev_name, root_dev, extra_dev) = {
        let primary_name = block_device_name(0);
        let secondary_name = block_device_name(1);
        let primary_path = alloc::format!("/dev/{}", primary_name);
        let secondary_path = alloc::format!("/dev/{}", secondary_name);
        let map = BLOCK_DEVICES.lock();
        if let Some(dev) = map.get(&secondary_name).cloned() {
            let extra_dev = map
                .get(&primary_name)
                .cloned()
                .map(|dev| (primary_path.clone(), dev));
            (secondary_path, dev, extra_dev)
        } else {
            let dev = map
                .get(&primary_name)
                .cloned()
                .expect("[kernel] rootfs primary block device not found");
            (primary_path, dev, None)
        }
    };

    #[cfg(feature = "fat32")]
    {
        use fs::Fat32FileSystem;
        let vfs = Fat32FileSystem::open(root_dev.clone()).map_err(ERRNO::from)?;
        let root = Fat32FileSystem::root_inode(&vfs);
        do_mount("/", root)?;
        record_mount("/", root_dev_name.as_str(), "fat32", "rw");
    }
    #[cfg(feature = "easyfs")]
    {
        use fs::EasyFileSystem;
        let efs = EasyFileSystem::open(root_dev.clone());
        let root = EasyFileSystem::root_inode(&efs);
        do_mount("/", root).unwrap_or_else(|_| panic!("[kernel] failed to mount easyfs at /"));
        record_mount("/", root_dev_name.as_str(), "easyfs", "rw");
    }
    #[cfg(feature = "ext4")]
    {
        use fs::Ext4FileSystem;
        let efs = Ext4FileSystem::open(root_dev.clone());
        let root = Ext4FileSystem::root_inode(&efs);
        do_mount("/", root.clone()).unwrap_or_else(|_| panic!("[kernel] failed to mount ext4 at /"));
        record_mount("/", root_dev_name.as_str(), "ext4", "rw");
        if let Some((extra_dev_name, extra_dev)) = extra_dev {
            let extra_fs = Ext4FileSystem::open(extra_dev);
            let extra_root = Ext4FileSystem::root_inode(&extra_fs);
            do_mount("/mnt", extra_root)
                .unwrap_or_else(|_| panic!("[kernel] failed to mount ext4 at /mnt"));
            record_mount("/mnt", extra_dev_name.as_str(), "ext4", "rw");
            info!("[kernel] mounted extra ext4 at /mnt from {}", extra_dev_name);
        }
    }

    let tmpfs_root = Inode::from_vfs_node(new_tmpfs_root());
    do_mount("/tmp", tmpfs_root)
        .unwrap_or_else(|_| panic!("[kernel] failed to mount tmpfs at /tmp"));
    record_mount("/tmp", "tmpfs", "tmpfs", "rw");

    info!("[kernel] rootfs initialised");
    Ok(())
}

/// List all apps in the root directory
pub fn list_apps() {
    println!("/**** APPS ****");
    for (app, _) in ROOT_INODE.ls() {
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

fn join_remaining(target: &str, rest: &[String]) -> String {
    if rest.is_empty() {
        return String::from(target);
    }
    let mut out = String::from(target);
    if !out.ends_with('/') {
        out.push('/');
    }
    for (idx, component) in rest.iter().enumerate() {
        if idx != 0 {
            out.push('/');
        }
        out.push_str(component);
    }
    out
}

/// Resolve a path and optionally leave the final symlink unresolved, returning
/// both the target inode and the final canonical path after symlink expansion.
pub fn lookup_inode_follow_with_path(
    cwd: &str,
    path: &str,
    follow_final: bool,
) -> Result<(Arc<Inode>, String), ERRNO> {
    let mut abs = canonicalize(cwd, path);
    let mut depth = 0usize;

    loop {
        let components: Vec<String> = abs
            .split('/')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        if components.is_empty() {
            return Ok((Arc::clone(&ROOT_INODE), String::from("/")));
        }

        let mut cur = Arc::clone(&ROOT_INODE);
        let mut cur_path = String::from("/");
        let mut restart: Option<String> = None;

        for (idx, component) in components.iter().enumerate() {
            if !cur.is_dir() {
                return Err(ERRNO::ENOTDIR);
            }
            let child = cur.find(component).ok_or(ERRNO::ENOENT)?;
            let is_final = idx + 1 == components.len();
            if child.is_symlink() && (!is_final || follow_final) {
                depth += 1;
                if depth > MAX_SYMLINK_DEPTH {
                    return Err(ERRNO::ELOOP);
                }
                let target = child.read_link().map_err(ERRNO::from)?;
                let base = if target.starts_with('/') {
                    String::from("/")
                } else {
                    cur_path.clone()
                };
                let combined = join_remaining(target.as_str(), &components[idx + 1..]);
                restart = Some(canonicalize(base.as_str(), combined.as_str()));
                break;
            }

            cur = child;
            if cur_path != "/" {
                cur_path.push('/');
            }
            cur_path.push_str(component);
        }

        if let Some(next_abs) = restart {
            abs = next_abs;
        } else {
            return Ok((cur, abs));
        }
    }
}

/// Resolve a path and optionally leave the final symlink unresolved.
pub fn lookup_inode_follow(cwd: &str, path: &str, follow_final: bool) -> Result<Arc<Inode>, ERRNO> {
    lookup_inode_follow_with_path(cwd, path, follow_final).map(|(inode, _)| inode)
}

/// Resolve parent directory with symlinks followed in all parent components.
fn resolve_parent_follow(cwd: &str, path: &str) -> Result<(Arc<Inode>, String, String), ERRNO> {
    let abs = canonicalize(cwd, path);
    if abs == "/" {
        return Err(ERRNO::ENOENT);
    }
    let (parent_path, filename) = match abs.rfind('/') {
        Some(0) => (String::from("/"), String::from(&abs[1..])),
        Some(idx) => (String::from(&abs[..idx]), String::from(&abs[idx + 1..])),
        None => (String::from("/"), abs.clone()),
    };
    if filename.is_empty() {
        return Err(ERRNO::ENOENT);
    }
    let parent = lookup_inode_follow("/", parent_path.as_str(), true)?;
    Ok((parent, filename, parent_path))
}

/// Resolve `path` into (parent_directory_inode, filename).
/// Returns `None` if the parent directory does not exist.
fn resolve_parent(cwd: &str, path: &str) -> Option<(Arc<Inode>, String)> {
    resolve_parent_follow(cwd, path)
        .ok()
        .map(|(parent, filename, _)| (parent, filename))
}

fn runtime_block_inode(abs_path: &str, inode: &Arc<Inode>) -> Option<Arc<Inode>> {
    let dev_name = abs_path.strip_prefix("/dev/")?;
    if dev_name.contains('/') {
        return None;
    }
    let dev = BLOCK_DEVICES.lock().get(dev_name).cloned()?;
    if inode.file_type() != VfsFileType::Block && !dev_name.starts_with("vd") {
        return None;
    }
    let minor = super::devfs::blkdev_minor_from_name(dev_name);
    Some(Inode::from_vfs_node(
        Arc::new(BlockDevNode::new(dev, minor)) as Arc<dyn VfsNode>
    ))
}

/// Open (or optionally create) a file/directory at `path` relative to `cwd`.
pub fn open_file_at_with_status(
    cwd: &str,
    path: &str,
    flags: OpenFlags,
) -> Result<(Arc<OSInode>, bool), ERRNO> {
    trace!("kernel: open_file_at: cwd={}, path={}, flags={:?}", cwd, path, flags);
    let abs = canonicalize(cwd, path);
    debug!("open_file_at: path = {} -> abs path = {}", path, abs);

    if flags.contains(OpenFlags::CREATE) {
        // Navigate to the parent directory and create the file there.
        let (parent, name, parent_path) = resolve_parent_follow(cwd, path)?;
        if let Some(existing) = parent.find(&name) {
            // 已存在文件时，`O_CREAT` 只负责“存在则直接打开”，不能隐式截断。
            debug!("EXCL flag valid: {}", flags.contains(OpenFlags::EXCL));
            if flags.contains(OpenFlags::EXCL) {
                return Err(ERRNO::EEXIST);
            }
            if existing.is_symlink() && flags.contains(OpenFlags::NOFOLLOW) {
                return Err(ERRNO::ELOOP);
            }
            let mut inode = if existing.is_symlink() {
                let existing_path = canonicalize(parent_path.as_str(), name.as_str());
                lookup_inode_follow("/", existing_path.as_str(), true)?
            } else {
                existing
            };
            if let Some(block_inode) = runtime_block_inode(abs.as_str(), &inode) {
                inode = block_inode;
            }
            if flags.contains(OpenFlags::TRUNC) {
                if inode.file_type() == VfsFileType::Regular {
                    page_cache::truncate_inode(&inode, 0).map_err(ERRNO::from)?;
                }
            }
            Ok((Arc::new(OSInode::new(inode, abs.clone())), false))
        } else {
            parent
                .create(&name)
                .map(|inode| {
                    let _ = inode.set_times_now(inode_now());
                    (Arc::new(OSInode::new(inode, abs.clone())), true)
                })
                .ok_or(ERRNO::EIO)
        }
    } else {
        let mut inode = lookup_inode_follow(cwd, path, !flags.contains(OpenFlags::NOFOLLOW))?;
        if inode.is_symlink() {
            return Err(ERRNO::ELOOP);
        }
        if let Some(block_inode) = runtime_block_inode(abs.as_str(), &inode) {
            inode = block_inode;
        }
        {
            if flags.contains(OpenFlags::TRUNC) && inode.file_type() == VfsFileType::Regular {
                debug!("open_file_at: truncating existing file at {}", abs);
                page_cache::truncate_inode(&inode, 0).map_err(ERRNO::from)?;
            }
            Ok((Arc::new(OSInode::new(inode, abs.clone())), false))
        }
    }
}

/// Open (or optionally create) a file/directory at `path` relative to `cwd`.
pub fn open_file_at(cwd: &str, path: &str, flags: OpenFlags) -> Result<Arc<OSInode>, ERRNO> {
    open_file_at_with_status(cwd, path, flags).map(|(inode, _created)| inode)
}

/// Create a directory at `path` relative to `cwd`.
/// Returns `true` on success.
pub fn mkdir_at(cwd: &str, path: &str) -> Result<(), ERRNO> {
    mkdir_at_with_inode(cwd, path).map(|_| ())
}

/// Create a directory at `path` relative to `cwd` and return the new inode.
pub fn mkdir_at_with_inode(cwd: &str, path: &str) -> Result<Arc<Inode>, ERRNO> {
    if let Ok((parent, name, _)) = resolve_parent_follow(cwd, path) {
        // 已存在同名目录或文件
        if parent.find(&name).is_some() {
            return Err(ERRNO::EEXIST);
        }
        // 父节点不是目录
        if !parent.is_dir() {
            return Err(ERRNO::ENOTDIR);
        }
        // 创建失败
        if let Some(inode) = parent.mkdir(&name) {
            let _ = inode.set_times_now(inode_now());
            Ok(inode)
        } else {
            Err(ERRNO::EIO)
        }
    } else if lookup_inode_follow(cwd, path, true).is_ok() {
        Err(ERRNO::EEXIST)
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
        /// fail if file exists
        const EXCL = 0x80;
        /// truncate file size to 0
        const TRUNC = 0x200;
        /// open directory
        const DIRECTORY = 0x10000;
        /// fail if the final component is a symbolic link
        const NOFOLLOW = 0x20000;
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
    let abs = canonicalize("/", name);
    if flags.contains(OpenFlags::CREATE) {
        if let Some(inode) = ROOT_INODE.find(name) {
            // 与 `openat(O_CREAT)` 保持一致：只有显式 `O_TRUNC` 才清空已有文件。
            if flags.contains(OpenFlags::TRUNC) {
                page_cache::truncate_inode(&inode, 0).ok()?;
            }
            Some(Arc::new(OSInode::new(inode, abs)))
        } else {
            // create file
            ROOT_INODE.create(name).map(|inode| {
                let _ = inode.set_times_now(inode_now());
                Arc::new(OSInode::new(inode, canonicalize("/", name)))
            })
        }
    } else {
        ROOT_INODE.find(name).and_then(|inode| {
            if flags.contains(OpenFlags::TRUNC) {
                page_cache::truncate_inode(&inode, 0).ok()?;
            }
            Some(Arc::new(OSInode::new(inode, abs)))
        })
    }
}

/// Create a hard link from `old_path` to `new_path`.
pub fn linkat(old_cwd: &str, old_path: &str, new_cwd: &str, new_path: &str) -> Result<(), ERRNO> {
    linkat_with_flags(old_cwd, old_path, new_cwd, new_path, 0)
}

/// Create a hard link from `old_path` to `new_path` with Linux `linkat` flags.
pub fn linkat_with_flags(
    old_cwd: &str,
    old_path: &str,
    new_cwd: &str,
    new_path: &str,
    flags: u32,
) -> Result<(), ERRNO> {
    if flags & !AT_SYMLINK_FOLLOW != 0 {
        return Err(ERRNO::EINVAL);
    }
    let (_, old_name, _) = resolve_parent_follow(old_cwd, old_path)?;
    let (new_parent, new_name, _) = resolve_parent_follow(new_cwd, new_path)?;
    if old_name.is_empty() || new_name.is_empty() {
        return Err(ERRNO::ENOENT);
    }
    let (old_parent, old_name, old_parent_path) = resolve_parent_follow(old_cwd, old_path)?;
    let old_inode = if flags & AT_SYMLINK_FOLLOW != 0 {
        let old_abs = canonicalize(old_parent_path.as_str(), old_name.as_str());
        lookup_inode_follow("/", old_abs.as_str(), true)?
    } else {
        old_parent.find(old_name.as_str()).ok_or(ERRNO::ENOENT)?
    };
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

/// Create a symbolic link named `link_path` containing `target`.
pub fn symlinkat(target: &str, cwd: &str, link_path: &str) -> Result<(), ERRNO> {
    let (parent, name, _) = resolve_parent_follow(cwd, link_path)?;
    if parent.find(name.as_str()).is_some() {
        return Err(ERRNO::EEXIST);
    }
    let inode = parent.symlink(name.as_str(), target).map_err(ERRNO::from)?;
    let _ = inode.set_times_now(inode_now());
    Ok(())
}

/// Rename a path from `old_path` to `new_path`.
///
/// Linux `renameat(2)` requires the target replacement to be atomic, so the
/// operation is always delegated to the backend's native `rename_child`
/// primitive instead of being emulated with `link + unlink`.
pub fn rename_at(
    old_cwd: &str,
    old_path: &str,
    new_cwd: &str,
    new_path: &str,
) -> Result<(), ERRNO> {
    let old_abs = canonicalize(old_cwd, old_path);
    let new_abs = canonicalize(new_cwd, new_path);
    if old_abs == new_abs {
        return Ok(());
    }

    let (old_parent, old_name) = resolve_parent(old_cwd, old_path).ok_or(ERRNO::ENOENT)?;
    let (new_parent, new_name) = resolve_parent(new_cwd, new_path).ok_or(ERRNO::ENOENT)?;
    if old_name.is_empty() || new_name.is_empty() {
        return Err(ERRNO::ENOENT);
    }
    if !old_parent.is_dir() || !new_parent.is_dir() {
        return Err(ERRNO::ENOTDIR);
    }

    let old_inode = old_parent.find(old_name.as_str()).ok_or(ERRNO::ENOENT)?;
    if old_inode.is_dir() {
        let mut old_abs_prefix = old_abs.clone();
        if !old_abs_prefix.ends_with('/') {
            old_abs_prefix.push('/');
        }
        if new_abs.starts_with(old_abs_prefix.as_str()) {
            return Err(ERRNO::EINVAL);
        }
    }
    if let Some(new_inode) = new_parent.find(new_name.as_str()) {
        if old_inode.ino() == new_inode.ino() {
            return Ok(());
        }
        if old_inode.is_dir() && !new_inode.is_dir() {
            return Err(ERRNO::ENOTDIR);
        }
        if !old_inode.is_dir() && new_inode.is_dir() {
            return Err(ERRNO::EISDIR);
        }
        if new_inode.is_dir() && !new_inode.ls().is_empty() {
            return Err(ERRNO::ENOTEMPTY);
        }
    }

    old_parent.rename_child(old_name.as_str(), &new_parent, new_name.as_str())?;
    Ok(())
}

/// Remove a link at `path` relative to `cwd`.
pub fn unlinkat(cwd: &str, path: &str, flags: u32) -> Result<(), ERRNO> {
    debug!("unlinkat: cwd={}, path={}, flags={:#x}", cwd, path, flags);
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
        // if !inode.ls().is_empty() {
        if inode.ls().len() > 2 {
            // Contains at least one entry other than `.` and `..`
            return Err(ERRNO::ENOTEMPTY);
        }
        parent.rmdir(name.as_str())?
    } else {
        if flags & AT_REMOVEDIR != 0 {
            return Err(ERRNO::ENOTDIR);
        }
        // 删除普通文件前丢弃旧页，避免已经解除目录项后仍有脏页迟到回写。
        discard_inode(&inode);
        if let Err(err) = parent.unlink(name.as_str()) {
            error!(
                "[unlinkat] unlink regular file failed: cwd={} path={} name={} errno={}",
                cwd,
                path,
                name,
                err as i32
            );
            return Err(err.into());
        }
    }
    Ok(())
}

impl File for OSInode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    /// file readable?
    fn readable(&self) -> bool {
        // 目录项遍历走 `getdents64`，普通 `read` 仅对常规文件开放。
        !self.inode.is_dir()
    }
    /// file writable?
    fn writable(&self) -> bool {
        // 目录修改应走 `mkdir/unlink/link` 等路径操作，而不是普通 `write`。
        !self.inode.is_dir()
    }
    fn is_dir(&self) -> bool {
        self.inode.is_dir()
    }
    fn read_at(&self, offset: usize, mut buf: UserBuffer) -> usize {
        let mapping = self.page_mapping();
        let mut file_off = offset;
        let mut total_read_size = 0usize;
        for slice in buf.buffers.iter_mut() {
            let read_size = if let Some(mapping) = mapping.as_ref() {
                mapping.read(file_off, *slice)
            } else {
                self.inode.read_at(file_off, *slice)
            };
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
    fn read_bytes_at(&self, offset: usize, buf: &mut [u8]) -> Result<usize, ERRNO> {
        let mapping = self.page_mapping();
        Ok(if let Some(mapping) = mapping.as_ref() {
            mapping.read(offset, buf)
        } else {
            self.inode.read_at(offset, buf)
        })
    }
    fn write_at(&self, offset: usize, buf: UserBuffer) -> usize {
        let mapping = self.page_mapping();
        let mut total_write_size = 0usize;
        let mut file_off = offset;
        for slice in buf.buffers.iter() {
            let write_size = if let Some(mapping) = mapping.as_ref() {
                mapping.write(file_off, *slice)
            } else {
                self.inode.write_at(file_off, *slice)
            };
            assert_eq!(write_size, slice.len());
            file_off += write_size;
            total_write_size += write_size;
        }
        total_write_size
    }
    fn write_bytes_at(&self, offset: usize, buf: &[u8]) -> Result<usize, ERRNO> {
        Ok(if let Some(mapping) = self.page_mapping().as_ref() {
            mapping.write(offset, buf)
        } else {
            self.inode.write_at(offset, buf)
        })
    }

    fn truncate(&self, new_size: usize) -> Result<(), ERRNO> {
        page_cache::truncate_inode(&self.inode, new_size).map_err(ERRNO::from)
    }

    fn fallocate(&self, mode: i32, offset: usize, len: usize) -> Result<(), ERRNO> {
        page_cache::fallocate_inode(&self.inode, mode, offset, len).map_err(ERRNO::from)
    }

    fn ioctl(&self, req: usize, arg: usize) -> Result<isize, ERRNO> {
        let vfs_node = self.inode.vfs_node();
        if let Some(rtc) = vfs_node.as_any().downcast_ref::<RtcDevNode>() {
            return rtc.ioctl(req, arg);
        }
        if let Some(tty) = vfs_node.as_any().downcast_ref::<TtyDeviceNode>() {
            return tty.ioctl(req, arg);
        }
        if let Some(block) = vfs_node.as_any().downcast_ref::<BlockDevNode>() {
            return block.ioctl(req, arg);
        }
        Err(ERRNO::ENOTTY)
    }
    fn getdents64(&self, offset: usize, buf: &mut [u8]) -> usize {
        if !self.inode.is_dir() {
            return 0;
        }
        self.inode.getdents64(offset, buf)
    }

    fn is_seekable(&self) -> bool {
        true
    }

    fn as_inode(&self) -> Option<Arc<Inode>> {
        Some(Arc::clone(&self.inode))
    }

    fn stat(&self) -> Stat {
        let vfs_node = self.inode.vfs_node();
        if let Some(rtc) = vfs_node.as_any().downcast_ref::<RtcDevNode>() {
            return rtc.stat();
        }
        inode_stat(&self.inode)
    }

    fn statfs(&self) -> Result<StatFs64, ERRNO> {
        self.inode.statfs().map_err(ERRNO::from)
    }

    fn sync(&self) -> Result<(), ERRNO> {
        if let Some(mapping) = self.page_mapping() {
            mapping.sync();
        }
        Ok(())
    }

    fn path(&self) -> Option<String> {
        Some(self.path.clone())
    }

    fn chmod(&self, mode: u32) -> Result<(), fs::errno::FS_ERRNO> {
        self.inode.set_mode(mode)?;
        Ok(())
    }

    fn backing_inode(&self) -> Option<Arc<Inode>> {
        Some(Arc::clone(&self.inode))
    }
}

/// 根据底层 inode 构造 `stat` 结果，供 `fstat` 与 `newfstatat` 共用。
pub fn inode_stat(inode: &Arc<Inode>) -> Stat {
    // Read all attributes in one batched call (single lock acquisition for
    // backends like ext4, instead of one per field).
    let attrs = inode.stat_attrs();
    let file_type = inode.file_type();
    let mode = StatMode::from_bits_truncate(attrs.mode.unwrap_or(
        match file_type {
            VfsFileType::Directory => StatMode::DIR.bits() | 0o755,
            VfsFileType::Symlink => StatMode::LINK.bits() | 0o777,
            VfsFileType::Char => StatMode::CHAR.bits() | 0o666,
            VfsFileType::Block => StatMode::BLOCK.bits() | 0o660,
            VfsFileType::Fifo => StatMode::FIFO.bits() | 0o666,
            VfsFileType::Socket => StatMode::SOCK.bits() | 0o777,
            VfsFileType::Regular | VfsFileType::Unknown => StatMode::FILE.bits() | 0o644,
        }
    ));
    // Prefer the page-cache size for regular files that have dirty mappings;
    // fall back to the batched attribute read otherwise.
    let size = if file_type == VfsFileType::Regular {
        page_cache::cached_inode_size_fast(inode, attrs.size)
    } else {
        attrs.size
    };
    Stat {
        dev: 0,
        ino: attrs.ino,
        mode,
        nlink: attrs.nlink,
        uid: attrs.uid.unwrap_or(0),
        gid: attrs.gid.unwrap_or(0),
        rdev: attrs.rdev,
        pad0: 0,
        size: size as i64,
        blksize: 512,
        pad1: 0,
        blocks: (size as u64).div_ceil(512),
        atime_sec: attrs.atime.map(|t| t.sec as isize).unwrap_or(0),
        atime_nsec: attrs.atime.map(|t| t.nsec as isize).unwrap_or(0),
        mtime_sec: attrs.mtime.map(|t| t.sec as isize).unwrap_or(0),
        mtime_nsec: attrs.mtime.map(|t| t.nsec as isize).unwrap_or(0),
        ctime_sec: attrs.ctime.map(|t| t.sec as isize).unwrap_or(0),
        ctime_nsec: attrs.ctime.map(|t| t.nsec as isize).unwrap_or(0),
        unused: [0; 2],
    }
}

// ---------------------------------------------------------------------------
// Device-filesystem helpers
// ---------------------------------------------------------------------------

/// Populate `/dev` with one [`BlockDevNode`] per discovered block device.
///
/// Must be called **after** both [`probe_block_devices`](crate::drivers::block::probe_block_devices)
/// and [`init_rootfs`].  The `/dev` directory is provided by a dedicated
/// devfs mount.
pub fn init_dev() {
    let dev_dir = ensure_virtual_dir("/dev")
        .unwrap_or_else(|_| panic!("[kernel] failed to create /dev"));
    let tmpfs_root = Inode::from_vfs_node(new_tmpfs_root());
    do_mount("/dev/shm", tmpfs_root)
        .unwrap_or_else(|_| panic!("[kernel] failed to mount tmpfs at /dev/shm"));
    record_mount("/dev/shm", "tmpfs", "tmpfs", "rw");
    // Register special character devices under /dev
    let null_node = Arc::new(NullDevNode::new());
    dev_dir.bind("null", null_node as Arc<dyn VfsNode>);
    info!("[kernel] /dev/null registered");

    let cpu_dma_latency_node = Arc::new(CpuDmaLatencyNode::new());
    dev_dir.bind("cpu_dma_latency", cpu_dma_latency_node as Arc<dyn VfsNode>);
    info!("[kernel] /dev/cpu_dma_latency registered");

    let console_node: Arc<dyn VfsNode> = Arc::new(crate::fs::TtyDeviceNode::new(crate::fs::TtyDeviceKind::Console, 0x0501));
    dev_dir.bind("console", Arc::clone(&console_node));
    let tty_node: Arc<dyn VfsNode> = Arc::new(crate::fs::TtyDeviceNode::new(crate::fs::TtyDeviceKind::Tty, 0x0500));
    dev_dir.bind("tty", tty_node);
    info!("[kernel] /dev/console and /dev/tty registered");

    ensure_ltp_scratch_device();

    // Register discovered block devices (e.g. /dev/vda, /dev/vda2, /dev/vdb).
    let map = BLOCK_DEVICES.lock();
    for (dev_name, dev) in map.iter() {
        let major = super::devfs::blkdev_major_from_name(dev_name);
        let minor = super::devfs::blkdev_minor_from_name(dev_name);
        let node = Arc::new(BlockDevNode::new_with_major(Arc::clone(dev), major, minor));
        dev_dir.bind(dev_name, node as Arc<dyn VfsNode>);
        info!("[kernel] /dev/{} registered", dev_name);
    }

    // Register RTC aliases for Linux userland compatibility (e.g. BusyBox hwclock).
    let misc_dir = ensure_virtual_dir("/dev/misc")
        .unwrap_or_else(|_| panic!("[kernel] failed to create /dev/misc"));
    let rtc_node: Arc<dyn VfsNode> = Arc::new(RtcDevNode::new());
    dev_dir.bind("rtc", Arc::clone(&rtc_node));
    dev_dir.bind("rtc0", Arc::clone(&rtc_node));
    misc_dir.bind("rtc", rtc_node);
    info!("[kernel] /dev/rtc, /dev/rtc0 and /dev/misc/rtc registered");

    // Register /dev/urandom and /dev/random (map to same CSPRNG device).
    let urandom_node: Arc<dyn VfsNode> = Arc::new(UrandomDevNode::new());
    dev_dir.bind("urandom", Arc::clone(&urandom_node));
    dev_dir.bind("random", Arc::clone(&urandom_node));
    info!("[kernel] /dev/urandom and /dev/random registered");

    let zero_node: Arc<dyn VfsNode> = Arc::new(ZeroDevNode::new());
    dev_dir.bind("zero", zero_node);

    info!("[kernel] /dev initialized");
}

/// Mount procfs at `/proc`.
///
/// Must be called after `init_rootfs` so the virtual root is ready.
pub fn init_procfs() {
    let proc_root: Arc<dyn VfsNode> = Arc::new(ProcRootNode::new());
    let proc_inode = Inode::from_vfs_node(proc_root);
    do_mount("/proc", proc_inode)
        .unwrap_or_else(|_| panic!("[kernel] failed to mount procfs at /proc"));
    record_mount("/proc", "proc", "proc", "rw");
    info!("[kernel] /proc initialized");
}

/// Mount sysfs at `/sys`.
pub fn init_sysfs() {
    let sys_root: Arc<dyn VfsNode> = Arc::new(SysRootNode::new());
    let sys_inode = Inode::from_vfs_node(sys_root);
    do_mount("/sys", sys_inode)
        .unwrap_or_else(|_| panic!("[kernel] failed to mount sysfs at /sys"));
    record_mount("/sys", "sysfs", "sysfs", "rw");
    info!("[kernel] /sys initialized");
}

/// Mount the filesystem on `dev_path` at the absolute path `abs_mnt`.
///
/// `dev_path` must resolve to a [`BlockDevNode`] in the VFS (e.g. `/dev/vda`).
/// `abs_mnt` must be an already-canonicalized absolute pathname.
/// `fs_type` is a filesystem type string: `"vfat"`, `"fat32"`, `"ext2"`,
/// `"ext3"`, or `"ext4"`.
pub fn mount_device(dev_path: &str, abs_mnt: &str, fs_type: &str, readonly: bool) -> Result<(), ERRNO> {
    debug!(
        "mount_device: dev_path={}, abs_mnt={}, fs_type={}",
        dev_path,
        abs_mnt,
        fs_type,
    );
    let dev_inode = lookup_inode_follow("/", dev_path, true).or(Err(ERRNO::ENODEV))?;
    let vfs_node = dev_inode.vfs_node();
    let block_dev_node = vfs_node
        .as_any()
        .downcast_ref::<BlockDevNode>()
        .ok_or(ERRNO::ENOTBLK)?;
    let block_dev = Arc::clone(&block_dev_node.device);

    let fs_root: Arc<Inode> = match fs_type {
        "vfat" | "fat32" | "ext2" | "ext3" => Inode::from_vfs_node(new_tmpfs_root()),
        #[cfg(feature = "ext4")]
        "ext4" => {
            use fs::Ext4FileSystem;
            let vfs = Ext4FileSystem::open(block_dev);
            Ext4FileSystem::root_inode(&vfs)
        }
        _ => return Err(ERRNO::ENODEV),
    };

    do_mount(abs_mnt, fs_root)?;
    record_mount(
        abs_mnt,
        &canonicalize("/", dev_path),
        fs_type,
        if readonly { "ro" } else { "rw" },
    );
    Ok(())
}

/// Mount a fresh tmpfs instance at `abs_mnt`.
pub fn mount_tmpfs(abs_mnt: &str, readonly: bool) -> Result<(), ERRNO> {
    let fs_root = Inode::from_vfs_node(new_tmpfs_root());
    do_mount(abs_mnt, fs_root)?;
    record_mount(abs_mnt, "tmpfs", "tmpfs", if readonly { "ro" } else { "rw" });
    Ok(())
}

/// Mount a sysfs view rooted at `abs_mnt`.
pub fn mount_sysfs(abs_mnt: &str, readonly: bool) -> Result<(), ERRNO> {
    let fs_root = Inode::from_vfs_node(Arc::new(SysRootNode::new()) as Arc<dyn VfsNode>);
    do_mount(abs_mnt, fs_root)?;
    record_mount(abs_mnt, "sysfs", "sysfs", if readonly { "ro" } else { "rw" });
    Ok(())
}

/// Mount a minimal cgroup v2 hierarchy at `abs_mnt`.
pub fn mount_cgroup2(abs_mnt: &str, readonly: bool) -> Result<(), ERRNO> {
    let fs_root = Inode::from_vfs_node(new_cgroup2_root());
    do_mount(abs_mnt, fs_root)?;
    record_mount(abs_mnt, "cgroup2", "cgroup2", if readonly { "ro" } else { "rw" });
    Ok(())
}

/// Update an existing mount record in place, preserving the mounted tree.
///
/// CosmOS currently tracks remount state in mount metadata so path-based checks
/// can observe `ro` versus `rw` without rebuilding the mounted filesystem.
pub fn remount_path(abs_mnt: &str, dev_path: &str, fs_type: &str, readonly: bool) -> Result<(), ERRNO> {
    update_mount_record(
        abs_mnt,
        dev_path,
        fs_type,
        if readonly { "ro" } else { "rw" },
    )
}
