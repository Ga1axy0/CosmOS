use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::sync::{Arc, Weak};
use bitflags::bitflags;
use core::cmp::{max, min};

use fs::errno::FS_ERRNO;
use fs::Inode;
use lazy_static::lazy_static;

use crate::config::{MEMORY_END, PAGE_SIZE};
use crate::mm::{
    frame_alloc, invalidate_inode_mappings_after_truncate, FrameTracker, PhysPageNum,
};
use crate::sync::SpinNoIrqLock;
use crate::task::{WaitQueue, WaitReason};

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
    /// 并发装页/回写时的等待队列。
    wait_queue: Arc<WaitQueue>,
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
            wait_queue: Arc::new(WaitQueue::new()),
        }
    }

    /// 返回当前缓存页对应的物理页号。
    pub fn ppn(&self) -> PhysPageNum {
        self.frame.ppn
    }

    /// 返回当前页内多少字节已经对应有效文件内容。
    #[allow(dead_code)]
    pub fn valid_bytes(&self) -> usize {
        self.valid_bytes
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
    /// 当前所有脏页的文件内页号集合。
    dirty_pages: BTreeSet<u64>,
}

impl PageMapping {
    /// 为指定 inode 创建新的 page mapping。
    fn new(inode: &Arc<Inode>, size: usize) -> Self {
        Self {
            inode: Arc::downgrade(inode),
            pages: BTreeMap::new(),
            size,
            dirty_pages: BTreeSet::new(),
        }
    }
}

/// inode 对应 page mapping 的稳定句柄。
#[derive(Clone)]
pub struct PageMappingHandle {
    /// 指向底层 page mapping 的共享引用。
    inner: Arc<SpinNoIrqLock<PageMapping>>,
}

impl PageMappingHandle {
    /// 基于底层 mapping 构造句柄。
    fn new(inner: Arc<SpinNoIrqLock<PageMapping>>) -> Self {
        Self { inner }
    }

    /// 返回 page cache 视角下的当前文件长度。
    pub fn size(&self) -> usize {
        self.inner.lock().size
    }

    /// 读取指定范围的数据，必要时装入缺失缓存页。
    pub fn read(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_mapping(&self.inner, offset, buf)
    }

    /// 写入指定范围的数据，并把涉及页标记为脏页。
    pub fn write(&self, offset: usize, buf: &[u8]) -> usize {
        write_mapping(&self.inner, offset, buf)
    }

    /// 将当前 mapping 中的全部脏页同步到底层文件。
    pub fn sync(&self) {
        sync_mapping(&self.inner);
    }

    /// 调整当前文件长度，并同步更新已有缓存页。
    pub fn truncate(&self, new_size: usize) {
        truncate_mapping(&self.inner, new_size);
    }

