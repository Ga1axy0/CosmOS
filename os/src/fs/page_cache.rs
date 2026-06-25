use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use bitflags::bitflags;
use core::cmp::{max, min};
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

use fs::errno::FS_ERRNO;
use fs::Inode;
use lazy_static::lazy_static;

use crate::bootinfo;
use crate::config::PAGE_SIZE;
use crate::hal::hartid;
use crate::mm::{
    frame_alloc, frame_allocator_stats, invalidate_inode_mappings_after_truncate, FrameTracker,
    InodeKey, MmError, PhysPageNum,
};
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{WaitQueue, WaitReason};

#[cfg(feature = "io_perf_counters")]
static READ_PAGE_LOADS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static READ_PAGE_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_MAPPING_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_MAPPING_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static SYNC_MAPPING_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static SYNC_RANGE_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITEBACK_PAGES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITEBACK_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITEBACK_BATCHES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITEBACK_BATCH_PAGES: AtomicUsize = AtomicUsize::new(0);

const MAX_WRITEBACK_BATCH_PAGES: usize = 32;

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
        let _ = sync_mapping(&self.inner);
    }

    /// 调整当前文件长度，并同步更新已有缓存页。
    pub fn truncate(&self, new_size: usize) -> Result<(), FS_ERRNO> {
        truncate_mapping(&self.inner, new_size)
    }

    /// Reserve file space without forcing eager page creation.
    pub fn fallocate(&self, mode: i32, offset: usize, len: usize) -> Result<(), FS_ERRNO> {
        fallocate_mapping(&self.inner, mode, offset, len)
    }

    /// 获取指定文件页号对应的缓存页，供后续 `mmap` 缺页路径复用。
    #[allow(dead_code)]
    pub fn get_page(&self, page_idx: u64) -> Arc<SpinNoIrqLock<CachePage>> {
        self.try_get_page(page_idx)
            .expect("page cache OOM in non-fallible get_page path")
    }

    /// 获取指定文件页号对应的缓存页，必要时装入；OOM 时返回 `MmError`。
    pub fn try_get_page(&self, page_idx: u64) -> Result<Arc<SpinNoIrqLock<CachePage>>, MmError> {
        get_or_load_page(&self.inner, page_idx)
    }
}

/// 全局 page cache 管理器。
pub struct PageCacheManager {
    /// 简化版 CLOCK 队列，允许存在重复和失效条目。
    inactive: VecDeque<Weak<SpinNoIrqLock<CachePage>>>,
    /// 已创建过 page mapping 的 inode 索引，供全局/按文件系统同步使用。
    mappings: BTreeMap<InodeKey, Weak<SpinNoIrqLock<PageMapping>>>,
    /// 当前缓存页总数。
    pub cached_pages: usize,
    /// 超过该阈值后开始回收。
    pub high_watermark: usize,
    /// 回收到该阈值后停止。
    pub low_watermark: usize,
}

impl PageCacheManager {
    /// 创建新的全局 page cache 管理器。
    fn new() -> Self {
        let (high_watermark, low_watermark) = default_watermarks();
        Self {
            inactive: VecDeque::new(),
            mappings: BTreeMap::new(),
            cached_pages: 0,
            high_watermark,
            low_watermark,
        }
    }
}

lazy_static! {
    /// 全局 page cache 管理器。
    pub static ref PAGE_CACHE_MANAGER: SpinNoIrqLock<PageCacheManager> =
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

/// Like [`cached_inode_size`], but uses `fs_size` as the fallback instead of
/// calling `inode.size()` again.  Useful when the caller already has a
/// batched-read size and wants to avoid a redundant lock acquisition.
pub fn cached_inode_size_fast(inode: &Arc<Inode>, fs_size: usize) -> usize {
    if !is_inode_page_cacheable(inode) {
        return fs_size;
    }
    if let Some(mapping) = try_get_mapping(inode) {
        return mapping.size();
    }
    fs_size
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
        return mapping.truncate(new_size);
    }
    if let Err(err) = inode.truncate(new_size) {
        error!(
            "[page_cache] truncate backing inode failed: fs_id={} ino={} new_size={} errno={}",
            inode.fs_id(),
            inode.ino(),
            new_size,
            err as i32
        );
        return Err(err);
    }
    Ok(())
}

