use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use bitflags::bitflags;
use core::cmp::{max, min};

use fs::Inode;
use lazy_static::lazy_static;

use crate::config::{MEMORY_END, PAGE_SIZE};
use crate::mm::{frame_alloc, FrameTracker};
use crate::sync::SpinNoIrqLock;

bitflags! {
    /// 单个缓存页的状态位。
    struct CachePageState: u8 {
        /// 页内数据已经可读。
        const UPTODATE = 1 << 0;
        /// 页内容已修改、尚未回写。
        const DIRTY = 1 << 1;
        /// 当前正在写回到底层文件。
        const WRITEBACK = 1 << 2;
        /// 当前正在从底层文件装入。
        const LOADING = 1 << 3;
        /// 当前正在从 page cache 中摘除。
        const EVICTING = 1 << 4;
    }
}

/// 单个文件缓存页。
pub struct CachePage {
    /// 文件内页号。
    index: u64,
    /// 反向指向所属 mapping，供全局回收器回收时定位。
    owner: Weak<SpinNoIrqLock<PageMapping>>,
    /// 真实缓存数据所在的物理页框。
    frame: FrameTracker,
    /// 当前页内多少字节对文件内容有效。
    valid_bytes: usize,
    /// 页当前状态。
    state: CachePageState,
    /// 被内核显式持有时禁止回收。
    pin_count: usize,
    /// 预留给后续 `mmap(MAP_SHARED)` 的映射计数。
    map_count: usize,
    /// 简化 CLOCK 回收用访问位。
    ref_bit: bool,
}

impl CachePage {
    /// 基于新页框创建一个缓存页。
    fn new(index: u64, owner: Weak<SpinNoIrqLock<PageMapping>>, frame: FrameTracker) -> Self {
        Self {
            index,
            owner,
            frame,
            valid_bytes: 0,
            state: CachePageState::empty(),
            pin_count: 0,
            map_count: 0,
            ref_bit: true,
        }
    }
}

/// 单个 inode 对应的一整份页缓存。
pub struct PageMapping {
    /// 反向指向宿主 inode，供回收和写回时找到底层文件。
    inode: Weak<Inode>,
    /// 当前文件缓存的所有页。
    pages: BTreeMap<u64, Arc<SpinNoIrqLock<CachePage>>>,
    /// page cache 视角下的文件长度。
    size: usize,
    /// 当前脏页数量。
    dirty_pages: usize,
}

impl PageMapping {
    /// 为指定 inode 创建新的 page mapping。
    fn new(inode: &Arc<Inode>, size: usize) -> Self {
        Self {
            inode: Arc::downgrade(inode),
            pages: BTreeMap::new(),
            size,
            dirty_pages: 0,
        }
    }
}

/// 全局 page cache 管理器。
pub struct PageCacheManager {
    /// 简化版 CLOCK 队列，允许存在重复和失效条目。
    inactive: VecDeque<Weak<SpinNoIrqLock<CachePage>>>,
    /// 当前缓存页总数。
    cached_pages: usize,
    /// 超过该阈值后开始回收。
    high_watermark: usize,
    /// 回收到该阈值后停止。
    low_watermark: usize,
}

impl PageCacheManager {
    /// 创建新的全局 page cache 管理器。
    fn new() -> Self {
        let total_pages = max(1, MEMORY_END / PAGE_SIZE);
        let high_watermark = max(128, total_pages / 16);
        let low_watermark = max(96, high_watermark * 3 / 4);
        Self {
            inactive: VecDeque::new(),
            cached_pages: 0,
            high_watermark,
            low_watermark,
        }
    }
}

lazy_static! {
    /// 全局 page cache 管理器。
    static ref PAGE_CACHE_MANAGER: SpinNoIrqLock<PageCacheManager> =
        SpinNoIrqLock::new(PageCacheManager::new());
}

/// 判断一个 inode 当前是否适合进入 page cache。
pub fn is_inode_page_cacheable(inode: &Arc<Inode>) -> bool {
    !inode.is_dir() && inode.fs_id() != 0 && inode.ino() != 0
}