    /// 获取指定文件页号对应的缓存页，供后续 `mmap` 缺页路径复用。
    #[allow(dead_code)]
    pub fn get_page(&self, page_idx: u64) -> Arc<SpinNoIrqLock<CachePage>> {
        get_or_load_page(&self.inner, page_idx)
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
    if let Some(mapping) = try_get_mapping(inode) {
        return mapping.size();
    }
    inode.size()
}

/// 获取当前 inode 对应的稳定 page mapping 句柄。
pub fn mapping_for_inode(inode: &Arc<Inode>) -> Option<PageMappingHandle> {
    if !is_inode_page_cacheable(inode) {
        return None;
    }
    Some(PageMappingHandle::new(get_or_create_mapping(inode)))
}

/// 读取 inode 指定范围的数据，普通文件优先走 page cache。
#[allow(dead_code)]
pub fn read_inode(inode: &Arc<Inode>, offset: usize, buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    if !is_inode_page_cacheable(inode) {
        return inode.read_at(offset, buf);
    }
    mapping_for_inode(inode)
        .expect("cacheable inode must have page mapping")
        .read(offset, buf)
}

/// 向 inode 写入数据，普通文件优先写入 page cache 并标脏。
#[allow(dead_code)]
pub fn write_inode(inode: &Arc<Inode>, offset: usize, buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    if !is_inode_page_cacheable(inode) {
        return inode.write_at(offset, buf);
    }
    mapping_for_inode(inode)
        .expect("cacheable inode must have page mapping")
        .write(offset, buf)
}

/// 调整 inode 逻辑长度，并同步更新对应 page cache。
pub fn truncate_inode(inode: &Arc<Inode>, new_size: usize) -> Result<(), FS_ERRNO> {
    debug!(
        "[page_cache] truncate inode: fs_id={} ino={} old_size={} new_size={}",
        inode.fs_id(),
        inode.ino(),
        cached_inode_size(inode),
        new_size
    );
    if let Some(mapping) = mapping_for_inode(inode) {
        if new_size < mapping.size() {
            invalidate_inode_mappings_after_truncate(inode, new_size);
        }
        mapping.truncate(new_size);
        return Ok(());
    }
    inode.truncate(new_size)?;
    Ok(())
}

/// 将某个 inode 的脏页全部同步到底层文件。
#[allow(dead_code)]
pub fn sync_inode(inode: &Arc<Inode>) {
    if !is_inode_page_cacheable(inode) {
        return;
    }
    let Some(mapping) = try_get_mapping(inode) else {
        return;
    };
    mapping.sync();
}

/// 返回指定页对应的 cache page；供后续 `mmap` 缺页路径复用。
#[allow(dead_code)]
pub fn get_cached_page(inode: &Arc<Inode>, page_idx: u64) -> Option<Arc<SpinNoIrqLock<CachePage>>> {
    mapping_for_inode(inode).map(|mapping| mapping.get_page(page_idx))
}

/// 增加某个缓存页的共享映射计数，防止其在仍被用户页表引用时被回收。
pub fn retain_mapped_page(page: &Arc<SpinNoIrqLock<CachePage>>) {
    let mut page_guard = page.lock();
    page_guard.ref_bit = true;
    page_guard.map_count += 1;
}

/// 减少某个缓存页的共享映射计数。
pub fn release_mapped_page(page: &Arc<SpinNoIrqLock<CachePage>>) {
    let mut page_guard = page.lock();
    page_guard.map_count = page_guard.map_count.saturating_sub(1);
}

/// 将一个已经通过共享映射暴露给用户态的缓存页标记为脏页。
pub fn mark_cached_page_dirty(page: &Arc<SpinNoIrqLock<CachePage>>) {
    let owner = {
        let page_guard = page.lock();
        page_guard.owner.upgrade()
    };
    let Some(owner) = owner else {
        return;
    };
    let mut page_guard = page.lock();
    mark_page_dirty(&owner, &mut page_guard);
}

/// 查找 inode 对应的 mapping；不存在时返回 `None`。
fn try_get_mapping(inode: &Arc<Inode>) -> Option<PageMappingHandle> {
    inode
        .page_cache_state::<SpinNoIrqLock<PageMapping>>()
        .map(PageMappingHandle::new)
}

/// 获取或创建 inode 对应的 page mapping。
fn get_or_create_mapping(inode: &Arc<Inode>) -> Arc<SpinNoIrqLock<PageMapping>> {
    let current_size = inode.size();
    let (mapping, inserted) = inode.get_or_insert_page_cache_state(|| {
        Arc::new(SpinNoIrqLock::new(PageMapping::new(inode, current_size)))
    });
    {
        let mut mapping_guard = mapping.lock();
        if mapping_guard.size < current_size {
            mapping_guard.size = current_size;
        }
    }
    if inserted {
        debug!(
            "[page_cache] mapping miss: inode_ptr={:#x} fs_id={} ino={} size={}",
            Arc::as_ptr(inode) as usize,
            inode.fs_id(),
            inode.ino(),
            current_size
        );
    } else {
        debug!(
            "[page_cache] mapping hit: inode_ptr={:#x} fs_id={} ino={}",
            Arc::as_ptr(inode) as usize,
            inode.fs_id(),
            inode.ino()
        );
    }
    mapping
}

/// 读取单个 mapping 中指定范围的数据。
fn read_mapping(mapping: &Arc<SpinNoIrqLock<PageMapping>>, offset: usize, buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }

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
        let page = get_or_load_page(mapping, page_idx);

        let page_guard = page.lock();
        let readable = min(page_guard.valid_bytes.saturating_sub(page_off), end - file_off);
        if readable == 0 {
            break;
        }
        let bytes = page_guard.ppn().get_bytes_array();
        buf[done..done + readable].copy_from_slice(&bytes[page_off..page_off + readable]);
        done += readable;
    }
    done
}