/// Reserve file space without forcing eager data allocation, while keeping the
/// page-cache view of the inode length in sync.
pub fn fallocate_inode(
    inode: &Arc<Inode>,
    mode: i32,
    offset: usize,
    len: usize,
) -> Result<(), FS_ERRNO> {
    let new_size = offset.checked_add(len).ok_or(FS_ERRNO::EINVAL)?;
    debug!(
        "[page_cache] fallocate inode: fs_id={} ino={} mode={:#x} offset={} len={} old_size={} new_size={}",
        inode.fs_id(),
        inode.ino(),
        mode,
        offset,
        len,
        cached_inode_size(inode),
        new_size
    );
    if let Some(mapping) = mapping_for_inode(inode) {
        mapping.fallocate(mode, offset, len)?;
        return Ok(());
    }
    inode.fallocate(mode, offset, len)?;
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

/// 同步当前文件系统上所有 page-cache-backed inode 的脏页。
pub fn sync_fs(fs_id: u64) -> Result<(), ERRNO> {
    for mapping in collect_mappings(Some(fs_id)) {
        sync_mapping(&mapping)?;
    }
    Ok(())
}

/// 同步全局所有 page cache 脏页。
pub fn sync_all() -> Result<(), ERRNO> {
    for mapping in collect_mappings(None) {
        sync_mapping(&mapping)?;
    }
    Ok(())
}

fn perf_load(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::Relaxed)
}

#[cfg(feature = "io_perf_counters")]
pub fn reset_perf_counters() {
    READ_PAGE_LOADS.store(0, Ordering::Relaxed);
    READ_PAGE_BYTES.store(0, Ordering::Relaxed);
    WRITE_MAPPING_CALLS.store(0, Ordering::Relaxed);
    WRITE_MAPPING_BYTES.store(0, Ordering::Relaxed);
    SYNC_MAPPING_CALLS.store(0, Ordering::Relaxed);
    SYNC_RANGE_CALLS.store(0, Ordering::Relaxed);
    WRITEBACK_PAGES.store(0, Ordering::Relaxed);
    WRITEBACK_BYTES.store(0, Ordering::Relaxed);
    WRITEBACK_BATCHES.store(0, Ordering::Relaxed);
    WRITEBACK_BATCH_PAGES.store(0, Ordering::Relaxed);
}

