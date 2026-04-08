use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;

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

/// 单个 inode cache 条目。
struct InodeCacheEntry {
    /// 被 cache 强持有的稳定内存 inode。
    inode: Arc<Inode>,
    /// 简化 CLOCK 回收用访问位。
    ref_bit: bool,
}

/// 全局 inode cache 管理器。
struct InodeCacheManager {
    /// 稳定键到 inode 条目的映射。
    table: BTreeMap<InodeCacheKey, InodeCacheEntry>,
    /// 简化版 CLOCK 队列，允许存在重复键。
    inactive: VecDeque<InodeCacheKey>,
    /// 超过该阈值后开始回收。
    high_watermark: usize,
    /// 回收到该阈值后停止。
    low_watermark: usize,
}

impl InodeCacheManager {
    /// 创建新的 inode cache 管理器。
    fn new() -> Self {
        Self {
            table: BTreeMap::new(),
            inactive: VecDeque::new(),
            high_watermark: 256,
            low_watermark: 192,
        }
    }
}

lazy_static! {
    /// 全局 inode cache，强持有近期使用过的稳定内存 inode。
    static ref INODE_CACHE: Mutex<InodeCacheManager> = Mutex::new(InodeCacheManager::new());
}

/// 按稳定键获取或创建内存 inode，保证同一文件对象复用同一个 `Arc<Inode>`。
pub(crate) fn get_or_create_inode(node: Arc<dyn VfsNode>) -> Arc<Inode> {
    let Some(key) = cache_key_of(node.as_ref()) else {
        return Inode::new_uncached(node);
    };

    {
        let mut cache = INODE_CACHE.lock();
        if let Some(entry) = cache.table.get_mut(&key) {
            entry.ref_bit = true;
            return Arc::clone(&entry.inode);
        }
    }

    let inode = Inode::new_uncached(node);
    {
        let mut cache = INODE_CACHE.lock();
        if let Some(entry) = cache.table.get_mut(&key) {
            entry.ref_bit = true;
            return Arc::clone(&entry.inode);
        }
        cache.table.insert(
            key,
            InodeCacheEntry {
                inode: Arc::clone(&inode),
                ref_bit: true,
            },
        );
        cache.inactive.push_back(key);
    }
    reclaim_inode_cache_if_needed();
    // TODO：后续可把当前按条目数的阈值回收，升级成结合内存压力的 shrinker。
    // TODO：当前 key 仅包含 `(fs_id, ino)`；若后端会在旧 inode 仍存活时复用 inode 号，需要补充 generation 或显式失效协议。
    inode
}

/// 当 inode cache 过大时，回收到低水位。
fn reclaim_inode_cache_if_needed() {
    loop {
        let need_reclaim = {
            let cache = INODE_CACHE.lock();
            cache.table.len() > cache.high_watermark
        };
        if !need_reclaim {
            break;
        }
        if !reclaim_one_inode() {
            break;
        }
    }
}

/// 尝试回收一个仅被 cache 持有的 inode。
fn reclaim_one_inode() -> bool {
    let key = {
        let mut cache = INODE_CACHE.lock();
        if cache.table.len() <= cache.low_watermark {
            return false;
        }
        cache.inactive.pop_front()
    };
    let Some(key) = key else {
        return false;
    };

    let mut cache = INODE_CACHE.lock();
    let Some(entry) = cache.table.get_mut(&key) else {
        return true;
    };

    // 仅当 cache 自己是唯一持有者时，才允许回收这个 inode。
    if Arc::strong_count(&entry.inode) > 1 {
        entry.ref_bit = true;
        cache.inactive.push_back(key);
        return true;
    }
    if entry.ref_bit {
        entry.ref_bit = false;
        cache.inactive.push_back(key);
        return true;
    }

    cache.table.remove(&key);
    true
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
