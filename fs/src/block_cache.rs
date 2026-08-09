//! Block Cache Layer
//! Implements about the disk block cache functionality
use super::{BlockDevice, BlockRead, BlockWrite, BLOCK_SZ};
use alloc::collections::BTreeMap;
#[cfg(feature = "io_perf_counters")]
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
#[cfg(feature = "io_perf_counters")]
use core::fmt::Write;
#[cfg(feature = "io_perf_counters")]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicBool, Ordering};
use lazy_static::*;

use crate::sleep_mutex::SleepMutex as Mutex;

#[cfg(feature = "io_perf_counters")]
static GET_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static GET_HITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static GET_MISSES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static OVERWRITE_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static OVERWRITE_HITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static OVERWRITE_MISSES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static OVERWRITE_DIRECT_WRITE_OPS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static OVERWRITE_DIRECT_WRITE_BLOCKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static OVERWRITE_RANGES_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static OVERWRITE_RANGES_ITEMS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static OVERWRITE_RANGES_SINGLE_ITEM_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static OVERWRITE_RANGES_BLOCKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static EVICTIONS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static LOOKUP_SCAN_STEPS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static EVICT_SCAN_STEPS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static SYNC_ALL_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static SYNC_BLOCK_VISITS: AtomicUsize = AtomicUsize::new(0);

/// BlockCache is a cache for a block in disk.
pub struct BlockCache {
    cache: Vec<u8>,
    block_id: usize,
    block_device: Arc<dyn BlockDevice>,
    modified: bool,
    /// Approximate CLOCK reference bit.  It is atomic because cache hits
    /// already hold the entry mutex only while copying data; the manager
    /// needs to sample/clear this bit while choosing a victim.
    ref_bit: AtomicBool,
}

impl BlockCache {
    /// Load a new BlockCache from disk.
    pub fn new(block_id: usize, block_device: Arc<dyn BlockDevice>) -> Self {
        // for alignment and move effciency
        let mut cache = vec![0u8; BLOCK_SZ];
        block_device.read_block(block_id, &mut cache);
        Self {
            cache,
            block_id,
            block_device,
            modified: false,
            ref_bit: AtomicBool::new(true),
        }
    }

    /// Create a cache entry for a block whose entire content is being overwritten.
    pub fn new_overwrite(block_id: usize, block_device: Arc<dyn BlockDevice>, data: &[u8]) -> Self {
        assert!(data.len() == BLOCK_SZ);
        let mut cache = vec![0u8; BLOCK_SZ];
        cache.copy_from_slice(data);
        Self {
            cache,
            block_id,
            block_device,
            modified: true,
            ref_bit: AtomicBool::new(true),
        }
    }

    /// Create a cache entry from freshly read on-disk bytes.
    pub fn new_from_bytes(block_id: usize, block_device: Arc<dyn BlockDevice>, data: &[u8]) -> Self {
        assert!(data.len() == BLOCK_SZ);
        let mut cache = vec![0u8; BLOCK_SZ];
        cache.copy_from_slice(data);
        Self {
            cache,
            block_id,
            block_device,
            modified: false,
            ref_bit: AtomicBool::new(true),
        }
    }

    #[inline]
    fn touch(&self) {
        self.ref_bit.store(true, Ordering::Relaxed);
    }

    #[inline]
    fn take_ref_bit(&self) -> bool {
        self.ref_bit.swap(false, Ordering::Relaxed)
    }

    /// Get the slice in the block cache according to the offset.
    fn addr_of_offset(&self, offset: usize) -> usize {
        &self.cache[offset] as *const _ as usize
    }
    /// Get a immutable reference to the data in the block cache according to the offset.
    pub fn get_ref<T>(&self, offset: usize) -> &T
    where
        T: Sized,
    {
        let type_size = core::mem::size_of::<T>();
        assert!(offset + type_size <= BLOCK_SZ);
        self.touch();
        let addr = self.addr_of_offset(offset);
        unsafe { &*(addr as *const T) }
    }
    /// Get a mutable reference to the data in the block cache according to the offset.
    pub fn get_mut<T>(&mut self, offset: usize) -> &mut T
    where
        T: Sized,
    {
        let type_size = core::mem::size_of::<T>();
        assert!(offset + type_size <= BLOCK_SZ);
        self.modified = true;
        self.touch();
        let addr = self.addr_of_offset(offset);
        unsafe { &mut *(addr as *mut T) }
    }
    /// Read the data from the block cache according to the offset.
    pub fn read<T, V>(&self, offset: usize, f: impl FnOnce(&T) -> V) -> V {
        f(self.get_ref(offset))
    }
    /// Write the data into the block cache according to the offset.
    pub fn modify<T, V>(&mut self, offset: usize, f: impl FnOnce(&mut T) -> V) -> V {
        f(self.get_mut(offset))
    }