#[cfg(feature = "io_perf_counters")]
pub fn render_perf_counters() -> String {
    let mut out = String::new();
    let _ = writeln!(&mut out, "page_cache:");
    let _ = writeln!(
        &mut out,
        "  read_page_loads {}",
        perf_load(&READ_PAGE_LOADS)
    );
    let _ = writeln!(
        &mut out,
        "  read_page_bytes {}",
        perf_load(&READ_PAGE_BYTES)
    );
    let _ = writeln!(
        &mut out,
        "  write_mapping_calls {}",
        perf_load(&WRITE_MAPPING_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  write_mapping_bytes {}",
        perf_load(&WRITE_MAPPING_BYTES)
    );
    let _ = writeln!(
        &mut out,
        "  sync_mapping_calls {}",
        perf_load(&SYNC_MAPPING_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  sync_range_calls {}",
        perf_load(&SYNC_RANGE_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  writeback_pages {}",
        perf_load(&WRITEBACK_PAGES)
    );
    let _ = writeln!(
        &mut out,
        "  writeback_bytes {}",
        perf_load(&WRITEBACK_BYTES)
    );
    let _ = writeln!(
        &mut out,
        "  writeback_batches {}",
        perf_load(&WRITEBACK_BATCHES)
    );
    let _ = writeln!(
        &mut out,
        "  writeback_batch_pages {}",
        perf_load(&WRITEBACK_BATCH_PAGES)
    );
    out
}

/// 同步某个 inode 指定范围内的脏页。
pub fn sync_inode_range(inode: &Arc<Inode>, offset: usize, len: usize) -> Result<(), ERRNO> {
    if len == 0 || !is_inode_page_cacheable(inode) {
        return Ok(());
    }
    let Some(mapping) = try_get_mapping(inode) else {
        return Ok(());
    };
    mapping.sync_range(offset, len)
}

/// 丢弃指定 inode 的 page cache，并断开旧页 owner，防止删除后脏页迟到回写。
pub fn discard_inode(inode: &Arc<Inode>) {
    if !is_inode_page_cacheable(inode) {
        return;
    }
    let Some(mapping) = inode.take_page_cache_state::<SpinNoIrqLock<PageMapping>>() else {
        return;
    };
    let removed_pages = {
        let mut mapping_guard = mapping.lock();
        for page in mapping_guard.pages.values() {
            let mut page_guard = page.lock();
            page_guard.owner = Weak::new();
            page_guard
                .state
                .remove(CachePageState::DIRTY | CachePageState::WRITEBACK);
            page_guard.pin_count = page_guard.pin_count.saturating_sub(1);
            page_guard.wait_queue.wake_all();
        }
        let removed_pages = mapping_guard.pages.len();
        mapping_guard.pages.clear();
        mapping_guard.dirty_pages.clear();
        removed_pages
    };
    {
        let mut manager = PAGE_CACHE_MANAGER.lock();
        manager.cached_pages = manager.cached_pages.saturating_sub(removed_pages);
        manager.mappings.remove(&InodeKey::from_inode(inode));
    }
}

/// 返回指定页对应的 cache page；供后续 `mmap` 缺页路径复用。
#[allow(dead_code)]
pub fn get_cached_page(inode: &Arc<Inode>, page_idx: u64) -> Option<Arc<SpinNoIrqLock<CachePage>>> {
    mapping_for_inode(inode).and_then(|mapping| mapping.try_get_page(page_idx).ok())
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
        let key = InodeKey::from_inode(inode);
        PAGE_CACHE_MANAGER
            .lock()
            .mappings
            .insert(key, Arc::downgrade(&mapping));
        debug!(
            "[page_cache] mapping miss: inode_ptr={:#x} fs_id={} ino={} size={}",
            Arc::as_ptr(inode) as usize,
            inode.fs_id(),
            inode.ino(),
            current_size
        );
    } else {
        // debug!(
        //     "[page_cache] mapping hit: inode_ptr={:#x} fs_id={} ino={}",
        //     Arc::as_ptr(inode) as usize,
        //     inode.fs_id(),
        //     inode.ino()
        // );
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
        let page =
            get_or_load_page(mapping, page_idx).expect("page cache OOM in buffered read path");

        let page_guard = page.lock();
        let readable = min(
            page_guard.valid_bytes.saturating_sub(page_off),
            end - file_off,
        );
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

    #[cfg(feature = "io_perf_counters")]
    WRITE_MAPPING_CALLS.fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "io_perf_counters")]
    WRITE_MAPPING_BYTES.fetch_add(buf.len(), Ordering::Relaxed);

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
        let page =
            get_or_create_page(mapping, page_idx).expect("page cache OOM in buffered write path");

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
fn truncate_mapping(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    new_size: usize,
) -> Result<(), FS_ERRNO> {
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
        return Ok(());
    }

    debug!(
        "[page_cache] truncate mapping: old_size={} new_size={}",
        old_size, new_size
    );

    // 先让底层 inode 调整成功，再修改 page cache 视图，避免失败时两边长度分离。
    if let Err(err) = inode.truncate(new_size) {
        error!(
            "[page_cache] truncate backing inode failed: fs_id={} ino={} old_size={} new_size={} errno={}",
            inode.fs_id(),
            inode.ino(),
            old_size,
            new_size,
            err as i32
        );
        return Err(err);
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
                first_removed_idx, removed_cnt
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
                new_tail_idx, new_tail_valid
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
                        old_tail_idx, old_tail_valid, new_valid
                    );
                    bytes[old_tail_valid..new_valid].fill(0);
                    page_guard.valid_bytes = max(page_guard.valid_bytes, new_valid);
                    page_guard.state.insert(CachePageState::UPTODATE);
                }
            }
        }
    }
    Ok(())
}