/// 返回 page cache 视角下的文件长度。
pub fn cached_inode_size(inode: &Arc<Inode>) -> usize {
    if !is_inode_page_cacheable(inode) {
        return inode.size();
    }
    if let Some(mapping) = find_mapping(inode) {
        return mapping.lock().size;
    }
    inode.size()
}

/// 读取 inode 指定范围的数据，普通文件优先走 page cache。
pub fn read_inode(inode: &Arc<Inode>, offset: usize, buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    if !is_inode_page_cacheable(inode) {
        return inode.read_at(offset, buf);
    }

    let mapping = get_or_create_mapping(inode);
    let file_size = mapping.lock().size;
    if offset >= file_size {
        return 0;
    }

    let mut done = 0usize;
    let end = min(file_size, offset.saturating_add(buf.len()));
    while offset + done < end {
        let file_off = offset + done;
        let page_idx = file_page_index(file_off);
        let page_off = file_page_offset(file_off);
        let page = get_or_create_page(&mapping, page_idx);
        ensure_page_uptodate(&mapping, &page, page_idx);

        let page_guard = page.lock();
        let readable = min(page_guard.valid_bytes.saturating_sub(page_off), end - file_off);
        if readable == 0 {
            break;
        }
        let bytes = page_guard.frame.ppn.get_bytes_array();
        buf[done..done + readable].copy_from_slice(&bytes[page_off..page_off + readable]);
        done += readable;
    }
    done
}

/// 向 inode 写入数据，普通文件优先写入 page cache 并标脏。
pub fn write_inode(inode: &Arc<Inode>, offset: usize, buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    if !is_inode_page_cacheable(inode) {
        return inode.write_at(offset, buf);
    }

    let mapping = get_or_create_mapping(inode);
    let old_size = mapping.lock().size;
    let new_size = offset.saturating_add(buf.len());
    {
        let mut mapping_guard = mapping.lock();
        if mapping_guard.size < new_size {
            mapping_guard.size = new_size;
        }
    }

    let mut done = 0usize;
    while done < buf.len() {
        let file_off = offset + done;
        let page_idx = file_page_index(file_off);
        let page_off = file_page_offset(file_off);
        let writable = min(PAGE_SIZE - page_off, buf.len() - done);
        let page = get_or_create_page(&mapping, page_idx);

        let page_start = page_start(page_idx);
        let page_end_before = min(old_size, page_start + PAGE_SIZE);
        let need_load = page_start < old_size
            && (page_off != 0 || writable < PAGE_SIZE)
            && !page.lock().state.contains(CachePageState::UPTODATE);
        if need_load {
            ensure_page_uptodate(&mapping, &page, page_idx);
        }

        let mapping_size = mapping.lock().size;
        let expected_valid = page_valid_bytes_for_size(mapping_size, page_idx);
        let mut page_guard = page.lock();
        let bytes = page_guard.frame.ppn.get_bytes_array();
        bytes[page_off..page_off + writable].copy_from_slice(&buf[done..done + writable]);

        // 新建页或越过 EOF 的写入需要把未覆盖区域也视为有效零填充内容。
        if page_start >= old_size && !page_guard.state.contains(CachePageState::UPTODATE) {
            page_guard.state.insert(CachePageState::UPTODATE);
        }
        page_guard.valid_bytes = max(page_guard.valid_bytes, expected_valid);
        if page_guard.valid_bytes < page_end_before.saturating_sub(page_start) {
            page_guard.valid_bytes = page_end_before - page_start;
        }
        mark_page_dirty(&mapping, &mut page_guard);
        done += writable;
    }

    reclaim_if_needed();
    done
}

/// 将 inode 截断到 0，并丢弃对应的缓存页。
pub fn truncate_inode_zero(inode: &Arc<Inode>) {
    if is_inode_page_cacheable(inode) {
        invalidate_inode_cache(inode);
    }
    inode.clear();
    // TODO：后续接入 `mmap(MAP_SHARED)` 后，这里还需要同步失效用户态映射。
}