    /// Read raw bytes from this cached block.
    ///
    /// This is preferred for parsing on-disk packed data structures (e.g. FAT32 BPB/dir entries)
    /// because it avoids creating potentially unaligned typed references.
    pub fn read_bytes(&self, offset: usize, buf: &mut [u8]) {
        assert!(offset + buf.len() <= BLOCK_SZ);
        self.touch();
        buf.copy_from_slice(&self.cache[offset..offset + buf.len()]);
    }

    /// Write raw bytes into this cached block.
    ///
    /// Marks the block as modified.
    pub fn write_bytes(&mut self, offset: usize, data: &[u8]) {
        assert!(offset + data.len() <= BLOCK_SZ);
        self.modified = true;
        self.touch();
        self.cache[offset..offset + data.len()].copy_from_slice(data);
    }

    /// Sync(write) the block cache to disk.
    pub fn sync(&mut self) {
        if self.modified {
            self.modified = false;
            self.block_device.write_block(self.block_id, &self.cache);
        }
    }
}

impl Drop for BlockCache {
    fn drop(&mut self) {
        self.sync()
    }
}

const BLOCK_CACHE_SIZE: usize = 65536;

/// BlockCacheManager is a manager for BlockCache.
pub struct BlockCacheManager {
    /// Cache keys arranged in a circular CLOCK hand.
    ///
    /// Unlike an LRU list, a hit only sets the entry's reference bit and does
    /// not move anything in this vector.  Victim slots are reused in place so
    /// the queue does not grow during repeated evictions.
    queue: Vec<(usize, usize)>,
    /// Next slot examined by the CLOCK hand.
    clock_hand: usize,
    /// Cache entries indexed by `(device_id, block_id)` for fast lookup.
    map: BTreeMap<(usize, usize), Arc<Mutex<BlockCache>>>,
}

impl BlockCacheManager {
    /// Create a new BlockCacheManager with an empty queue.
    pub fn new() -> Self {
        Self {
            queue: Vec::new(),
            clock_hand: 0,
            map: BTreeMap::new(),
        }
    }

    fn device_id(block_device: &Arc<dyn BlockDevice>) -> usize {
        Arc::as_ptr(block_device) as *const () as usize
    }

    /// Make room for one insertion and return a reusable queue slot.
    ///
    /// A referenced, unpinned entry receives a second chance: its reference
    /// bit is cleared and the hand advances.  Pinned entries are skipped
    /// until they become available.  The returned slot is replaced by the
    /// caller after the new entry has been constructed.
    fn evict_one_if_needed(&mut self) -> Option<usize> {
        if self.map.len() < BLOCK_CACHE_SIZE {
            return None;
        }

        let queue_len = self.queue.len();
        if queue_len == 0 {
            panic!("BlockCache queue is empty while cache map is full");
        }

        // Two passes are sufficient for CLOCK: the first pass clears all
        // second-chance bits, and the second pass can select an unpinned
        // unreferenced entry.  A pinned entry remains ineligible.
        let max_scan = queue_len.saturating_mul(2).max(1);
        for scanned in 0..max_scan {
            #[cfg(not(feature = "io_perf_counters"))]
            let _ = scanned;
            let idx = self.clock_hand % queue_len;
            let key = self.queue[idx];
            let Some(cache) = self.map.get(&key) else {
                // This should not normally happen, but recovering here keeps
                // a stale queue slot from wedging the cache manager.
                self.clock_hand = (idx + 1) % queue_len;
                #[cfg(feature = "io_perf_counters")]
                EVICT_SCAN_STEPS.fetch_add(1, Ordering::Relaxed);
                continue;
            };

            if Arc::strong_count(cache) == 1 && !cache.lock().take_ref_bit() {
                #[cfg(feature = "io_perf_counters")]
                EVICT_SCAN_STEPS.fetch_add(scanned + 1, Ordering::Relaxed);
                #[cfg(feature = "io_perf_counters")]
                EVICTIONS.fetch_add(1, Ordering::Relaxed);

                // Removing the manager's Arc may synchronously write back a
                // dirty entry through BlockCache::Drop, preserving the old
                // correctness behavior.  The slot itself is reused below.
                self.map.remove(&key);
                self.clock_hand = (idx + 1) % queue_len;
                return Some(idx);
            }

            self.clock_hand = (idx + 1) % queue_len;
        }

        #[cfg(feature = "io_perf_counters")]
        EVICT_SCAN_STEPS.fetch_add(max_scan, Ordering::Relaxed);
        panic!("Run out of BlockCache: all entries are pinned");
    }