fn fallocate_mapping(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    mode: i32,
    offset: usize,
    len: usize,
) -> Result<(), FS_ERRNO> {
    if mode != 0 {
        return Err(FS_ERRNO::EOPNOTSUPP);
    }
    let new_size = offset.checked_add(len).ok_or(FS_ERRNO::EINVAL)?;
    let inode = {
        let mapping_guard = mapping.lock();
        mapping_guard
            .inode
            .upgrade()
            .expect("page cache inode disappeared")
    };
    let old_size = mapping.lock().size;
    inode.fallocate(mode, offset, len)?;

    if new_size <= old_size {
        return Ok(());
    }

    mapping.lock().size = new_size;

    if old_size > 0 {
        let old_tail_idx = file_page_index(old_size.saturating_sub(1));
        let old_tail_valid = page_valid_bytes_for_size(old_size, old_tail_idx);
        if old_tail_valid < PAGE_SIZE {
            let tail_page = mapping.lock().pages.get(&old_tail_idx).cloned();
            if let Some(tail_page) = tail_page {
                let mut page_guard = tail_page.lock();
                let new_valid = page_valid_bytes_for_size(new_size, old_tail_idx);
                if new_valid > old_tail_valid {
                    let bytes = page_guard.ppn().get_bytes_array();
                    bytes[old_tail_valid..new_valid].fill(0);
                    page_guard.valid_bytes = max(page_guard.valid_bytes, new_valid);
                    page_guard.state.insert(CachePageState::UPTODATE);
                }
            }
        }
    }

    Ok(())
}

/// 获取并确保装入某个文件页对应的 cache page。
fn get_or_load_page(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    page_idx: u64,
) -> Result<Arc<SpinNoIrqLock<CachePage>>, MmError> {
    let page = get_or_create_page(mapping, page_idx)?;
    ensure_page_uptodate(mapping, &page, page_idx);
    Ok(page)
}

/// 获取或创建某个文件页对应的 cache page。
fn get_or_create_page(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    page_idx: u64,
) -> Result<Arc<SpinNoIrqLock<CachePage>>, MmError> {
    if let Some(page) = mapping.lock().pages.get(&page_idx).cloned() {
        trace!("[page_cache] page hit: page_idx={}", page_idx);
        return Ok(page);
    }

    let frame = alloc_cache_frame().ok_or(MmError::OutOfMemory)?;
    let page = Arc::new(SpinNoIrqLock::new(CachePage::new(
        page_idx,
        Arc::downgrade(mapping),
        frame,
    )));

    {
        let mut mapping_guard = mapping.lock();
        if let Some(existing) = mapping_guard.pages.get(&page_idx).cloned() {
            debug!("[page_cache] page hit-after-race: page_idx={}", page_idx);
            return Ok(existing);
        }
        mapping_guard.pages.insert(page_idx, Arc::clone(&page));
    }

    let mut manager = PAGE_CACHE_MANAGER.lock();
    manager.cached_pages += 1;
    manager.inactive.push_back(Arc::downgrade(&page));
    trace!(
        "[page_cache] page miss: page_idx={} cached_pages={}",
        page_idx,
        manager.cached_pages
    );
    drop(manager);
    Ok(page)
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
            trace!(
                "[page_cache] load page: fs_id={} ino={} page_idx={} valid_bytes={}",
                inode.fs_id(),
                inode.ino(),
                page_idx,
                valid_bytes
            );
            // TODO：后续接入通用 truncate 后，需要避免装页与截断并发时把旧数据重新提交回 cache。
            let read = inode.read_at(page_start_off, &mut bytes[..valid_bytes]);
            #[cfg(feature = "io_perf_counters")]
            READ_PAGE_LOADS.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "io_perf_counters")]
            READ_PAGE_BYTES.fetch_add(read, Ordering::Relaxed);
            read
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
    page_guard
        .state
        .insert(CachePageState::DIRTY | CachePageState::UPTODATE);
    mapping.lock().dirty_pages.insert(page_guard.index);
}

/// 同步单个 mapping 的全部脏页。
fn sync_mapping(mapping: &Arc<SpinNoIrqLock<PageMapping>>) -> Result<(), ERRNO> {
    #[cfg(feature = "io_perf_counters")]
    SYNC_MAPPING_CALLS.fetch_add(1, Ordering::Relaxed);
    let dirty_pages: Vec<_> = mapping.lock().dirty_pages.iter().copied().collect();
    flush_dirty_pages(mapping, &dirty_pages)
}