/// 将某个 inode 的脏页全部同步到底层文件。
pub fn sync_inode(inode: &Arc<Inode>) {
    if !is_inode_page_cacheable(inode) {
        return;
    }
    let Some(mapping) = find_mapping(inode) else {
        return;
    };
    sync_mapping(&mapping);
}

/// 返回指定页对应的 cache page；供后续 `mmap` 缺页路径复用。
pub fn get_cached_page(inode: &Arc<Inode>, page_idx: u64) -> Option<Arc<SpinNoIrqLock<CachePage>>> {
    if !is_inode_page_cacheable(inode) {
        return None;
    }
    let mapping = get_or_create_mapping(inode);
    let page = get_or_create_page(&mapping, page_idx);
    ensure_page_uptodate(&mapping, &page, page_idx);
    Some(page)
}

/// 失效某个 inode 对应的全部 page cache。
fn invalidate_inode_cache(inode: &Arc<Inode>) {
    let mut manager = PAGE_CACHE_MANAGER.lock();
    if let Some(mapping) = inode.take_page_cache_state::<SpinNoIrqLock<PageMapping>>() {
        let removed_pages = mapping.lock().pages.len();
        manager.cached_pages = manager.cached_pages.saturating_sub(removed_pages);
    }
}

/// 查找 inode 对应的 mapping；不存在时返回 `None`。
fn find_mapping(inode: &Arc<Inode>) -> Option<Arc<SpinNoIrqLock<PageMapping>>> {
    inode.page_cache_state::<SpinNoIrqLock<PageMapping>>()
}

/// 获取或创建 inode 对应的 page mapping。
fn get_or_create_mapping(inode: &Arc<Inode>) -> Arc<SpinNoIrqLock<PageMapping>> {
    let current_size = inode.size();
    if let Some(mapping) = inode.page_cache_state::<SpinNoIrqLock<PageMapping>>() {
        info!(
            "[page_cache] mapping hit: inode_ptr={:#x} fs_id={} ino={}",
            Arc::as_ptr(inode) as usize,
            inode.fs_id(),
            inode.ino()
        );
        {
            let mut mapping_guard = mapping.lock();
            if mapping_guard.size < current_size {
                mapping_guard.size = current_size;
            }
        }
        return mapping;
    }

    let mapping = Arc::new(SpinNoIrqLock::new(PageMapping::new(inode, current_size)));
    inode.set_page_cache_state(Arc::clone(&mapping));
    info!(
        "[page_cache] mapping miss: inode_ptr={:#x} fs_id={} ino={} size={}",
        Arc::as_ptr(inode) as usize,
        inode.fs_id(),
        inode.ino(),
        current_size
    );
    mapping
}

/// 获取或创建某个文件页对应的 cache page。
fn get_or_create_page(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    page_idx: u64,
) -> Arc<SpinNoIrqLock<CachePage>> {
    if let Some(page) = mapping.lock().pages.get(&page_idx).cloned() {
        info!("[page_cache] page hit: page_idx={}", page_idx);
        return page;
    }

    let frame = alloc_cache_frame();
    let page = Arc::new(SpinNoIrqLock::new(CachePage::new(
        page_idx,
        Arc::downgrade(mapping),
        frame,
    )));

    {
        let mut mapping_guard = mapping.lock();
        if let Some(existing) = mapping_guard.pages.get(&page_idx).cloned() {
            info!("[page_cache] page hit-after-race: page_idx={}", page_idx);
            return existing;
        }
        mapping_guard.pages.insert(page_idx, Arc::clone(&page));
    }

    let mut manager = PAGE_CACHE_MANAGER.lock();
    manager.cached_pages += 1;
    manager.inactive.push_back(Arc::downgrade(&page));
    info!(
        "[page_cache] page miss: page_idx={} cached_pages={}",
        page_idx,
        manager.cached_pages
    );
    drop(manager);
    page
}