    fn install_entry(
        &mut self,
        key: (usize, usize),
        block_cache: Arc<Mutex<BlockCache>>,
        reusable_slot: Option<usize>,
    ) {
        if let Some(idx) = reusable_slot {
            self.queue[idx] = key;
        } else {
            self.queue.push(key);
        }
        self.map.insert(key, block_cache);
    }

    fn insert_prefetched_block(
        &mut self,
        block_id: usize,
        block_device: Arc<dyn BlockDevice>,
        data: &[u8],
    ) -> Arc<Mutex<BlockCache>> {
        let key = (Self::device_id(&block_device), block_id);
        if let Some(block_cache) = self.map.get(&key) {
            return Arc::clone(block_cache);
        }
        let reusable_slot = self.evict_one_if_needed();
        let block_cache = Arc::new(Mutex::new(BlockCache::new_from_bytes(
            block_id,
            Arc::clone(&block_device),
            data,
        )));
        self.install_entry(key, Arc::clone(&block_cache), reusable_slot);
        block_cache
    }

    /// Get a block cache from the queue. according to the block_id.
    pub fn get_block_cache(
        &mut self,
        block_id: usize,
        block_device: Arc<dyn BlockDevice>,
    ) -> Arc<Mutex<BlockCache>> {
        #[cfg(feature = "io_perf_counters")]
        GET_CALLS.fetch_add(1, Ordering::Relaxed);
        let key = (Self::device_id(&block_device), block_id);
        #[cfg(feature = "io_perf_counters")]
        LOOKUP_SCAN_STEPS.fetch_add(1, Ordering::Relaxed);
        if let Some(block_cache) = self.map.get(&key) {
            #[cfg(feature = "io_perf_counters")]
            GET_HITS.fetch_add(1, Ordering::Relaxed);
            block_cache.lock().touch();
            return Arc::clone(block_cache);
        }
        #[cfg(feature = "io_perf_counters")]
        GET_MISSES.fetch_add(1, Ordering::Relaxed);

        let reusable_slot = self.evict_one_if_needed();
        // load block into mem and push back
        let block_cache = Arc::new(Mutex::new(BlockCache::new(
            block_id,
            Arc::clone(&block_device),
        )));
        self.install_entry(key, Arc::clone(&block_cache), reusable_slot);
        block_cache
    }

    /// Read a contiguous range of 512-byte blocks, satisfying cache misses with
    /// one backend `read_blocks` per contiguous miss run and backfilling the
    /// block cache with the returned data.
    pub fn read_block_cache_range(
        &mut self,
        start_block: usize,
        block_device: Arc<dyn BlockDevice>,
        buf: &mut [u8],
    ) {
        let mut read = BlockRead { start_block, data: buf };
        self.read_block_cache_ranges(block_device, core::slice::from_mut(&mut read));
    }