/// 同步单个 mapping 中指定范围的脏页。
fn sync_mapping_range(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    offset: usize,
    len: usize,
) -> Result<(), ERRNO> {
    #[cfg(feature = "io_perf_counters")]
    SYNC_RANGE_CALLS.fetch_add(1, Ordering::Relaxed);
    if len == 0 {
        return Ok(());
    }
    let start_idx = file_page_index(offset);
    let end_off = offset
        .checked_add(len.saturating_sub(1))
        .ok_or(ERRNO::EOVERFLOW)?;
    let end_idx = file_page_index(end_off);
    let dirty_pages: Vec<_> = {
        let mapping_guard = mapping.lock();
        mapping_guard
            .dirty_pages
            .range(start_idx..=end_idx)
            .copied()
            .collect()
    };
    flush_dirty_pages(mapping, &dirty_pages)
}

struct WritebackPage {
    page_idx: u64,
    page: Arc<SpinNoIrqLock<CachePage>>,
}

struct WritebackBatch {
    pages: Vec<WritebackPage>,
    data: Vec<u8>,
    owner_inode: Arc<Inode>,
}

enum BatchCollectResult {
    Batch {
        batch: WritebackBatch,
        consumed: usize,
    },
    Wait(Arc<SpinNoIrqLock<CachePage>>),
    Single(Arc<SpinNoIrqLock<CachePage>>),
    Skip,
}

fn flush_dirty_pages(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    dirty_pages: &[u64],
) -> Result<(), ERRNO> {
    let mut pos = 0usize;
    while pos < dirty_pages.len() {
        match collect_writeback_batch(mapping, dirty_pages, pos) {
            BatchCollectResult::Batch { batch, consumed } => {
                flush_writeback_batch(mapping, batch)?;
                pos += consumed.max(1);
            }
            BatchCollectResult::Wait(page) | BatchCollectResult::Single(page) => {
                flush_page(mapping, &page)?;
                pos += 1;
            }
            BatchCollectResult::Skip => {
                pos += 1;
            }
        }
    }
    Ok(())
}

fn collect_writeback_batch(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    dirty_pages: &[u64],
    start_pos: usize,
) -> BatchCollectResult {
    let owner_inode = {
        let mapping_guard = mapping.lock();
        mapping_guard
            .inode
            .upgrade()
            .expect("page cache inode disappeared")
    };

    let mut pages = Vec::new();
    let mut data = Vec::new();
    let mut consumed = 0usize;
    let mut expected_page_idx = dirty_pages[start_pos];

    while start_pos + consumed < dirty_pages.len() && pages.len() < MAX_WRITEBACK_BATCH_PAGES {
        let page_idx = dirty_pages[start_pos + consumed];
        if page_idx != expected_page_idx {
            break;
        }

        let Some(page) = mapping.lock().pages.get(&page_idx).cloned() else {
            if pages.is_empty() {
                return BatchCollectResult::Skip;
            }
            break;
        };

        let valid_bytes = {
            let mut page_guard = page.lock();
            if page_guard.state.contains(CachePageState::WRITEBACK) {
                if pages.is_empty() {
                    return BatchCollectResult::Wait(page.clone());
                }
                break;
            }
            if !page_guard.state.contains(CachePageState::DIRTY) {
                if pages.is_empty() {
                    return BatchCollectResult::Skip;
                }
                break;
            }
            if page_guard.valid_bytes == 0 {
                if pages.is_empty() {
                    return BatchCollectResult::Single(page.clone());
                }
                break;
            }

            page_guard.state.insert(CachePageState::WRITEBACK);
            page_guard.pin_count += 1;
            let valid_bytes = page_guard.valid_bytes;
            let bytes = page_guard.ppn().get_bytes_array();
            data.extend_from_slice(&bytes[..valid_bytes]);
            valid_bytes
        };

        pages.push(WritebackPage { page_idx, page });
        consumed += 1;

        if valid_bytes != PAGE_SIZE {
            break;
        }
        expected_page_idx += 1;
    }

    if pages.is_empty() {
        BatchCollectResult::Skip
    } else {
        BatchCollectResult::Batch {
            batch: WritebackBatch {
                pages,
                data,
                owner_inode,
            },
            consumed,
        }
    }
}

