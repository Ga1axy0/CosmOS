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

/// Snapshot of the global inode cache state.
#[derive(Clone, Copy, Debug, Default)]
pub struct InodeCacheStats {
    /// Number of live stable in-memory inode entries.
    pub entries: usize,
    /// Number of queued CLOCK candidates, including duplicate keys.
    pub inactive_entries: usize,
    /// Entry-count threshold that starts eviction.
    pub high_watermark: usize,
    /// Entry-count threshold that stops eviction.
    pub low_watermark: usize,
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

/// Drop a cached inode by its stable key.
pub(crate) fn remove_cached_inode(fs_id: u64, ino: u64) {
    if fs_id == 0 || ino == 0 {
        return;
    }
    INODE_CACHE
        .lock()
        .table
        .remove(&InodeCacheKey { fs_id, ino });
}

/// Drop a cached inode corresponding to a backend node.
///
/// This is needed when a filesystem can reuse inode numbers after unlink/rmdir:
/// a newly allocated backend node must not resolve to an old in-memory inode
/// with stale file type state.
pub(crate) fn remove_cached_node(node: &dyn VfsNode) {
    if let Some(key) = cache_key_of(node) {
        remove_cached_inode(key.fs_id, key.ino);
    }
}

/// 当 inode cache 过大时，回收到低水位。
fn reclaim_inode_cache_if_needed() {
    let mut no_progress_rounds = 0usize;
    loop {
        let (need_reclaim, len, scan_budget) = {
            let cache = INODE_CACHE.lock();
            (
                cache.table.len() > cache.high_watermark,
                cache.table.len(),
                cache.inactive.len().max(cache.table.len()).max(1),
            )
        };
        if !need_reclaim {
            break;
        }
        // If every cached inode is still strongly referenced elsewhere (for
        // example by the dentry cache), reclaim may be unable to make progress.
        // Stop after a bounded scan instead of looping forever.
        if no_progress_rounds >= scan_budget.saturating_mul(2) {
            break;
        }
        if !reclaim_one_inode() {
            break;
        }
        let shrunk = {
            let cache = INODE_CACHE.lock();
            cache.table.len() < len
        };
        if shrunk {
            no_progress_rounds = 0;
        } else {
            no_progress_rounds += 1;
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

/// Return the current global inode-cache footprint and queue depths.
pub fn inode_cache_stats() -> InodeCacheStats {
    let cache = INODE_CACHE.lock();
    InodeCacheStats {
        entries: cache.table.len(),
        inactive_entries: cache.inactive.len(),
        high_watermark: cache.high_watermark,
        low_watermark: cache.low_watermark,
    }
}