    /// Read several independent ranges and submit all cold miss runs as one
    /// device batch, while preserving the shared block-cache contents.
    pub fn read_block_cache_ranges(
        &mut self,
        block_device: Arc<dyn BlockDevice>,
        ranges: &mut [BlockRead<'_>],
    ) {
        let device_id = Self::device_id(&block_device);
        let mut pending_meta = Vec::new();

        for (range_idx, range) in ranges.iter_mut().enumerate() {
            assert!(range.data.len() % BLOCK_SZ == 0);
            let block_count = range.data.len() / BLOCK_SZ;
            #[cfg(feature = "io_perf_counters")]
            GET_CALLS.fetch_add(block_count, Ordering::Relaxed);

            let mut idx = 0usize;
            while idx < block_count {
                #[cfg(feature = "io_perf_counters")]
                LOOKUP_SCAN_STEPS.fetch_add(1, Ordering::Relaxed);
                let key = (device_id, range.start_block + idx);
                if let Some(block_cache) = self.map.get(&key).cloned() {
                    #[cfg(feature = "io_perf_counters")]
                    GET_HITS.fetch_add(1, Ordering::Relaxed);
                    let offset = idx * BLOCK_SZ;
                    block_cache
                        .lock()
                        .read_bytes(0, &mut range.data[offset..offset + BLOCK_SZ]);
                    idx += 1;
                    continue;
                }

                let miss_start = idx;
                idx += 1;
                while idx < block_count {
                    #[cfg(feature = "io_perf_counters")]
                    LOOKUP_SCAN_STEPS.fetch_add(1, Ordering::Relaxed);
                    let key = (device_id, range.start_block + idx);
                    if self.map.contains_key(&key) {
                        break;
                    }
                    idx += 1;
                }
                #[cfg(feature = "io_perf_counters")]
                GET_MISSES.fetch_add(idx - miss_start, Ordering::Relaxed);
                pending_meta.push((range_idx, miss_start, idx));
            }
        }

        // A page-cache read frequently finds every backing block resident.
        // Avoid allocating an empty request vector and crossing into the
        // instrumented device path for that common all-hit case.
        if pending_meta.is_empty() {
            return;
        }

        let mut pending = Vec::with_capacity(pending_meta.len());
        for &(range_idx, miss_start, miss_end) in &pending_meta {
            // The metadata pass has ended. Miss intervals are disjoint, so
            // these reborrows do not alias the other pending requests.
            let range = unsafe { &mut *ranges.as_mut_ptr().add(range_idx) };
            let byte_start = miss_start * BLOCK_SZ;
            let byte_end = miss_end * BLOCK_SZ;
            let data = unsafe {
                core::slice::from_raw_parts_mut(
                    range.data.as_mut_ptr().add(byte_start),
                    byte_end - byte_start,
                )
            };
            pending.push(BlockRead {
                start_block: range.start_block + miss_start,
                data,
            });
        }

        block_device.read_blocks_many(&mut pending);
        for read in &pending {
            for (idx, block) in read.data.chunks(BLOCK_SZ).enumerate() {
                self.insert_prefetched_block(
                    read.start_block + idx,
                    Arc::clone(&block_device),
                    block,
                );
            }
        }
    }

    pub fn overwrite_block_cache_range(
        &mut self,
        start_block: usize,
        block_device: Arc<dyn BlockDevice>,
        data: &[u8],
    ) {
        assert!(data.len() % BLOCK_SZ == 0);
        let block_count = data.len() / BLOCK_SZ;
        #[cfg(feature = "io_perf_counters")]
        OVERWRITE_CALLS.fetch_add(block_count, Ordering::Relaxed);
        if block_count == 0 {
            return;
        }
        let device_id = Self::device_id(&block_device);
        let end_block = start_block
            .checked_add(block_count)
            .expect("block cache overwrite range overflow");
        let start_key = (device_id, start_block);
        let end_key = (device_id, end_block);

        #[cfg(feature = "io_perf_counters")]
        let mut hits = 0usize;
        #[cfg(not(feature = "io_perf_counters"))]
        let hits = ();

        for ((_, block_id), block_cache) in self.map.range(start_key..end_key) {
            let idx = *block_id - start_block;
            let offset = idx * BLOCK_SZ;
            block_cache
                .lock()
                .write_bytes(0, &data[offset..offset + BLOCK_SZ]);
            #[cfg(feature = "io_perf_counters")]
            {
                hits += 1;
            }
        }

        #[cfg(feature = "io_perf_counters")]
        {
            LOOKUP_SCAN_STEPS.fetch_add(1 + hits, Ordering::Relaxed);
            OVERWRITE_HITS.fetch_add(hits, Ordering::Relaxed);
            OVERWRITE_MISSES.fetch_add(block_count - hits, Ordering::Relaxed);
        }
        #[cfg(not(feature = "io_perf_counters"))]
        let _ = hits;
    }
}

#[cfg(feature = "io_perf_counters")]
fn load(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::Relaxed)
}

