use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};

use lazy_static::lazy_static;
use spin::Mutex;

use crate::vfs::{Inode, VfsNode};

/// 稳定内存 inode 的缓存键。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct InodeCacheKey {
    /// 底层文件系统实例标识。
    pub fs_id: u64,
    /// 文件系统内部 inode 编号。
    pub ino: u64,
}

lazy_static! {
    /// 全局 inode cache，仅保存弱引用以避免长期强持有 inode。
    static ref INODE_CACHE: Mutex<BTreeMap<InodeCacheKey, Weak<Inode>>> = Mutex::new(BTreeMap::new());
}

/// 按稳定键获取或创建内存 inode，保证同一文件对象复用同一个 `Arc<Inode>`。
pub(crate) fn get_or_create_inode(node: Arc<dyn VfsNode>) -> Arc<Inode> {
    let Some(key) = cache_key_of(node.as_ref()) else {
        return Inode::new_uncached(node);
    };

    let mut cache = INODE_CACHE.lock();
    if let Some(existing) = cache.get(&key) {
        if let Some(inode) = existing.upgrade() {
            return inode;
        }
        cache.remove(&key);
    }

    let inode = Inode::new_uncached(node);
    cache.insert(key, Arc::downgrade(&inode));
    // TODO：后续可在内存压力下增量清理失效的 weak 条目，避免目录扫描后键空间持续膨胀。
    // TODO：当前 key 仅包含 `(fs_id, ino)`；若后端会在旧 inode 仍存活时复用 inode 号，需要补充 generation 或显式失效协议。
    inode
}

/// 提取可进入 inode cache 的稳定键；返回 `None` 表示该节点暂不参与复用。
fn cache_key_of(node: &dyn VfsNode) -> Option<InodeCacheKey> {
    let fs_id = node.fs_id();
    let ino = node.ino();
    if fs_id == 0 || ino == 0 {
        None
    } else {
        Some(InodeCacheKey { fs_id, ino })
    }
}