/// 向单个 mapping 中写入指定范围的数据，并标脏涉及页。
fn write_mapping(mapping: &Arc<SpinNoIrqLock<PageMapping>>, offset: usize, buf: &[u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }

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
        let page = get_or_create_page(mapping, page_idx);

        let page_start = page_start(page_idx);
        let page_end_before = min(old_size, page_start + PAGE_SIZE);
        let need_load = page_start < old_size
            && (page_off != 0 || writable < PAGE_SIZE)
            && !page.lock().state.contains(CachePageState::UPTODATE);
        if need_load {
            ensure_page_uptodate(mapping, &page, page_idx);
        }

        let mapping_size = mapping.lock().size;
        let expected_valid = page_valid_bytes_for_size(mapping_size, page_idx);
        let mut page_guard = page.lock();
        let bytes = page_guard.ppn().get_bytes_array();
        bytes[page_off..page_off + writable].copy_from_slice(&buf[done..done + writable]);

        if page_start >= old_size && !page_guard.state.contains(CachePageState::UPTODATE) {
            page_guard.state.insert(CachePageState::UPTODATE);
        }
        page_guard.valid_bytes = max(page_guard.valid_bytes, expected_valid);
        if page_guard.valid_bytes < page_end_before.saturating_sub(page_start) {
            page_guard.valid_bytes = page_end_before - page_start;
        }
        mark_page_dirty(mapping, &mut page_guard);
        done += writable;
    }

    reclaim_if_needed();
    done
}

/// 调整当前 mapping 长度，并同步更新已有缓存页。
fn truncate_mapping(mapping: &Arc<SpinNoIrqLock<PageMapping>>, new_size: usize) {
    let inode = {
        let mapping_guard = mapping.lock();
        mapping_guard
            .inode
            .upgrade()
            .expect("page cache inode disappeared")
    };

    let old_size = mapping.lock().size;
    if old_size == new_size {
        debug!(
            "[page_cache] truncate skip: old_size == new_size == {}",
            new_size
        );
        return;
    }

    debug!(
        "[page_cache] truncate mapping: old_size={} new_size={}",
        old_size,
        new_size
    );

    // 这里底层 inode truncate 失败时直接 panic，避免 page cache 与底层长度分离。
    if inode.truncate(new_size).is_err() {
        panic!("page cache truncate must stay consistent with backing inode");
    }

    let old_last_valid = old_size.saturating_sub(1);
    let new_last_valid = new_size.saturating_sub(1);
    let old_tail_idx = file_page_index(old_last_valid);
    let new_tail_idx = file_page_index(new_last_valid);
    let new_tail_valid = if new_size == 0 {
        0
    } else {
        page_valid_bytes_for_size(new_size, new_tail_idx)
    };

    // 对于truncate缩小的情况，被丢弃的页即使是脏的也无需写回，最终总是要被丢弃的。
    let removed_pages = {
        let mut mapping_guard = mapping.lock();
        mapping_guard.size = new_size;

        if new_size < old_size {
            let first_removed_idx = if new_size == 0 {
                0
            } else if new_tail_valid == PAGE_SIZE {
                new_tail_idx.saturating_add(1)
            } else {
                new_tail_idx.saturating_add(1)
            };
            let removed_indices: alloc::vec::Vec<_> = mapping_guard
                .pages
                .range(first_removed_idx..)
                .map(|(&idx, _)| idx)
                .collect();
            let removed_cnt = removed_indices.len();
            debug!(
                "[page_cache] truncate shrink: first_removed_idx={} removed_pages={}",
                first_removed_idx,
                removed_cnt
            );
            for page_idx in removed_indices {
                mapping_guard.pages.remove(&page_idx);
                mapping_guard.dirty_pages.remove(&page_idx);
            }
            if new_size == 0 {
                mapping_guard.dirty_pages.clear();
            }
            removed_cnt
        } else {
            0
        }
    };
    {
        let mut manager = PAGE_CACHE_MANAGER.lock();
        manager.cached_pages = manager.cached_pages.saturating_sub(removed_pages);
    }

    if new_size < old_size && new_size > 0 {
        let tail_page = mapping.lock().pages.get(&new_tail_idx).cloned();
        if let Some(tail_page) = tail_page {
            let mut page_guard = tail_page.lock();
            let bytes = page_guard.ppn().get_bytes_array();
            debug!(
                "[page_cache] truncate shrink tail: page_idx={} keep_bytes={}",
                new_tail_idx,
                new_tail_valid
            );
            bytes[new_tail_valid..].fill(0);
            page_guard.valid_bytes = min(page_guard.valid_bytes, new_tail_valid);
            page_guard.state.insert(CachePageState::UPTODATE);
        }
    }

    if new_size > old_size && old_size > 0 {
        let old_tail_valid = page_valid_bytes_for_size(old_size, old_tail_idx);
        if old_tail_valid < PAGE_SIZE {
            let tail_page = mapping.lock().pages.get(&old_tail_idx).cloned();
            if let Some(tail_page) = tail_page {
                let mut page_guard = tail_page.lock();
                let new_valid = page_valid_bytes_for_size(new_size, old_tail_idx);
                if new_valid > old_tail_valid {
                    let bytes = page_guard.ppn().get_bytes_array();
                    debug!(
                        "[page_cache] truncate grow tail: page_idx={} zero_range=[{}..{})",
                        old_tail_idx,
                        old_tail_valid,
                        new_valid
                    );
                    bytes[old_tail_valid..new_valid].fill(0);
                    page_guard.valid_bytes = max(page_guard.valid_bytes, new_valid);
                    page_guard.state.insert(CachePageState::UPTODATE);
                }
            }
        }
    }
}