/// 保证某个页已装入缓存。
fn ensure_page_uptodate(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    page: &Arc<SpinNoIrqLock<CachePage>>,
    page_idx: u64,
) {
    let mut should_load = false;
    {
        let mut page_guard = page.lock();
        page_guard.ref_bit = true;
        if page_guard.state.contains(CachePageState::UPTODATE) {
            return;
        }
        if !page_guard.state.contains(CachePageState::LOADING) {
            page_guard.state.insert(CachePageState::LOADING);
            page_guard.pin_count += 1;
            should_load = true;
        }
    }

    if !should_load {
        loop {
            let ready = {
                let page_guard = page.lock();
                page_guard.state.contains(CachePageState::UPTODATE)
                    || !page_guard.state.contains(CachePageState::LOADING)
            };
            if ready {
                return;
            }
            // TODO：后续可改成等待队列，避免并发 miss 时忙等。
            core::hint::spin_loop();
        }
    }

    let page_start_off = page_start(page_idx);
    let valid_bytes = {
        let mapping_guard = mapping.lock();
        page_valid_bytes_for_size(mapping_guard.size, page_idx)
    };
    let read_size = if valid_bytes == 0 {
        0
    } else {
        let inode = mapping.lock().inode.upgrade().expect("page cache inode disappeared");
        let page_guard = page.lock();
        let bytes = page_guard.frame.ppn.get_bytes_array();
        bytes.fill(0);
        info!(
            "[page_cache] load page: fs_id={} ino={} page_idx={} valid_bytes={}",
            inode.fs_id(),
            inode.ino(),
            page_idx,
            valid_bytes
        );
        inode.read_at(page_start_off, &mut bytes[..valid_bytes])
    };

    let mut page_guard = page.lock();
    page_guard.valid_bytes = max(valid_bytes, read_size);
    page_guard.state.remove(CachePageState::LOADING);
    page_guard.state.insert(CachePageState::UPTODATE);
    page_guard.pin_count = page_guard.pin_count.saturating_sub(1);
}

/// 将缓存页标记为脏页并更新 mapping 统计。
fn mark_page_dirty(mapping: &Arc<SpinNoIrqLock<PageMapping>>, page_guard: &mut CachePage) {
    page_guard.ref_bit = true;
    if page_guard.state.contains(CachePageState::DIRTY) {
        return;
    }
    page_guard.state.insert(CachePageState::DIRTY | CachePageState::UPTODATE);
    mapping.lock().dirty_pages += 1;
}

/// 同步单个 mapping 的全部脏页。
fn sync_mapping(mapping: &Arc<SpinNoIrqLock<PageMapping>>) {
    let pages: alloc::vec::Vec<_> = mapping.lock().pages.values().cloned().collect();
    for page in pages {
        flush_page(mapping, &page);
    }
}

/// 将单个脏页写回底层文件。
fn flush_page(mapping: &Arc<SpinNoIrqLock<PageMapping>>, page: &Arc<SpinNoIrqLock<CachePage>>) {
    let (page_idx, valid_bytes, owner_inode) = {
        let mut page_guard = page.lock();
        if !page_guard.state.contains(CachePageState::DIRTY)
            || page_guard.state.contains(CachePageState::WRITEBACK)
        {
            return;
        }
        page_guard.state.insert(CachePageState::WRITEBACK);
        page_guard.pin_count += 1;
        let inode = mapping.lock().inode.upgrade().expect("page cache inode disappeared");
        (page_guard.index, page_guard.valid_bytes, inode)
    };

    if valid_bytes != 0 {
        let page_guard = page.lock();
        let bytes = page_guard.frame.ppn.get_bytes_array();
        info!(
            "[page_cache] writeback page: page_idx={} valid_bytes={}",
            page_idx,
            valid_bytes
        );
        owner_inode.write_at(page_start(page_idx), &bytes[..valid_bytes]);
    }

    let mut page_guard = page.lock();
    if page_guard.state.contains(CachePageState::DIRTY) {
        page_guard.state.remove(CachePageState::DIRTY);
        let mut mapping_guard = mapping.lock();
        mapping_guard.dirty_pages = mapping_guard.dirty_pages.saturating_sub(1);
    }
    page_guard.state.remove(CachePageState::WRITEBACK);
    page_guard.pin_count = page_guard.pin_count.saturating_sub(1);
}