#[cfg(feature = "io_perf_counters")]
pub fn reset_perf_counters() {
    GET_CALLS.store(0, Ordering::Relaxed);
    GET_HITS.store(0, Ordering::Relaxed);
    GET_MISSES.store(0, Ordering::Relaxed);
    OVERWRITE_CALLS.store(0, Ordering::Relaxed);
    OVERWRITE_HITS.store(0, Ordering::Relaxed);
    OVERWRITE_MISSES.store(0, Ordering::Relaxed);
    OVERWRITE_DIRECT_WRITE_OPS.store(0, Ordering::Relaxed);
    OVERWRITE_DIRECT_WRITE_BLOCKS.store(0, Ordering::Relaxed);
    OVERWRITE_RANGES_CALLS.store(0, Ordering::Relaxed);
    OVERWRITE_RANGES_ITEMS.store(0, Ordering::Relaxed);
    OVERWRITE_RANGES_SINGLE_ITEM_CALLS.store(0, Ordering::Relaxed);
    OVERWRITE_RANGES_BLOCKS.store(0, Ordering::Relaxed);
    EVICTIONS.store(0, Ordering::Relaxed);
    LOOKUP_SCAN_STEPS.store(0, Ordering::Relaxed);
    EVICT_SCAN_STEPS.store(0, Ordering::Relaxed);
    SYNC_ALL_CALLS.store(0, Ordering::Relaxed);
    SYNC_BLOCK_VISITS.store(0, Ordering::Relaxed);
}