fn flush_writeback_batch(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    batch: WritebackBatch,
) -> Result<(), ERRNO> {
    let start_page_idx = batch.pages[0].page_idx;
    let expected = batch.data.len();
    let mut write_ok = true;
    #[cfg(feature = "io_perf_counters")]
    {
        WRITEBACK_PAGES.fetch_add(batch.pages.len(), Ordering::Relaxed);
        WRITEBACK_BYTES.fetch_add(expected, Ordering::Relaxed);
        WRITEBACK_BATCHES.fetch_add(1, Ordering::Relaxed);
        WRITEBACK_BATCH_PAGES.fetch_add(batch.pages.len(), Ordering::Relaxed);
    }

    debug!(
        "[page_cache] writeback batch: start_page_idx={} pages={} bytes={}",
        start_page_idx,
        batch.pages.len(),
        expected
    );

    match batch
        .owner_inode
        .write_at_result(page_start(start_page_idx), &batch.data)
    {
        Ok(written) if written == expected => {}
        Ok(written) => {
            error!(
                "[page_cache] short batch writeback: start_page_idx={} expected={} actual={}",
                start_page_idx, expected, written
            );
            write_ok = false;
        }
        Err(err) => {
            error!(
                "[page_cache] batch writeback failed: start_page_idx={} expected={} errno={}",
                start_page_idx, expected, err as i32
            );
            write_ok = false;
        }
    }

    for info in batch.pages {
        finish_page_writeback(mapping, info.page_idx, &info.page, write_ok);
    }

    if write_ok {
        Ok(())
    } else {
        Err(ERRNO::EIO)
    }
}