/// 获取并确保装入某个文件页对应的 cache page。
fn get_or_load_page(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    page_idx: u64,
) -> Arc<SpinNoIrqLock<CachePage>> {
    let page = get_or_create_page(mapping, page_idx);
    ensure_page_uptodate(mapping, &page, page_idx);
    page
}

/// 获取或创建某个文件页对应的 cache page。
fn get_or_create_page(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    page_idx: u64,
) -> Arc<SpinNoIrqLock<CachePage>> {
    if let Some(page) = mapping.lock().pages.get(&page_idx).cloned() {
        // debug!("[page_cache] page hit: page_idx={}", page_idx);
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
            debug!("[page_cache] page hit-after-race: page_idx={}", page_idx);
            return existing;
        }
        mapping_guard.pages.insert(page_idx, Arc::clone(&page));
    }

    let mut manager = PAGE_CACHE_MANAGER.lock();
    manager.cached_pages += 1;
    manager.inactive.push_back(Arc::downgrade(&page));
    // debug!(
    //     "[page_cache] page miss: page_idx={} cached_pages={}",
    //     page_idx,
    //     manager.cached_pages
    // );
    drop(manager);
    page
}

/// 保证某个页已装入缓存。
fn ensure_page_uptodate(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    page: &Arc<SpinNoIrqLock<CachePage>>,
    page_idx: u64,
) {
    loop {
        let (wait_queue, loader_ppn) = {
            let mut page_guard = page.lock();
            page_guard.ref_bit = true;
            if page_guard.state.contains(CachePageState::UPTODATE) {
                return;
            }
            if page_guard.state.contains(CachePageState::LOADING) {
                (Some(Arc::clone(&page_guard.wait_queue)), None)
            } else {
                page_guard.state.insert(CachePageState::LOADING);
                page_guard.pin_count += 1;
                (None, Some(page_guard.ppn()))
            }
        };

        if let Some(wait_queue) = wait_queue {
            wait_queue.wait_with_reason_or_skip(WaitReason::BlockDeviceIo, || {
                let page_guard = page.lock();
                page_guard.state.contains(CachePageState::UPTODATE)
                    || !page_guard.state.contains(CachePageState::LOADING)
            });
            continue;
        }

        let ppn = loader_ppn.expect("page loader must own a target frame");
        let page_start_off = page_start(page_idx);
        let valid_bytes = {
            let mapping_guard = mapping.lock();
            page_valid_bytes_for_size(mapping_guard.size, page_idx)
        };
        let inode = {
            let mapping_guard = mapping.lock();
            mapping_guard
                .inode
                .upgrade()
                .expect("page cache inode disappeared")
        };
        let read_size = if valid_bytes == 0 {
            0
        } else {
            let bytes = ppn.get_bytes_array();
            bytes.fill(0);
            // debug!(
            //     "[page_cache] load page: fs_id={} ino={} page_idx={} valid_bytes={}",
            //     inode.fs_id(),
            //     inode.ino(),
            //     page_idx,
            //     valid_bytes
            // );
            // TODO：后续接入通用 truncate 后，需要避免装页与截断并发时把旧数据重新提交回 cache。
            inode.read_at(page_start_off, &mut bytes[..valid_bytes])
        };

        let wait_queue = {
            let mut page_guard = page.lock();
            page_guard.valid_bytes = max(valid_bytes, read_size);
            page_guard.state.remove(CachePageState::LOADING);
            page_guard.state.insert(CachePageState::UPTODATE);
            page_guard.pin_count = page_guard.pin_count.saturating_sub(1);
            Arc::clone(&page_guard.wait_queue)
        };
        wait_queue.wake_all();
        return;
    }
}