#[cfg(feature = "io_perf_counters")]
pub fn render_perf_counters() -> String {
    let get_calls = load(&GET_CALLS);
    let overwrite_calls = load(&OVERWRITE_CALLS);
    let lookups = get_calls + overwrite_calls;
    let evictions = load(&EVICTIONS);
    let lookup_steps = load(&LOOKUP_SCAN_STEPS);
    let evict_scan_steps = load(&EVICT_SCAN_STEPS);
    let cached_blocks = BLOCK_CACHE_MANAGER.lock().map.len();

    let mut out = String::new();
    let _ = writeln!(&mut out, "block_cache:");
    let _ = writeln!(&mut out, "  cached_blocks {}", cached_blocks);
    let _ = writeln!(&mut out, "  get_calls {}", get_calls);
    let _ = writeln!(&mut out, "  get_hits {}", load(&GET_HITS));
    let _ = writeln!(&mut out, "  get_misses {}", load(&GET_MISSES));
    let _ = writeln!(&mut out, "  overwrite_calls {}", overwrite_calls);
    let _ = writeln!(&mut out, "  overwrite_hits {}", load(&OVERWRITE_HITS));
    let _ = writeln!(&mut out, "  overwrite_misses {}", load(&OVERWRITE_MISSES));
    let _ = writeln!(
        &mut out,
        "  overwrite_direct_write_ops {}",
        load(&OVERWRITE_DIRECT_WRITE_OPS)
    );
    let _ = writeln!(
        &mut out,
        "  overwrite_direct_write_blocks {}",
        load(&OVERWRITE_DIRECT_WRITE_BLOCKS)
    );
    let _ = writeln!(
        &mut out,
        "  overwrite_ranges_calls {}",
        load(&OVERWRITE_RANGES_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  overwrite_ranges_items {}",
        load(&OVERWRITE_RANGES_ITEMS)
    );
    let _ = writeln!(
        &mut out,
        "  overwrite_ranges_single_item_calls {}",
        load(&OVERWRITE_RANGES_SINGLE_ITEM_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  overwrite_ranges_blocks {}",
        load(&OVERWRITE_RANGES_BLOCKS)
    );
    let _ = writeln!(&mut out, "  evictions {}", evictions);
    let _ = writeln!(&mut out, "  lookup_steps {}", lookup_steps);
    let _ = writeln!(
        &mut out,
        "  avg_lookup_steps_x100 {}",
        if lookups == 0 {
            0
        } else {
            lookup_steps.saturating_mul(100) / lookups
        }
    );
    let _ = writeln!(&mut out, "  evict_scan_steps {}", evict_scan_steps);
    let _ = writeln!(
        &mut out,
        "  avg_evict_scan_x100 {}",
        if evictions == 0 {
            0
        } else {
            evict_scan_steps.saturating_mul(100) / evictions
        }
    );
    let _ = writeln!(&mut out, "  sync_all_calls {}", load(&SYNC_ALL_CALLS));
    let _ = writeln!(
        &mut out,
        "  sync_block_visits {}",
        load(&SYNC_BLOCK_VISITS)
    );
    out
}

lazy_static! {
    /// BLOCK_CACHE_MANAGER: Glocal instance of BlockCacheManager.
    pub static ref BLOCK_CACHE_MANAGER: Mutex<BlockCacheManager> =
        Mutex::new(BlockCacheManager::new());
}
/// Get a block cache from the queue. according to the block_id.
pub fn get_block_cache(
    block_id: usize,
    block_device: Arc<dyn BlockDevice>,
) -> Arc<Mutex<BlockCache>> {
    BLOCK_CACHE_MANAGER
        .lock()
        .get_block_cache(block_id, block_device)
}

/// Read a contiguous range of 512-byte blocks, merging cold miss runs into
/// larger backend reads while preserving block-cache coherence.
pub fn read_block_cache_range(
    start_block: usize,
    block_device: Arc<dyn BlockDevice>,
    buf: &mut [u8],
) {
    BLOCK_CACHE_MANAGER
        .lock()
        .read_block_cache_range(start_block, block_device, buf)
}

/// Read several independent ranges through the shared block cache.
pub fn read_block_cache_ranges(
    block_device: Arc<dyn BlockDevice>,
    ranges: &mut [BlockRead<'_>],
) {
    BLOCK_CACHE_MANAGER
        .lock()
        .read_block_cache_ranges(block_device, ranges)
}

/// Overwrite a complete block cache entry without first loading old disk data.
pub fn overwrite_block_cache(block_id: usize, block_device: Arc<dyn BlockDevice>, data: &[u8]) {
    overwrite_block_cache_range(block_id, block_device, data);
}

/// Overwrite complete contiguous blocks without first loading old disk data.
pub fn overwrite_block_cache_range(
    start_block: usize,
    block_device: Arc<dyn BlockDevice>,
    data: &[u8],
) {
    assert!(data.len() % BLOCK_SZ == 0);
    if data.is_empty() {
        return;
    }

    BLOCK_CACHE_MANAGER.lock().overwrite_block_cache_range(
        start_block,
        Arc::clone(&block_device),
        data,
    );
    #[cfg(feature = "io_perf_counters")]
    OVERWRITE_DIRECT_WRITE_OPS.fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "io_perf_counters")]
    OVERWRITE_DIRECT_WRITE_BLOCKS.fetch_add(data.len() / BLOCK_SZ, Ordering::Relaxed);
    block_device.write_blocks(start_block, data);
}

/// Overwrite multiple complete contiguous block ranges without first loading old disk data.
pub fn overwrite_block_cache_ranges(block_device: Arc<dyn BlockDevice>, writes: &[BlockWrite<'_>]) {
    if writes.is_empty() {
        return;
    }

    let mut write_count = 0usize;
    #[cfg(feature = "io_perf_counters")]
    let mut block_count = 0usize;
    {
        let mut manager = BLOCK_CACHE_MANAGER.lock();
        for write in writes {
            assert!(write.data.len() % BLOCK_SZ == 0);
            if write.data.is_empty() {
                continue;
            }
            write_count += 1;
            #[cfg(feature = "io_perf_counters")]
            {
                block_count += write.data.len() / BLOCK_SZ;
            }
            manager.overwrite_block_cache_range(
                write.start_block,
                Arc::clone(&block_device),
                write.data,
            );
        }
    }

    if write_count == 0 {
        return;
    }
    #[cfg(feature = "io_perf_counters")]
    {
        OVERWRITE_RANGES_CALLS.fetch_add(1, Ordering::Relaxed);
        OVERWRITE_RANGES_ITEMS.fetch_add(write_count, Ordering::Relaxed);
        if write_count == 1 {
            OVERWRITE_RANGES_SINGLE_ITEM_CALLS.fetch_add(1, Ordering::Relaxed);
        }
        OVERWRITE_RANGES_BLOCKS.fetch_add(block_count, Ordering::Relaxed);
    }
    #[cfg(feature = "io_perf_counters")]
    OVERWRITE_DIRECT_WRITE_OPS.fetch_add(write_count, Ordering::Relaxed);
    #[cfg(feature = "io_perf_counters")]
    OVERWRITE_DIRECT_WRITE_BLOCKS.fetch_add(block_count, Ordering::Relaxed);
    block_device.write_blocks_many(writes);
}

/// Sync(write) all the block cache to disk.
pub fn block_cache_sync_all() {
    #[cfg(feature = "io_perf_counters")]
    SYNC_ALL_CALLS.fetch_add(1, Ordering::Relaxed);
    let manager = BLOCK_CACHE_MANAGER.lock();
    for cache in manager.map.values() {
        #[cfg(feature = "io_perf_counters")]
        SYNC_BLOCK_VISITS.fetch_add(1, Ordering::Relaxed);
        cache.lock().sync();
    }
}
