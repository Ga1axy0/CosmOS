//! Minimal sysfs implementation for `/sys/class/net`.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::Write;

use fs::vfs::{VfsFileType, VfsNode};

use crate::net::compat::{self, CompatNetIfInfo};

const SYSFS_MAGIC: u64 = 0x6265_6572;

fn read_string_at(data: String, offset: usize, buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let bytes = data.as_bytes();
    if offset >= bytes.len() {
        return 0;
    }
    let end = (offset + buf.len()).min(bytes.len());
    let len = end - offset;
    buf[..len].copy_from_slice(&bytes[offset..end]);
    len
}

fn fmt_mac(mac: [u8; 6]) -> String {
    let mut out = String::new();
    let _ = write!(
        &mut out,
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    out
}

#[derive(Default, Debug)]
pub(crate) struct SysRootNode;

impl SysRootNode {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl VfsNode for SysRootNode {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn file_type(&self) -> VfsFileType {
        VfsFileType::Directory
    }
    fn ls(&self) -> Vec<(String, VfsFileType)> {
        alloc::vec![(String::from("class"), VfsFileType::Directory)]
    }
    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        match name {
            "class" => Some(Arc::new(SysClassNode) as Arc<dyn VfsNode>),
            _ => None,
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
            SYSFS_MAGIC,
            crate::config::PAGE_SIZE as u64,
            SYSFS_MAGIC,
            255,
        ))
    }
}

#[derive(Default, Debug)]
struct SysClassNode;

impl VfsNode for SysClassNode {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn file_type(&self) -> VfsFileType {
        VfsFileType::Directory
    }
    fn ls(&self) -> Vec<(String, VfsFileType)> {
        alloc::vec![(String::from("net"), VfsFileType::Directory)]
    }
    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        match name {
            "net" => Some(Arc::new(SysNetClassNode) as Arc<dyn VfsNode>),
            _ => None,
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
            SYSFS_MAGIC,
            crate::config::PAGE_SIZE as u64,
            SYSFS_MAGIC,
            255,
        ))
    }
}

#[derive(Default, Debug)]
struct SysNetClassNode;

impl VfsNode for SysNetClassNode {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn file_type(&self) -> VfsFileType {
        VfsFileType::Directory
    }
    fn ls(&self) -> Vec<(String, VfsFileType)> {
        compat::list_ifaces()
            .into_iter()
            .map(|iface| (cstr_to_string(&iface.name), VfsFileType::Directory))
            .collect()
    }
    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        compat::get_iface_info(name)
            .map(|iface| Arc::new(SysNetIfaceNode { iface }) as Arc<dyn VfsNode>)
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
            SYSFS_MAGIC,
            crate::config::PAGE_SIZE as u64,
            SYSFS_MAGIC,
            255,
        ))
    }
}

#[derive(Debug)]
struct SysNetIfaceNode {
    iface: CompatNetIfInfo,
}

impl VfsNode for SysNetIfaceNode {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn file_type(&self) -> VfsFileType {
        VfsFileType::Directory
    }
    fn ls(&self) -> Vec<(String, VfsFileType)> {
        alloc::vec![
            (String::from("address"), VfsFileType::Regular),
            (String::from("mtu"), VfsFileType::Regular),
        ]
    }
    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        match name {
            "address" => Some(Arc::new(SysNetAttrNode::Address(self.iface)) as Arc<dyn VfsNode>),
            "mtu" => Some(Arc::new(SysNetAttrNode::Mtu(self.iface)) as Arc<dyn VfsNode>),
            _ => None,
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
            SYSFS_MAGIC,
            crate::config::PAGE_SIZE as u64,
            SYSFS_MAGIC,
            255,
        ))
    }
}

#[derive(Clone, Copy, Debug)]
enum SysNetAttrNode {
    Address(CompatNetIfInfo),
    Mtu(CompatNetIfInfo),
}

impl SysNetAttrNode {
    fn render(&self) -> String {
        match self {
            Self::Address(iface) => fmt_mac(iface.mac),
            Self::Mtu(iface) => {
                let mut out = String::new();
                let _ = writeln!(&mut out, "{}", iface.mtu);
                out
            }
        }
    }
}

impl VfsNode for SysNetAttrNode {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }
    fn size(&self) -> usize {
        self.render().len()
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
        read_string_at(self.render(), offset, buf)
    }
    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }
    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            SYSFS_MAGIC,
            crate::config::PAGE_SIZE as u64,
            SYSFS_MAGIC,
            255,
        ))
    }
}

fn cstr_to_string(bytes: &[u8]) -> String {
    let len = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from(core::str::from_utf8(&bytes[..len]).unwrap_or(""))
}