/// 将缓存页标记为脏页并更新 mapping 统计。
fn mark_page_dirty(mapping: &Arc<SpinNoIrqLock<PageMapping>>, page_guard: &mut CachePage) {
    page_guard.ref_bit = true;
    if page_guard.state.contains(CachePageState::DIRTY) {
        return;
    }
    page_guard.state.insert(CachePageState::DIRTY | CachePageState::UPTODATE);
    mapping.lock().dirty_pages.insert(page_guard.index);
}

/// 同步单个 mapping 的全部脏页。
fn sync_mapping(mapping: &Arc<SpinNoIrqLock<PageMapping>>) {
    let dirty_pages: alloc::vec::Vec<_> = mapping.lock().dirty_pages.iter().copied().collect();
    for page_idx in dirty_pages {
        let page = mapping.lock().pages.get(&page_idx).cloned();
        if let Some(page) = page {
            flush_page(mapping, &page);
        }
    }
}

/// 将单个脏页写回底层文件。
fn flush_page(mapping: &Arc<SpinNoIrqLock<PageMapping>>, page: &Arc<SpinNoIrqLock<CachePage>>) {
    loop {
        let (wait_queue, writeback_info) = {
            let mut page_guard = page.lock();
            if page_guard.state.contains(CachePageState::WRITEBACK) {
                (Some(Arc::clone(&page_guard.wait_queue)), None)
            } else if !page_guard.state.contains(CachePageState::DIRTY) {
                return;
            } else {
                page_guard.state.insert(CachePageState::WRITEBACK);
                page_guard.pin_count += 1;
                let inode = {
                    let mapping_guard = mapping.lock();
                    mapping_guard
                        .inode
                        .upgrade()
                        .expect("page cache inode disappeared")
                };
                (
                    None,
                    Some((page_guard.index, page_guard.valid_bytes, page_guard.ppn(), inode)),
                )
            }
        };

        if let Some(wait_queue) = wait_queue {
            wait_queue.wait_with_reason_or_skip(WaitReason::BlockDeviceIo, || {
                let page_guard = page.lock();
                !page_guard.state.contains(CachePageState::WRITEBACK)
            });
            continue;
        }

        let (page_idx, valid_bytes, ppn, owner_inode) =
            writeback_info.expect("writeback owner must provide page data");
        if valid_bytes != 0 {
            let bytes = ppn.get_bytes_array();
            debug!(
                "[page_cache] writeback page: page_idx={} valid_bytes={}",
                page_idx,
                valid_bytes
            );
            owner_inode.write_at(page_start(page_idx), &bytes[..valid_bytes]);
        }

        let wait_queue = {
            let mut page_guard = page.lock();
            if page_guard.state.contains(CachePageState::DIRTY) {
                if page_guard.map_count > 0 {
                    // 共享映射仍然存在时先保守地维持脏状态，避免写回后后续写入无法再次通知内核。
                    // TODO：后续补齐反向映射后，可在写回前清 PTE 脏位并重新写保护，从而精确清脏。
                    mapping.lock().dirty_pages.insert(page_idx);
                } else {
                    page_guard.state.remove(CachePageState::DIRTY);
                    mapping.lock().dirty_pages.remove(&page_idx);
                }
            }
            page_guard.state.remove(CachePageState::WRITEBACK);
            page_guard.pin_count = page_guard.pin_count.saturating_sub(1);
            Arc::clone(&page_guard.wait_queue)
        };
        wait_queue.wake_all();
        return;
    }
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
        debug!(
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
    if let Some(frame) = frame_alloc() {
        return frame;
    }
    // 分配失败时再主动推进回收，避免 cache 尚未超过高水位时错过可回收页。
    while reclaim_one() {
        if let Some(frame) = frame_alloc() {
            return frame;
        }
    }
    // TODO：后续可继续接入更积极的回收/等待策略；当前在无可回收页且彻底无页时直接报错。
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