/// 若当前缓存压力过大，则回收到低水位。
fn reclaim_if_needed() {
    loop {
        let need_reclaim = {
            let manager = PAGE_CACHE_MANAGER.lock();
            manager.cached_pages > manager.high_watermark
        };
        if !need_reclaim {
            break;
        }
        if !reclaim_one() {
            break;
        }
    }
}

/// 尝试推进一轮缓存页回收。
///
/// 返回值语义：
/// - `true`：本轮已经处理了一个候选页，外层可继续尝试下一轮回收；
///   这并不保证一定真正释放了一页，也可能只是跳过、清理失效队列项
///   或先触发脏页回写。
/// - `false`：当前没有必要继续回收，或已经没有可扫描的候选页，外层应停止回收循环。
fn reclaim_one() -> bool {
    let candidate = {
        let mut manager = PAGE_CACHE_MANAGER.lock();
        if manager.cached_pages <= manager.low_watermark {
            return false;
        }
        manager.inactive.pop_front()
    };
    let Some(candidate) = candidate else {
        return false;
    };
    let Some(page) = candidate.upgrade() else {
        return true;
    };

    {
        let mut page_guard = page.lock();
        if page_guard.pin_count > 0
            || page_guard.map_count > 0
            || page_guard
                .state
                .intersects(CachePageState::LOADING | CachePageState::WRITEBACK | CachePageState::EVICTING)
        {
            PAGE_CACHE_MANAGER.lock().inactive.push_back(Arc::downgrade(&page));
            return true;
        }
        if page_guard.ref_bit {
            page_guard.ref_bit = false;
            PAGE_CACHE_MANAGER.lock().inactive.push_back(Arc::downgrade(&page));
            return true;
        }
        if page_guard.state.contains(CachePageState::DIRTY) {
            drop(page_guard);
            if let Some(mapping) = page.lock().owner.upgrade() {
                flush_page(&mapping, &page);
            }
            PAGE_CACHE_MANAGER.lock().inactive.push_back(Arc::downgrade(&page));
            return true;
        }
        page_guard.state.insert(CachePageState::EVICTING);
    }

    let Some(mapping) = page.lock().owner.upgrade() else {
        let mut manager = PAGE_CACHE_MANAGER.lock();
        manager.cached_pages = manager.cached_pages.saturating_sub(1);
        return true;
    };
    let page_idx = page.lock().index;
    let removed = {
        let mut mapping_guard = mapping.lock();
        if let Some(existing) = mapping_guard.pages.get(&page_idx) {
            if Arc::ptr_eq(existing, &page) {
                mapping_guard.pages.remove(&page_idx)
            } else {
                None
            }
        } else {
            None
        }
    };

    if removed.is_some() {
        let mut manager = PAGE_CACHE_MANAGER.lock();
        manager.cached_pages = manager.cached_pages.saturating_sub(1);
        info!(
            "[page_cache] evict page: page_idx={} cached_pages={}",
            page_idx,
            manager.cached_pages
        );
    } else {
        page.lock().state.remove(CachePageState::EVICTING);
        PAGE_CACHE_MANAGER.lock().inactive.push_back(Arc::downgrade(&page));
    }
    true
}

/// 分配 page cache 使用的页框；内存紧张时先尝试回收一轮。
fn alloc_cache_frame() -> FrameTracker {
    if let Some(frame) = frame_alloc() {
        return frame;
    }
    reclaim_if_needed();
    // TODO：后续可继续接入更积极的回收/等待策略；当前先在彻底无页时直接报错。
    frame_alloc().expect("page cache out of memory")
}

/// 计算文件偏移所属页号。
fn file_page_index(offset: usize) -> u64 {
    (offset / PAGE_SIZE) as u64
}

/// 计算文件偏移在页内的位置。
fn file_page_offset(offset: usize) -> usize {
    offset % PAGE_SIZE
}

/// 计算某页在文件中的起始偏移。
fn page_start(page_idx: u64) -> usize {
    page_idx as usize * PAGE_SIZE
}

/// 根据当前文件大小计算某一页理论上有多少有效字节。
fn page_valid_bytes_for_size(size: usize, page_idx: u64) -> usize {
    let start = page_start(page_idx);
    size.saturating_sub(start).min(PAGE_SIZE)
}