fn finish_page_writeback(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    page_idx: u64,
    page: &Arc<SpinNoIrqLock<CachePage>>,
    write_ok: bool,
) {
    let wait_queue = {
        let mut page_guard = page.lock();
        if page_guard.state.contains(CachePageState::DIRTY) {
            if !write_ok || page_guard.map_count > 0 {
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
}

/// 将单个脏页写回底层文件。
fn flush_page(
    mapping: &Arc<SpinNoIrqLock<PageMapping>>,
    page: &Arc<SpinNoIrqLock<CachePage>>,
) -> Result<(), ERRNO> {
    loop {
        let (wait_queue, writeback_info) = {
            let mut page_guard = page.lock();
            if page_guard.state.contains(CachePageState::WRITEBACK) {
                (Some(Arc::clone(&page_guard.wait_queue)), None)
            } else if !page_guard.state.contains(CachePageState::DIRTY) {
                return Ok(());
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
                    Some((
                        page_guard.index,
                        page_guard.valid_bytes,
                        page_guard.ppn(),
                        inode,
                    )),
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
        let mut write_ok = true;
        if valid_bytes != 0 {
            let bytes = ppn.get_bytes_array();
            #[cfg(feature = "io_perf_counters")]
            {
                WRITEBACK_PAGES.fetch_add(1, Ordering::Relaxed);
                WRITEBACK_BYTES.fetch_add(valid_bytes, Ordering::Relaxed);
            }
            debug!(
                "[page_cache] writeback page: page_idx={} valid_bytes={}",
                page_idx, valid_bytes
            );
            match owner_inode.write_at_result(page_start(page_idx), &bytes[..valid_bytes]) {
                Ok(written) if written == valid_bytes => {}
                Ok(written) => {
                    error!(
                        "[page_cache] short writeback: page_idx={} expected={} actual={}",
                        page_idx, valid_bytes, written
                    );
                    write_ok = false;
                }
                Err(err) => {
                    error!(
                        "[page_cache] writeback failed: page_idx={} expected={} errno={}",
                        page_idx, valid_bytes, err as i32
                    );
                    write_ok = false;
                }
            }
        }

        finish_page_writeback(mapping, page_idx, page, write_ok);
        if write_ok {
            return Ok(());
        }
        return Err(ERRNO::EIO);
    }
}

/// 若当前缓存压力过大，则回收到低水位。
pub fn reclaim_if_needed() {
    refresh_page_cache_watermarks();
    let mut pass = 0usize;
    let mut deferred_only_passes = 0usize;
    loop {
        let (cached_before, low_watermark, high_watermark, inactive_before) = {
            let manager = PAGE_CACHE_MANAGER.lock();
            (
                manager.cached_pages,
                manager.low_watermark,
                manager.high_watermark,
                manager.inactive.len(),
            )
        };
        if cached_before <= low_watermark {
            break;
        }
        if inactive_before == 0 {
            break;
        }

        pass += 1;
        let stats = run_reclaim_pass(inactive_before);

        if stats.reclaimed > 0 {
            deferred_only_passes = 0;
            continue;
        }

        if stats.deferred == 0 {
            break;
        }

        deferred_only_passes += 1;
        if deferred_only_passes >= MAX_DEFERRED_ONLY_PASSES {
            break;
        }
    }
}

const MAX_DEFERRED_ONLY_PASSES: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReclaimStep {
    Reclaimed,
    Deferred,
    Skipped,
    Stop,
}

#[derive(Clone, Copy, Debug, Default)]
struct ReclaimPassStats {
    scanned: usize,
    reclaimed: usize,
    deferred: usize,
    skipped: usize,
    stopped: bool,
}

fn run_reclaim_pass(scan_budget: usize) -> ReclaimPassStats {
    let mut stats = ReclaimPassStats::default();
    for _ in 0..scan_budget {
        stats.scanned += 1;
        match reclaim_one() {
            ReclaimStep::Reclaimed => stats.reclaimed += 1,
            ReclaimStep::Deferred => stats.deferred += 1,
            ReclaimStep::Skipped => stats.skipped += 1,
            ReclaimStep::Stop => {
                stats.stopped = true;
                break;
            }
        }
    }
    stats
}

/// 尝试推进一轮缓存页回收。
///
/// 返回值区分：
/// - `Reclaimed`：本轮真正释放了一个缓存页；
/// - `Deferred`：本轮只做了“让页更可回收”的软推进，例如清 ref_bit 或写回脏页；
/// - `Skipped`：本轮遇到忙页/失效项等，无实质进展；
/// - `Stop`：当前没有必要或无法继续扫描，外层应停止本轮 direct reclaim。
fn reclaim_one() -> ReclaimStep {
    let candidate = {
        let mut manager = PAGE_CACHE_MANAGER.lock();
        if manager.cached_pages <= manager.low_watermark {
            return ReclaimStep::Stop;
        }
        manager.inactive.pop_front()
    };
    let Some(candidate) = candidate else {
        return ReclaimStep::Stop;
    };
    let Some(page) = candidate.upgrade() else {
        return ReclaimStep::Skipped;
    };

    {
        let mut page_guard = page.lock();
        let page_idx = page_guard.index;
        if page_guard.pin_count > 0
            || page_guard.map_count > 0
            || page_guard.state.intersects(
                CachePageState::LOADING | CachePageState::WRITEBACK | CachePageState::EVICTING,
            )
        {
            let pin_count = page_guard.pin_count;
            let map_count = page_guard.map_count;
            let state_bits = page_guard.state.bits();
            PAGE_CACHE_MANAGER
                .lock()
                .inactive
                .push_back(Arc::downgrade(&page));
            return ReclaimStep::Skipped;
        }
        if page_guard.ref_bit {
            page_guard.ref_bit = false;
            PAGE_CACHE_MANAGER
                .lock()
                .inactive
                .push_back(Arc::downgrade(&page));
            return ReclaimStep::Deferred;
        }
        if page_guard.state.contains(CachePageState::DIRTY) {
            drop(page_guard);
            let mapping = {
                let page_guard = page.lock();
                page_guard.owner.upgrade()
            };

            let flush_result = if let Some(mapping) = mapping.as_ref() {
                flush_page(&mapping, &page)
            } else {
                Ok(())
            };
            PAGE_CACHE_MANAGER
                .lock()
                .inactive
                .push_back(Arc::downgrade(&page));
            return if flush_result.is_ok() {
                ReclaimStep::Deferred
            } else {
                ReclaimStep::Skipped
            };
        }
        page_guard.state.insert(CachePageState::EVICTING);
    }

    let Some(mapping) = page.lock().owner.upgrade() else {
        let mut manager = PAGE_CACHE_MANAGER.lock();
        manager.cached_pages = manager.cached_pages.saturating_sub(1);
        return ReclaimStep::Reclaimed;
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
        ReclaimStep::Reclaimed
    } else {
        page.lock().state.remove(CachePageState::EVICTING);
        PAGE_CACHE_MANAGER
            .lock()
            .inactive
            .push_back(Arc::downgrade(&page));
        ReclaimStep::Skipped
    }
}

/// 按当前物理内存剩余情况刷新 page cache 水位。
fn refresh_page_cache_watermarks() {
    let (high_watermark, low_watermark) = dynamic_watermarks();
    let mut manager = PAGE_CACHE_MANAGER.lock();
    manager.high_watermark = high_watermark;
    manager.low_watermark = low_watermark;
    debug!(
        "[page_cache] refresh watermarks: high={} low={} cached_pages={}",
        manager.high_watermark, manager.low_watermark, manager.cached_pages
    );
}

/// 使用实时物理页统计动态计算水位。
fn dynamic_watermarks() -> (usize, usize) {
    let stats = frame_allocator_stats();
    if stats.total_pages == 0 {
        return default_watermarks();
    }
    let base_high = max(128, stats.total_pages / 4);
    let pressure_high = stats.free_pages / 2;
    let high_watermark = min(base_high, pressure_high);
    let low_watermark = high_watermark * 3 / 4;
    (high_watermark, low_watermark)
}

/// 初始化时使用的保守水位估算。
fn default_watermarks() -> (usize, usize) {
    let mut total_pages = 0usize;
    bootinfo::for_each_usable_memory_region(|region| {
        let start = region.start.div_ceil(PAGE_SIZE);
        let end = region.end / PAGE_SIZE;
        total_pages = total_pages.saturating_add(end.saturating_sub(start));
    });
    let total_pages = max(1, total_pages);
    let high_watermark = max(128, total_pages / 4);
    let low_watermark = max(96, high_watermark * 3 / 4);
    (high_watermark, low_watermark)
}

/// 分配 page cache 使用的页框；内存紧张时先尝试回收一轮。
fn alloc_cache_frame() -> Option<FrameTracker> {
    if let Some(frame) = frame_alloc() {
        return Some(frame);
    }
    let _ = sync_all();
    reclaim_if_needed();
    if let Some(frame) = frame_alloc() {
        return Some(frame);
    }
    let _ = sync_all();
    // 分配失败时再主动推进回收，避免 cache 尚未超过高水位时错过可回收页。
    let mut pass = 0usize;
    let mut deferred_only_passes = 0usize;
    loop {
        let (cached_before, low_watermark, high_watermark, inactive_before) = {
            let manager = PAGE_CACHE_MANAGER.lock();
            (
                manager.cached_pages,
                manager.low_watermark,
                manager.high_watermark,
                manager.inactive.len(),
            )
        };
        if inactive_before == 0 {
            break;
        }

        pass += 1;
        let stats = run_reclaim_pass(inactive_before);

        if stats.reclaimed > 0 {
            deferred_only_passes = 0;
            if let Some(frame) = frame_alloc() {
                return Some(frame);
            }
            continue;
        }

        if stats.deferred == 0 {
            break;
        }

        deferred_only_passes += 1;
        if deferred_only_passes >= MAX_DEFERRED_ONLY_PASSES {
            break;
        }

        if let Some(frame) = frame_alloc() {
            return Some(frame);
        }
    }
    None
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

fn collect_mappings(fs_id: Option<u64>) -> alloc::vec::Vec<Arc<SpinNoIrqLock<PageMapping>>> {
    let mut manager = PAGE_CACHE_MANAGER.lock();
    let mut collected = alloc::vec::Vec::new();

    match fs_id {
        Some(target_fs) => {
            warn!("[page_cache] collecting mappings for fs_id={}", target_fs);
            let start = InodeKey::fs_range_start(target_fs);
            let end = InodeKey::fs_range_end(target_fs);
            let mut stale_keys = alloc::vec::Vec::new();

            for (key, weak) in manager.mappings.range(start..=end) {
                if let Some(mapping) = weak.upgrade() {
                    collected.push(mapping);
                } else {
                    stale_keys.push(*key);
                }
            }

            for key in stale_keys {
                manager.mappings.remove(&key);
            }
        }
        None => {
            warn!("[page_cache] collecting mappings for all");
            manager.mappings.retain(|_, weak| {
                if let Some(mapping) = weak.upgrade() {
                    collected.push(mapping);
                    true
                } else {
                    false
                }
            });
        }
    }

    collected
}

impl PageMappingHandle {
    /// 将当前 mapping 中指定范围的脏页同步到底层文件。
    pub fn sync_range(&self, offset: usize, len: usize) -> Result<(), ERRNO> {
        sync_mapping_range(&self.inner, offset, len)
    }
}
