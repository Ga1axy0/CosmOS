use alloc::{collections::BTreeMap, string::String, sync::{Arc, Weak}, vec, vec::Vec};
use core::any::Any;
use core::cmp::min;
use core::fmt;
#[cfg(feature = "io_perf_counters")]
use core::fmt::Write;
#[cfg(feature = "io_perf_counters")]
use core::sync::atomic::{AtomicUsize, Ordering};
use log::{debug, info};

use crate::block_cache::{
    get_block_cache, overwrite_block_cache_range, overwrite_block_cache_ranges,
    read_block_cache_range, read_block_cache_ranges,
};
use crate::block_dev::{
    BlockDevice as OsBlockDevice, BlockRead as OsBlockRead, BlockWrite as OsBlockWrite,
};
use crate::dentry_cache::insert_dentry;
use crate::errno::FS_ERRNO;
use crate::sleep_mutex::SleepMutex as Mutex;
use crate::{STATFS_MAGIC_EXT4, STATFS_NAMELEN_DEFAULT, VfsStatFs};
use crate::vfs::{Inode, InodeTime, VfsAttrs, VfsFileType, VfsNode};
use crate::BLOCK_SZ;

use ext4_rs::{
    BlockDevice as Ext4BlockDevice, BlockWrite as Ext4BlockWrite, Ext4, InodeFileType, BLOCK_SIZE,
};

/// Adapts the OS block-id based device into ext4_rs offset-based IO.
struct Ext4BlockDeviceAdapter {
    inner: Arc<dyn OsBlockDevice>,
}

#[cfg(feature = "io_perf_counters")]
static WRITE_OFFSETS_MANY_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_OFFSETS_MANY_ITEMS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_OFFSETS_MANY_SINGLE_ITEM_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_OFFSETS_MANY_ALIGNED_ITEMS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static WRITE_OFFSETS_MANY_UNALIGNED_ITEMS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static PAGE_CACHE_READ_PLAN_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static PAGE_CACHE_READ_PLAN_HITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static PAGE_CACHE_READ_PLAN_FALLBACKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static PAGE_CACHE_READ_PLAN_BLOCKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static PAGE_CACHE_READ_PLAN_US: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static PAGE_CACHE_READ_IO_US: AtomicUsize = AtomicUsize::new(0);
// Wall time waiting for, and holding, the per-inode mapping lock.  Hold time
// intentionally includes the data I/O: it is the same-inode serialization
// window that a range-lock implementation could potentially expose.
#[cfg(feature = "io_perf_counters")]
static READ_MAPPING_LOCK_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static READ_MAPPING_LOCK_WAIT_US: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static READ_MAPPING_LOCK_HOLD_US: AtomicUsize = AtomicUsize::new(0);
// Mapping time is measured after acquiring the global ext4 mutex and around
// prepare_aligned_read_at.  It therefore isolates inode/extent lookup and
// physical-offset vector construction from global ext4-mutex wait time.
#[cfg(feature = "io_perf_counters")]
static EXTENT_MAP_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static EXTENT_MAP_BLOCKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static EXTENT_MAP_RUNS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static EXTENT_MAP_US: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static EXTENT_MAP_FALLBACKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static EXTENT_MAP_ERRORS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static DIR_LOOKUP_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static DIR_LOOKUP_HITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static DIR_LOOKUP_MISSES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static DIR_LOOKUP_BLOCKS_SCANNED: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static DIR_LOOKUP_DIRENTS_SCANNED: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static LS_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static LS_ENTRIES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static GETDENTS_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static GETDENTS_NONEMPTY_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static GETDENTS_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static GETDENTS_BACKEND_US: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static GETDENTS_PRIME_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static GETDENTS_PRIME_ENTRIES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static GETDENTS_PRIME_US: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "io_perf_counters")]
static PERF_TIME_NOW_US: AtomicUsize = AtomicUsize::new(0);

const EXT4_ROOT_INODE: u32 = 2;

#[inline]
fn decode_ext4_time(sec_lo: u32, extra: u32) -> InodeTime {
    let sec_hi = (extra & 0x3) as u64;
    let nsec = extra >> 2;
    InodeTime::new((sec_lo as u64) | (sec_hi << 32), nsec)
}

#[inline]
fn encode_ext4_time(ts: InodeTime) -> (u32, u32) {
    let sec_lo = ts.sec as u32;
    let sec_hi = ((ts.sec >> 32) as u32) & 0x3;
    let nsec = ts.nsec & 0x3fff_ffff;
    (sec_lo, (nsec << 2) | sec_hi)
}

impl Ext4BlockDeviceAdapter {
    fn new(inner: Arc<dyn OsBlockDevice>) -> Self {
        Self { inner }
    }
}

impl Ext4BlockDevice for Ext4BlockDeviceAdapter {
    fn read_offset(&self, offset: usize) -> Vec<u8> {
        let len = ext4_rs::BLOCK_SIZE;
        let start_block = offset / BLOCK_SZ;
        let end_block = (offset + len).div_ceil(BLOCK_SZ);
        let aligned_offset = start_block * BLOCK_SZ;
        let aligned_len = (end_block - start_block) * BLOCK_SZ;
        let mut aligned = vec![0u8; aligned_len];
        read_block_cache_range(start_block, Arc::clone(&self.inner), &mut aligned);
        let src_start = offset - aligned_offset;
        if src_start == 0 && aligned_len == len {
            return aligned;
        }
        let mut out = vec![0u8; len];
        out.copy_from_slice(&aligned[src_start..src_start + len]);
        out
    }

    fn read_offset_uncached(&self, offset: usize) -> Vec<u8> {
        let len = ext4_rs::BLOCK_SIZE;
        let start_block = offset / BLOCK_SZ;
        let end_block = (offset + len).div_ceil(BLOCK_SZ);
        let aligned_offset = start_block * BLOCK_SZ;
        let aligned_len = (end_block - start_block) * BLOCK_SZ;
        let mut aligned = vec![0u8; aligned_len];
        self.inner.read_blocks(start_block, &mut aligned);

        let src_start = offset - aligned_offset;
        if src_start == 0 && aligned_len == len {
            return aligned;
        }
        let mut out = vec![0u8; len];
        out.copy_from_slice(&aligned[src_start..src_start + len]);
        out
    }

    fn read_offsets(&self, offsets: &[usize], data: &mut [u8]) {
        assert_eq!(data.len(), offsets.len() * ext4_rs::BLOCK_SIZE);
        let mut pending: Vec<OsBlockRead<'_>> = Vec::new();
        let mut run_start = 0;
        let mut data_rest = data;
        while run_start < offsets.len() {
            let mut run_end = run_start + 1;
            while run_end < offsets.len()
                && offsets[run_end] == offsets[run_end - 1] + ext4_rs::BLOCK_SIZE
            {
                run_end += 1;
            }
            let run_len = (run_end - run_start) * ext4_rs::BLOCK_SIZE;
            let (run_data, rest) = data_rest.split_at_mut(run_len);
            let start_block = offsets[run_start] / BLOCK_SZ;
            pending.push(OsBlockRead {
                start_block,
                data: run_data,
            });
            data_rest = rest;
            run_start = run_end;
        }
        read_block_cache_ranges(Arc::clone(&self.inner), &mut pending);
    }

    fn read_offsets_uncached(&self, offsets: &[usize], data: &mut [u8]) {
        assert_eq!(data.len(), offsets.len() * ext4_rs::BLOCK_SIZE);
        if offsets.is_empty() {
            return;
        }

        // Merge physically contiguous filesystem blocks into larger direct
        // block-device requests.  The block cache is deliberately not
        // touched by this path.
        let mut pending: Vec<OsBlockRead<'_>> = Vec::new();
        let mut run_start = 0;
        let mut data_rest = data;
        while run_start < offsets.len() {
            let mut run_end = run_start + 1;
            while run_end < offsets.len()
                && offsets[run_end] == offsets[run_end - 1] + ext4_rs::BLOCK_SIZE
            {
                run_end += 1;
            }
            let run_len = (run_end - run_start) * ext4_rs::BLOCK_SIZE;
            let (run_data, rest) = data_rest.split_at_mut(run_len);
            assert_eq!(offsets[run_start] % BLOCK_SZ, 0);
            pending.push(OsBlockRead {
                start_block: offsets[run_start] / BLOCK_SZ,
                data: run_data,
            });
            data_rest = rest;
            run_start = run_end;
        }
        self.inner.read_blocks_many(&mut pending);
    }

    fn write_offset(&self, offset: usize, data: &[u8]) {
        if data.is_empty() {
            return;
        }

        let mut written = 0usize;

        if offset % BLOCK_SZ != 0 {
            let block_id = offset / BLOCK_SZ;
            let dst_start = offset % BLOCK_SZ;
            let len = (BLOCK_SZ - dst_start).min(data.len());
            get_block_cache(block_id, Arc::clone(&self.inner))
                .lock()
                .write_bytes(dst_start, &data[..len]);
            written += len;
        }

        let aligned_len = (data.len() - written) / BLOCK_SZ * BLOCK_SZ;
        if aligned_len != 0 {
            let start_block = (offset + written) / BLOCK_SZ;
            overwrite_block_cache_range(
                start_block,
                Arc::clone(&self.inner),
                &data[written..written + aligned_len],
            );
            written += aligned_len;
        }

        if written < data.len() {
            let block_id = (offset + written) / BLOCK_SZ;
            get_block_cache(block_id, Arc::clone(&self.inner))
                .lock()
                .write_bytes(0, &data[written..]);
        }
    }

    fn write_offsets_many(&self, writes: &[Ext4BlockWrite<'_>]) {
        #[cfg(feature = "io_perf_counters")]
        {
            let non_empty = writes.iter().filter(|write| !write.data.is_empty()).count();
            if non_empty != 0 {
                WRITE_OFFSETS_MANY_CALLS.fetch_add(1, Ordering::Relaxed);
                WRITE_OFFSETS_MANY_ITEMS.fetch_add(non_empty, Ordering::Relaxed);
                if non_empty == 1 {
                    WRITE_OFFSETS_MANY_SINGLE_ITEM_CALLS.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        let mut pending: Vec<OsBlockWrite<'_>> = Vec::new();
        for write in writes {
            if write.data.is_empty() {
                continue;
            }
            if offset_and_len_are_block_aligned(write.offset, write.data.len()) {
                #[cfg(feature = "io_perf_counters")]
                WRITE_OFFSETS_MANY_ALIGNED_ITEMS.fetch_add(1, Ordering::Relaxed);
                pending.push(OsBlockWrite {
                    start_block: write.offset / BLOCK_SZ,
                    data: write.data,
                });
            } else {
                #[cfg(feature = "io_perf_counters")]
                WRITE_OFFSETS_MANY_UNALIGNED_ITEMS.fetch_add(1, Ordering::Relaxed);
                if !pending.is_empty() {
                    overwrite_block_cache_ranges(Arc::clone(&self.inner), &pending);
                    pending.clear();
                }
                self.write_offset(write.offset, write.data);
            }
        }
        if !pending.is_empty() {
            overwrite_block_cache_ranges(Arc::clone(&self.inner), &pending);
        }
    }
}

#[inline]
fn offset_and_len_are_block_aligned(offset: usize, len: usize) -> bool {
    offset % BLOCK_SZ == 0 && len % BLOCK_SZ == 0
}

#[cfg(feature = "io_perf_counters")]
fn perf_load(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::Relaxed)
}

#[cfg(feature = "io_perf_counters")]
#[inline]
fn perf_now_us() -> Option<usize> {
    let ptr = PERF_TIME_NOW_US.load(Ordering::Relaxed);
    if ptr == 0 {
        None
    } else {
        Some(unsafe { core::mem::transmute::<usize, fn() -> usize>(ptr) }())
    }
}

#[cfg(feature = "io_perf_counters")]
#[inline]
fn perf_elapsed_us(start_us: Option<usize>) -> usize {
    match start_us {
        Some(start_us) => perf_now_us()
            .map(|end_us| end_us.saturating_sub(start_us))
            .unwrap_or(0),
        None => 0,
    }
}

#[cfg(feature = "io_perf_counters")]
fn count_physical_runs(physical_offsets: &[usize]) -> usize {
    if physical_offsets.is_empty() {
        return 0;
    }

    1 + physical_offsets
        .windows(2)
        .filter(|pair| pair[1] != pair[0].saturating_add(BLOCK_SIZE))
        .count()
}

#[cfg(feature = "io_perf_counters")]
pub fn set_perf_time_source(now_us: fn() -> usize) {
    PERF_TIME_NOW_US.store(now_us as usize, Ordering::Relaxed);
}

#[cfg(feature = "io_perf_counters")]
pub fn reset_perf_counters() {
    WRITE_OFFSETS_MANY_CALLS.store(0, Ordering::Relaxed);
    WRITE_OFFSETS_MANY_ITEMS.store(0, Ordering::Relaxed);
    WRITE_OFFSETS_MANY_SINGLE_ITEM_CALLS.store(0, Ordering::Relaxed);
    WRITE_OFFSETS_MANY_ALIGNED_ITEMS.store(0, Ordering::Relaxed);
    WRITE_OFFSETS_MANY_UNALIGNED_ITEMS.store(0, Ordering::Relaxed);
    PAGE_CACHE_READ_PLAN_CALLS.store(0, Ordering::Relaxed);
    PAGE_CACHE_READ_PLAN_HITS.store(0, Ordering::Relaxed);
    PAGE_CACHE_READ_PLAN_FALLBACKS.store(0, Ordering::Relaxed);
    PAGE_CACHE_READ_PLAN_BLOCKS.store(0, Ordering::Relaxed);
    PAGE_CACHE_READ_PLAN_US.store(0, Ordering::Relaxed);
    PAGE_CACHE_READ_IO_US.store(0, Ordering::Relaxed);
    READ_MAPPING_LOCK_CALLS.store(0, Ordering::Relaxed);
    READ_MAPPING_LOCK_WAIT_US.store(0, Ordering::Relaxed);
    READ_MAPPING_LOCK_HOLD_US.store(0, Ordering::Relaxed);
    EXTENT_MAP_CALLS.store(0, Ordering::Relaxed);
    EXTENT_MAP_BLOCKS.store(0, Ordering::Relaxed);
    EXTENT_MAP_RUNS.store(0, Ordering::Relaxed);
    EXTENT_MAP_US.store(0, Ordering::Relaxed);
    EXTENT_MAP_FALLBACKS.store(0, Ordering::Relaxed);
    EXTENT_MAP_ERRORS.store(0, Ordering::Relaxed);
    DIR_LOOKUP_CALLS.store(0, Ordering::Relaxed);
    DIR_LOOKUP_HITS.store(0, Ordering::Relaxed);
    DIR_LOOKUP_MISSES.store(0, Ordering::Relaxed);
    DIR_LOOKUP_BLOCKS_SCANNED.store(0, Ordering::Relaxed);
    DIR_LOOKUP_DIRENTS_SCANNED.store(0, Ordering::Relaxed);
    LS_CALLS.store(0, Ordering::Relaxed);
    LS_ENTRIES.store(0, Ordering::Relaxed);
    GETDENTS_CALLS.store(0, Ordering::Relaxed);
    GETDENTS_NONEMPTY_CALLS.store(0, Ordering::Relaxed);
    GETDENTS_BYTES.store(0, Ordering::Relaxed);
    GETDENTS_BACKEND_US.store(0, Ordering::Relaxed);
    GETDENTS_PRIME_CALLS.store(0, Ordering::Relaxed);
    GETDENTS_PRIME_ENTRIES.store(0, Ordering::Relaxed);
    GETDENTS_PRIME_US.store(0, Ordering::Relaxed);
}

#[cfg(feature = "io_perf_counters")]
pub fn render_perf_counters() -> String {
    let mut out = String::new();
    let dir_lookup_calls = perf_load(&DIR_LOOKUP_CALLS);
    let getdents_calls = perf_load(&GETDENTS_CALLS);
    let getdents_prime_calls = perf_load(&GETDENTS_PRIME_CALLS);
    let ls_calls = perf_load(&LS_CALLS);
    let read_mapping_lock_calls = perf_load(&READ_MAPPING_LOCK_CALLS);
    let extent_map_calls = perf_load(&EXTENT_MAP_CALLS);
    let _ = writeln!(&mut out, "ext4:");
    let _ = writeln!(
        &mut out,
        "  write_offsets_many_calls {}",
        perf_load(&WRITE_OFFSETS_MANY_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  write_offsets_many_items {}",
        perf_load(&WRITE_OFFSETS_MANY_ITEMS)
    );
    let _ = writeln!(
        &mut out,
        "  write_offsets_many_single_item_calls {}",
        perf_load(&WRITE_OFFSETS_MANY_SINGLE_ITEM_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  write_offsets_many_aligned_items {}",
        perf_load(&WRITE_OFFSETS_MANY_ALIGNED_ITEMS)
    );
    let _ = writeln!(
        &mut out,
        "  write_offsets_many_unaligned_items {}",
        perf_load(&WRITE_OFFSETS_MANY_UNALIGNED_ITEMS)
    );
    let _ = writeln!(
        &mut out,
        "  page_cache_read_plan_calls {}",
        perf_load(&PAGE_CACHE_READ_PLAN_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  page_cache_read_plan_hits {}",
        perf_load(&PAGE_CACHE_READ_PLAN_HITS)
    );
    let _ = writeln!(
        &mut out,
        "  page_cache_read_plan_fallbacks {}",
        perf_load(&PAGE_CACHE_READ_PLAN_FALLBACKS)
    );
    let _ = writeln!(
        &mut out,
        "  page_cache_read_plan_blocks {}",
        perf_load(&PAGE_CACHE_READ_PLAN_BLOCKS)
    );
    let _ = writeln!(
        &mut out,
        "  page_cache_read_plan_us {}",
        perf_load(&PAGE_CACHE_READ_PLAN_US)
    );
    let _ = writeln!(
        &mut out,
        "  page_cache_read_io_us {}",
        perf_load(&PAGE_CACHE_READ_IO_US)
    );
    let _ = writeln!(
        &mut out,
        "  read_mapping_lock_calls {}",
        read_mapping_lock_calls
    );
    let _ = writeln!(
        &mut out,
        "  read_mapping_lock_wait_us {}",
        perf_load(&READ_MAPPING_LOCK_WAIT_US)
    );
    let _ = writeln!(
        &mut out,
        "  read_mapping_lock_hold_us {}",
        perf_load(&READ_MAPPING_LOCK_HOLD_US)
    );
    let _ = writeln!(
        &mut out,
        "  avg_read_mapping_lock_wait_us_x100 {}",
        if read_mapping_lock_calls == 0 {
            0
        } else {
            perf_load(&READ_MAPPING_LOCK_WAIT_US).saturating_mul(100)
                / read_mapping_lock_calls
        }
    );
    let _ = writeln!(
        &mut out,
        "  avg_read_mapping_lock_hold_us_x100 {}",
        if read_mapping_lock_calls == 0 {
            0
        } else {
            perf_load(&READ_MAPPING_LOCK_HOLD_US).saturating_mul(100)
                / read_mapping_lock_calls
        }
    );
    let _ = writeln!(&mut out, "  extent_map_calls {}", extent_map_calls);
    let _ = writeln!(
        &mut out,
        "  extent_map_us {}",
        perf_load(&EXTENT_MAP_US)
    );
    let _ = writeln!(
        &mut out,
        "  extent_map_blocks {}",
        perf_load(&EXTENT_MAP_BLOCKS)
    );
    let _ = writeln!(&mut out, "  extent_map_runs {}", perf_load(&EXTENT_MAP_RUNS));
    let _ = writeln!(
        &mut out,
        "  extent_map_fallbacks {}",
        perf_load(&EXTENT_MAP_FALLBACKS)
    );
    let _ = writeln!(
        &mut out,
        "  extent_map_errors {}",
        perf_load(&EXTENT_MAP_ERRORS)
    );
    let _ = writeln!(
        &mut out,
        "  avg_extent_map_us_x100 {}",
        if extent_map_calls == 0 {
            0
        } else {
            perf_load(&EXTENT_MAP_US).saturating_mul(100) / extent_map_calls
        }
    );
    let _ = writeln!(
        &mut out,
        "  avg_extent_map_blocks_x100 {}",
        if extent_map_calls == 0 {
            0
        } else {
            perf_load(&EXTENT_MAP_BLOCKS).saturating_mul(100) / extent_map_calls
        }
    );
    let _ = writeln!(
        &mut out,
        "  avg_extent_map_runs_x100 {}",
        if extent_map_calls == 0 {
            0
        } else {
            perf_load(&EXTENT_MAP_RUNS).saturating_mul(100) / extent_map_calls
        }
    );
    let _ = writeln!(&mut out, "  getdents_calls {}", getdents_calls);
    let _ = writeln!(
        &mut out,
        "  getdents_nonempty_calls {}",
        perf_load(&GETDENTS_NONEMPTY_CALLS)
    );
    let _ = writeln!(&mut out, "  getdents_bytes {}", perf_load(&GETDENTS_BYTES));
    let _ = writeln!(
        &mut out,
        "  getdents_backend_us {}",
        perf_load(&GETDENTS_BACKEND_US)
    );
    let _ = writeln!(
        &mut out,
        "  avg_getdents_backend_us_x100 {}",
        if getdents_calls == 0 {
            0
        } else {
            perf_load(&GETDENTS_BACKEND_US).saturating_mul(100) / getdents_calls
        }
    );
    let _ = writeln!(
        &mut out,
        "  getdents_prime_calls {}",
        getdents_prime_calls
    );
    let _ = writeln!(
        &mut out,
        "  getdents_prime_entries {}",
        perf_load(&GETDENTS_PRIME_ENTRIES)
    );
    let _ = writeln!(
        &mut out,
        "  getdents_prime_us {}",
        perf_load(&GETDENTS_PRIME_US)
    );
    let _ = writeln!(
        &mut out,
        "  avg_getdents_prime_us_x100 {}",
        if getdents_prime_calls == 0 {
            0
        } else {
            perf_load(&GETDENTS_PRIME_US).saturating_mul(100) / getdents_prime_calls
        }
    );
    let _ = writeln!(&mut out, "  dir_lookup_calls {}", dir_lookup_calls);
    let _ = writeln!(&mut out, "  dir_lookup_hits {}", perf_load(&DIR_LOOKUP_HITS));
    let _ = writeln!(&mut out, "  dir_lookup_misses {}", perf_load(&DIR_LOOKUP_MISSES));
    let _ = writeln!(
        &mut out,
        "  dir_lookup_blocks_scanned {}",
        perf_load(&DIR_LOOKUP_BLOCKS_SCANNED)
    );
    let _ = writeln!(
        &mut out,
        "  dir_lookup_dirents_scanned {}",
        perf_load(&DIR_LOOKUP_DIRENTS_SCANNED)
    );
    let _ = writeln!(
        &mut out,
        "  avg_dir_lookup_blocks_x100 {}",
        if dir_lookup_calls == 0 {
            0
        } else {
            perf_load(&DIR_LOOKUP_BLOCKS_SCANNED).saturating_mul(100) / dir_lookup_calls
        }
    );
    let _ = writeln!(
        &mut out,
        "  avg_dir_lookup_dirents_x100 {}",
        if dir_lookup_calls == 0 {
            0
        } else {
            perf_load(&DIR_LOOKUP_DIRENTS_SCANNED).saturating_mul(100) / dir_lookup_calls
        }
    );
    let _ = writeln!(&mut out, "  ls_calls {}", ls_calls);
    let _ = writeln!(&mut out, "  ls_entries {}", perf_load(&LS_ENTRIES));
    let _ = writeln!(
        &mut out,
        "  avg_ls_entries_x100 {}",
        if ls_calls == 0 {
            0
        } else {
            perf_load(&LS_ENTRIES).saturating_mul(100) / ls_calls
        }
    );
    out
}

pub struct Ext4FileSystem {
    ext4: Mutex<Ext4>,
    /// Canonical per-inode mapping locks.  The weak table makes separately
    /// constructed VFS wrappers for one inode coordinate without retaining
    /// every inode ever observed by the filesystem.
    inode_mapping_locks: Mutex<BTreeMap<u32, Weak<Mutex<()>>>>,
}

impl Ext4FileSystem {
    pub fn open(block_device: Arc<dyn OsBlockDevice>) -> Arc<Self> {
        let ext4_dev: Arc<dyn Ext4BlockDevice> = Arc::new(Ext4BlockDeviceAdapter::new(block_device));
        let ext4 = Ext4::open(ext4_dev);
        Arc::new(Self {
            ext4: Mutex::new(ext4),
            inode_mapping_locks: Mutex::new(BTreeMap::new()),
        })
    }

    fn inode_mapping_lock(&self, inode_num: u32) -> Arc<Mutex<()>> {
        let mut locks = self.inode_mapping_locks.lock();
        if let Some(lock) = locks.get(&inode_num).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(inode_num, Arc::downgrade(&lock));
        lock
    }

    /// 返回根目录对应的稳定内存 inode。
    pub fn root_inode(fs: &Arc<Self>) -> Arc<Inode> {
        Inode::from_vfs_node(Arc::new(Ext4Inode::new(Arc::clone(fs), EXT4_ROOT_INODE, true)))
    }
}

pub struct Ext4Inode {
    fs: Arc<Ext4FileSystem>,
    inode_num: u32,
    file_type: VfsFileType,
    /// Stabilizes this inode's extent mapping while prepared data I/O is in
    /// flight.  This is deliberately narrower than the filesystem-wide ext4
    /// metadata lock.
    mapping_lock: Arc<Mutex<()>>,
}

impl Ext4Inode {
    /// 创建新的 ext4 内存 inode 包装对象。
    fn new(fs: Arc<Ext4FileSystem>, inode_num: u32, is_dir: bool) -> Self {
        let mapping_lock = fs.inode_mapping_lock(inode_num);
        Self {
            fs,
            inode_num,
            file_type: if is_dir { VfsFileType::Directory } else { VfsFileType::Regular },
            mapping_lock,
        }
    }

    fn new_with_type(fs: Arc<Ext4FileSystem>, inode_num: u32, file_type: VfsFileType) -> Self {
        let mapping_lock = fs.inode_mapping_lock(inode_num);
        Self {
            fs,
            inode_num,
            file_type,
            mapping_lock,
        }
    }

    fn inode_file_type(ext4: &Ext4, inode_num: u32) -> VfsFileType {
        let inode_ref = ext4.get_inode_ref(inode_num);
        match inode_ref.inode.file_type() {
            InodeFileType::S_IFDIR => VfsFileType::Directory,
            InodeFileType::S_IFLNK => VfsFileType::Symlink,
            InodeFileType::S_IFCHR => VfsFileType::Char,
            InodeFileType::S_IFBLK => VfsFileType::Block,
            InodeFileType::S_IFIFO => VfsFileType::Fifo,
            InodeFileType::S_IFSOCK => VfsFileType::Socket,
            InodeFileType::S_IFREG => VfsFileType::Regular,
            _ => VfsFileType::Unknown,
        }
    }

    fn dirent_file_type(de_type: u8) -> VfsFileType {
        match de_type {
            2 => VfsFileType::Directory,
            7 => VfsFileType::Symlink,
            3 => VfsFileType::Char,
            4 => VfsFileType::Block,
            5 => VfsFileType::Fifo,
            6 => VfsFileType::Socket,
            1 => VfsFileType::Regular,
            _ => VfsFileType::Unknown,
        }
    }

    fn linux_dirent_file_type(dtype: u8) -> VfsFileType {
        match dtype {
            4 => VfsFileType::Directory,
            10 => VfsFileType::Symlink,
            2 => VfsFileType::Char,
            6 => VfsFileType::Block,
            1 => VfsFileType::Fifo,
            12 => VfsFileType::Socket,
            8 => VfsFileType::Regular,
            _ => VfsFileType::Unknown,
        }
    }

    fn cache_dir_entry(&self, name: &str, inode_num: u32, file_type: VfsFileType) {
        if name == "." || name == ".." {
            return;
        }

        let child = Inode::from_vfs_node(
            Arc::new(Self::new_with_type(Arc::clone(&self.fs), inode_num, file_type))
                as Arc<dyn VfsNode>,
        );
        insert_dentry(self.fs_id(), self.ino(), name, &child);
    }

    fn prime_dentry_cache_from_dirents(&self, ext4: &Ext4, buf: &[u8]) -> usize {
        let mut cursor = 0usize;
        let mut primed_entries = 0usize;
        while cursor + 19 <= buf.len() {
            let reclen = u16::from_le_bytes([buf[cursor + 16], buf[cursor + 17]]) as usize;
            if reclen == 0 || cursor + reclen > buf.len() {
                break;
            }
            let name_end = buf[cursor + 19..cursor + reclen]
                .iter()
                .position(|&b| b == 0)
                .map(|idx| cursor + 19 + idx)
                .unwrap_or(cursor + reclen);
            if name_end == cursor + 19 {
                cursor += reclen;
                continue;
            }

            let ino = u64::from_le_bytes([
                buf[cursor],
                buf[cursor + 1],
                buf[cursor + 2],
                buf[cursor + 3],
                buf[cursor + 4],
                buf[cursor + 5],
                buf[cursor + 6],
                buf[cursor + 7],
            ]);
            let Ok(name) = core::str::from_utf8(&buf[cursor + 19..name_end]) else {
                cursor += reclen;
                continue;
            };
            if ino == 0 {
                cursor += reclen;
                continue;
            }

            let mut file_type = Self::linux_dirent_file_type(buf[cursor + 18]);
            if file_type == VfsFileType::Unknown {
                file_type = Self::inode_file_type(ext4, ino as u32);
            }
            self.cache_dir_entry(name, ino as u32, file_type);
            primed_entries += 1;
            cursor += reclen;
        }
        primed_entries
    }

    fn ext4_getdents64(&self, offset: usize, buf: &mut [u8]) -> usize {
        if self.file_type != VfsFileType::Directory {
            return 0;
        }

        let ext4 = self.fs.ext4.lock();
        #[cfg(feature = "io_perf_counters")]
        GETDENTS_CALLS.fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "io_perf_counters")]
        let backend_start_us = perf_now_us();
        let written = ext4.ext4_dir_getdents64(self.inode_num, offset, buf);
        #[cfg(feature = "io_perf_counters")]
        let backend_us = perf_elapsed_us(backend_start_us);
        #[cfg(feature = "io_perf_counters")]
        {
            GETDENTS_BACKEND_US.fetch_add(backend_us, Ordering::Relaxed);
            GETDENTS_BYTES.fetch_add(written, Ordering::Relaxed);
            if written != 0 {
                GETDENTS_NONEMPTY_CALLS.fetch_add(1, Ordering::Relaxed);
            }
        }
        if written != 0 {
            #[cfg(feature = "io_perf_counters")]
            let prime_start_us = perf_now_us();
            let primed_entries = self.prime_dentry_cache_from_dirents(&ext4, &buf[..written]);
            #[cfg(feature = "io_perf_counters")]
            {
                GETDENTS_PRIME_CALLS.fetch_add(1, Ordering::Relaxed);
                GETDENTS_PRIME_ENTRIES.fetch_add(primed_entries, Ordering::Relaxed);
                GETDENTS_PRIME_US.fetch_add(perf_elapsed_us(prime_start_us), Ordering::Relaxed);
            }
        }
        written
    }

    /// 查询目录项元数据，返回 `(inode 编号, 文件类型)`。
    fn lookup_child_meta(&self, name: &str) -> Option<(u32, VfsFileType)> {
        if self.file_type != VfsFileType::Directory {
            return None;
        }
        #[cfg(feature = "io_perf_counters")]
        DIR_LOOKUP_CALLS.fetch_add(1, Ordering::Relaxed);
        let ext4 = self.fs.ext4.lock();
        let result = ext4.ext4_dir_lookup_with_stats(self.inode_num, name);
        #[cfg(feature = "io_perf_counters")]
        match result.as_ref() {
            Some((_inode_num, _de_type, blocks_scanned, dirents_scanned)) => {
                DIR_LOOKUP_HITS.fetch_add(1, Ordering::Relaxed);
                DIR_LOOKUP_BLOCKS_SCANNED.fetch_add(*blocks_scanned, Ordering::Relaxed);
                DIR_LOOKUP_DIRENTS_SCANNED.fetch_add(*dirents_scanned, Ordering::Relaxed);
            }
            None => {
                DIR_LOOKUP_MISSES.fetch_add(1, Ordering::Relaxed);
            }
        }
        result.map(|(inode_num, _de_type, _blocks_scanned, _dirents_scanned)| {
            // Trust the inode's mode bits over the directory entry type.
            // A stale/corrupt d_type can otherwise turn a non-directory inode
            // into a cached "directory" and later panic inside ext4 dir helpers.
            let file_type = Self::inode_file_type(&ext4, inode_num);
            (inode_num, file_type)
        })
    }

    /// 在 ext4 后端内实现完整 truncate 语义。
    fn truncate_file(&self, new_size: usize) -> Result<(), FS_ERRNO> {
        if self.file_type == VfsFileType::Directory {
            return Err(FS_ERRNO::EISDIR);
        }

        // Acquire the inode-local mapping lock before the global ext4 lock.
        // Prepared reads retain this guard while their data I/O is in flight,
        // so truncate cannot free and reassign a resolved physical block.
        let _mapping_guard = self.mapping_lock.lock();
        let ext4 = self.fs.ext4.lock();
        let old_size = ext4.get_inode_ref(self.inode_num).inode.size() as usize;
        debug!(
            "Ext4Inode truncate: ino={} old_size={} new_size={}",
            self.inode_num,
            old_size,
            new_size
        );
        if old_size == new_size {
            return Ok(());
        }

        let zero_block = [0u8; BLOCK_SIZE];

        if new_size < old_size {
            let tail_off = new_size % BLOCK_SIZE;
            if new_size > 0 && tail_off != 0 {
                let zero_len = BLOCK_SIZE - tail_off;
                // 先把保留尾块的新 EOF 之后部分清零，避免未来再次扩容时旧数据重新可见。
                debug!(
                    "Ext4Inode truncate shrink tail: ino={} zero_from={} zero_len={}",
                    self.inode_num,
                    new_size,
                    zero_len
                );
                ext4.write_at(self.inode_num, new_size, &zero_block[..zero_len])?;
            }

            let mut inode_ref = ext4.get_inode_ref(self.inode_num);
            let block_size = BLOCK_SIZE as u64;
            let new_blocks = (new_size as u64).div_ceil(block_size);
            let old_blocks = (old_size as u64).div_ceil(block_size);
            if old_blocks > new_blocks {
                debug!(
                    "Ext4Inode truncate shrink blocks: ino={} old_blocks={} new_blocks={}",
                    self.inode_num,
                    old_blocks,
                    new_blocks
                );
                ext4.extent_remove_space(&mut inode_ref, new_blocks as u32, u32::MAX)?;
            }
            inode_ref.inode.set_size(new_size as u64);
            ext4.write_back_inode(&mut inode_ref);
            return Ok(());
        }

        // TODO：当前扩容采用显式补零，语义完整但不是稀疏文件实现，后续可按需优化。
        let mut cursor = old_size;
        while cursor < new_size {
            let chunk_len = min(BLOCK_SIZE, new_size - cursor);
            debug!(
                "Ext4Inode truncate grow chunk: ino={} off={} len={}",
                self.inode_num,
                cursor,
                chunk_len
            );
            ext4.write_at(self.inode_num, cursor, &zero_block[..chunk_len])?;
            cursor += chunk_len;
        }

        let mut inode_ref = ext4.get_inode_ref(self.inode_num);
        inode_ref.inode.set_size(new_size as u64);
        ext4.write_back_inode(&mut inode_ref);
        Ok(())
    }

    /// 将目录项从当前目录重命名到新父目录。
    fn rename_child_to(&self, old_name: &str, new_parent: &Self, new_name: &str) -> Result<(), FS_ERRNO> {
        if self.file_type != VfsFileType::Directory || new_parent.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        if !Arc::ptr_eq(&self.fs, &new_parent.fs) {
            return Err(FS_ERRNO::EXDEV);
        }

        loop {
            // Replacing an existing target may truncate and free its extent
            // tree. Resolve its inode first, acquire that inode's mapping
            // lock, then revalidate under the ext4 lock before committing.
            let expected_target_ino = {
                let ext4 = self.fs.ext4.lock();
                ext4
                    .ext4_dir_get_entries(new_parent.inode_num)
                    .into_iter()
                    .find(|de| de.get_name() == new_name)
                    .map(|de| de.inode)
            };
            let target_mapping_lock = expected_target_ino
                .map(|inode_num| self.fs.inode_mapping_lock(inode_num));
            let _target_mapping_guard =
                target_mapping_lock.as_ref().map(|lock| lock.lock());

            let ext4 = self.fs.ext4.lock();
            let old_entry = ext4
                .ext4_dir_get_entries(self.inode_num)
                .into_iter()
                .find(|de| de.get_name() == old_name)
                .ok_or(FS_ERRNO::ENOENT)?;
            let child_ino = old_entry.inode;
            let child_ref = ext4.get_inode_ref(child_ino);
            let child_is_dir = child_ref.inode.is_dir();
            let target_entry = ext4
                .ext4_dir_get_entries(new_parent.inode_num)
                .into_iter()
                .find(|de| de.get_name() == new_name);
            if target_entry.as_ref().map(|entry| entry.inode) != expected_target_ino {
                drop(ext4);
                continue;
            }

            if let Some(target_entry) = target_entry {
                let target_ino = target_entry.inode;
                let target_ref = ext4.get_inode_ref(target_ino);
                let target_is_dir = target_ref.inode.is_dir();
                if child_ino == target_ino {
                    return Ok(());
                }
                if child_is_dir && !target_is_dir {
                    return Err(FS_ERRNO::ENOTDIR);
                }
                if !child_is_dir && target_is_dir {
                    return Err(FS_ERRNO::EISDIR);
                }
                if target_is_dir && ext4.dir_has_entry(target_ino) {
                    return Err(FS_ERRNO::ENOTEMPTY);
                }
            }

            ext4.rename_entry(self.inode_num, old_name, new_parent.inode_num, new_name)?;
            return Ok(());
        }
    }
}

impl fmt::Debug for Ext4Inode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ext4Inode")
            .field("inode_num", &self.inode_num)
            .field("file_type", &self.file_type)
            .field("fs_ptr", &format_args!("{:p}", Arc::as_ptr(&self.fs)))
            .finish()
    }
}

impl VfsNode for Ext4Inode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        if self.file_type != VfsFileType::Directory {
            return Vec::new();
        }
        #[cfg(feature = "io_perf_counters")]
        LS_CALLS.fetch_add(1, Ordering::Relaxed);
        let ext4 = self.fs.ext4.lock();
        let entries = ext4
            .ext4_dir_get_entries(self.inode_num)
            .into_iter()
            .map(|de| {
                let inode_num = de.inode;
                let name = de.get_name();
                let dirent_type = Self::dirent_file_type(de.get_de_type());
                let file_type = if dirent_type == VfsFileType::Unknown {
                    Self::inode_file_type(&ext4, inode_num)
                } else {
                    dirent_type
                };
                self.cache_dir_entry(name.as_str(), inode_num, file_type);
                (name, file_type)
            })
            .collect::<Vec<_>>();
        #[cfg(feature = "io_perf_counters")]
        LS_ENTRIES.fetch_add(entries.len(), Ordering::Relaxed);
        entries
    }

    fn getdents64(&self, offset: usize, buf: &mut [u8]) -> usize {
        self.ext4_getdents64(offset, buf)
    }

    fn prefer_native_getdents64(&self) -> bool {
        true
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let (inode_num, file_type) = self.lookup_child_meta(name)?;
        Some(Arc::new(Self::new_with_type(Arc::clone(&self.fs), inode_num, file_type)) as Arc<dyn VfsNode>)
    }

    fn create(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        self.create_result(name).ok()
    }

    fn create_result(&self, name: &str) -> Result<Arc<dyn VfsNode>, FS_ERRNO> {
        if self.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        let ext4 = self.fs.ext4.lock();
        let inode = ext4
            .create(self.inode_num, name, InodeFileType::S_IFREG.bits())
            .map_err(FS_ERRNO::from)?;
        Ok(Arc::new(Self::new_with_type(Arc::clone(&self.fs), inode.inode_num, VfsFileType::Regular)) as Arc<dyn VfsNode>)
    }

    fn mkdir(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        self.mkdir_result(name).ok()
    }

    fn mkdir_result(&self, name: &str) -> Result<Arc<dyn VfsNode>, FS_ERRNO> {
        if self.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        let ext4 = self.fs.ext4.lock();
        let inode = ext4
            .create(self.inode_num, name, InodeFileType::S_IFDIR.bits())
            .map_err(FS_ERRNO::from)?;
        Ok(Arc::new(Self::new_with_type(Arc::clone(&self.fs), inode.inode_num, VfsFileType::Directory)) as Arc<dyn VfsNode>)
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn VfsNode>, FS_ERRNO> {
        if self.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        let ext4 = self.fs.ext4.lock();
        let inode = ext4
            .create(self.inode_num, name, InodeFileType::S_IFLNK.bits() | 0o777)
            .map_err(FS_ERRNO::from)?;
        ext4.write_at(inode.inode_num, 0, target.as_bytes()).map_err(FS_ERRNO::from)?;
        Ok(Arc::new(Self::new_with_type(
            Arc::clone(&self.fs),
            inode.inode_num,
            VfsFileType::Symlink,
        )) as Arc<dyn VfsNode>)
    }

    fn file_type(&self) -> VfsFileType {
        self.file_type
    }

    fn read_link(&self) -> Result<String, FS_ERRNO> {
        if self.file_type != VfsFileType::Symlink {
            return Err(FS_ERRNO::EINVAL);
        }
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        let size = inode_ref.inode.size() as usize;

        // ext4 fast symlink stores the link target inline in inode.i_block.
        // Treating it as regular file data would wrongly parse extents and panic.
        if size <= core::mem::size_of_val(&inode_ref.inode.block) && inode_ref.inode.blocks_count() == 0 {
            let mut buf = Vec::with_capacity(core::mem::size_of_val(&inode_ref.inode.block));
            for word in inode_ref.inode.block() {
                buf.extend_from_slice(&word.to_le_bytes());
            }
            buf.truncate(size);
            return String::from_utf8(buf).map_err(|_| FS_ERRNO::EINVAL);
        }

        let mut buf = vec![0u8; size];
        let read = ext4.read_at(self.inode_num, 0, &mut buf).map_err(FS_ERRNO::from)?;
        buf.truncate(read);
        String::from_utf8(buf).map_err(|_| FS_ERRNO::EINVAL)
    }

    fn stat_attrs(&self) -> VfsAttrs {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        let i = &inode_ref.inode;
        VfsAttrs {
            mode: Some(i.mode() as u32),
            ino: self.inode_num as u64,
            nlink: i.links_count() as u32,
            size: i.size() as usize,
            uid: Some(i.uid() as u32),
            gid: Some(i.gid() as u32),
            rdev: 0,
            atime: Some(decode_ext4_time(i.atime(), i.i_atime_extra())),
            mtime: Some(decode_ext4_time(i.mtime(), i.i_mtime_extra())),
            ctime: Some(decode_ext4_time(i.ctime(), i.i_ctime_extra())),
        }
    }

    fn statfs(&self) -> Result<VfsStatFs, FS_ERRNO> {
        let ext4 = self.fs.ext4.lock();
        let sb = ext4.super_block;
        Ok(VfsStatFs {
            f_type: STATFS_MAGIC_EXT4,
            f_bsize: sb.block_size() as u64,
            f_blocks: sb.blocks_count() as u64,
            f_bfree: sb.free_blocks_count(),
            f_bavail: sb.free_blocks_count(),
            f_files: sb.total_inodes() as u64,
            f_ffree: sb.free_inodes_count() as u64,
            f_fsid: [
                (Arc::as_ptr(&self.fs) as usize as u32) as i32,
                ((Arc::as_ptr(&self.fs) as usize as u64 >> 32) as u32) as i32,
            ],
            f_namelen: STATFS_NAMELEN_DEFAULT,
            f_frsize: sb.block_size() as u64,
            f_flags: 0,
            f_spare: [0; 4],
        })
    }

    fn clear(&self) {
        let _ = self.truncate_file(0);
    }

    fn truncate(&self, new_size: usize) -> Result<(), FS_ERRNO> {
        self.truncate_file(new_size)
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let ext4 = self.fs.ext4.lock();
        ext4.read_at(self.inode_num, offset, buf).unwrap_or(0)
    }

    fn read_at_page_cache(&self, offset: usize, buf: &mut [u8]) -> usize {
        // The inode-local guard stabilizes the physical mapping after the
        // global ext4 metadata lock is released.  This first implementation
        // serializes reads of one inode, but no longer serializes unrelated
        // inode and directory operations behind data-device latency.
        #[cfg(feature = "io_perf_counters")]
        let mapping_lock_wait_start_us = perf_now_us();
        let mapping_guard = self.mapping_lock.lock();
        #[cfg(feature = "io_perf_counters")]
        {
            READ_MAPPING_LOCK_CALLS.fetch_add(1, Ordering::Relaxed);
            READ_MAPPING_LOCK_WAIT_US.fetch_add(
                perf_elapsed_us(mapping_lock_wait_start_us),
                Ordering::Relaxed,
            );
        }
        #[cfg(feature = "io_perf_counters")]
        let mapping_lock_hold_start_us = perf_now_us();
        let result = (|| {
            #[cfg(feature = "io_perf_counters")]
            PAGE_CACHE_READ_PLAN_CALLS.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "io_perf_counters")]
            let plan_start_us = perf_now_us();
            let prepared = {
                let ext4 = self.fs.ext4.lock();
                #[cfg(feature = "io_perf_counters")]
                let extent_map_start_us = perf_now_us();
                let extent_map_result =
                    ext4.prepare_aligned_read_at(self.inode_num, offset, buf.len());
                #[cfg(feature = "io_perf_counters")]
                {
                    EXTENT_MAP_CALLS.fetch_add(1, Ordering::Relaxed);
                    EXTENT_MAP_US.fetch_add(
                        perf_elapsed_us(extent_map_start_us),
                        Ordering::Relaxed,
                    );
                }
                match extent_map_result {
                    Ok(Some((read_len, physical_offsets))) => Some((
                        Arc::clone(&ext4.block_device),
                        read_len,
                        {
                            #[cfg(feature = "io_perf_counters")]
                            {
                                EXTENT_MAP_BLOCKS.fetch_add(
                                    physical_offsets.len(),
                                    Ordering::Relaxed,
                                );
                                EXTENT_MAP_RUNS.fetch_add(
                                    count_physical_runs(&physical_offsets),
                                    Ordering::Relaxed,
                                );
                            }
                            physical_offsets
                        },
                    )),
                    Ok(None) => {
                        #[cfg(feature = "io_perf_counters")]
                        EXTENT_MAP_FALLBACKS.fetch_add(1, Ordering::Relaxed);
                        None
                    }
                    Err(_) => {
                        #[cfg(feature = "io_perf_counters")]
                        EXTENT_MAP_ERRORS.fetch_add(1, Ordering::Relaxed);
                        return 0;
                    }
                }
            };
            #[cfg(feature = "io_perf_counters")]
            PAGE_CACHE_READ_PLAN_US.fetch_add(perf_elapsed_us(plan_start_us), Ordering::Relaxed);

            if let Some((block_device, read_len, physical_offsets)) = prepared {
                #[cfg(feature = "io_perf_counters")]
                {
                    PAGE_CACHE_READ_PLAN_HITS.fetch_add(1, Ordering::Relaxed);
                    PAGE_CACHE_READ_PLAN_BLOCKS
                        .fetch_add(physical_offsets.len(), Ordering::Relaxed);
                }
                #[cfg(feature = "io_perf_counters")]
                let io_start_us = perf_now_us();
                block_device.read_offsets_uncached(&physical_offsets, &mut buf[..read_len]);
                #[cfg(feature = "io_perf_counters")]
                PAGE_CACHE_READ_IO_US.fetch_add(perf_elapsed_us(io_start_us), Ordering::Relaxed);
                return read_len;
            }

            #[cfg(feature = "io_perf_counters")]
            PAGE_CACHE_READ_PLAN_FALLBACKS.fetch_add(1, Ordering::Relaxed);
            let ext4 = self.fs.ext4.lock();
            ext4.read_at_uncached(self.inode_num, offset, buf).unwrap_or(0)
        })();
        drop(mapping_guard);
        #[cfg(feature = "io_perf_counters")]
        READ_MAPPING_LOCK_HOLD_US.fetch_add(
            perf_elapsed_us(mapping_lock_hold_start_us),
            Ordering::Relaxed,
        );
        result
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let _mapping_guard = self.mapping_lock.lock();
        let ext4 = self.fs.ext4.lock();
        ext4.write_at(self.inode_num, offset, buf).unwrap_or(0)
    }

    /// ext4 写入需要保留 ENOSPC/ENOTSUP，供 page cache 回写路径处理失败页。
    fn write_at_result(&self, offset: usize, buf: &[u8]) -> Result<usize, FS_ERRNO> {
        // Keep the mapping stable between write-plan preparation and device
        // submission for the same reason as the prepared read path.
        let _mapping_guard = self.mapping_lock.lock();
        let prepared = {
            let ext4 = self.fs.ext4.lock();
            let block_device = Arc::clone(&ext4.block_device);
            match ext4
                .prepare_aligned_write_at(self.inode_num, offset, buf.len())
                .map_err(FS_ERRNO::from)?
            {
                Some((written, runs)) => Some((block_device, written, runs)),
                None => None,
            }
        };

        if let Some((block_device, written, runs)) = prepared {
            let mut writes = Vec::new();
            for (disk_offset, buf_offset, len) in runs {
                writes.push(Ext4BlockWrite {
                    offset: disk_offset,
                    data: &buf[buf_offset..buf_offset + len],
                });
            }
            block_device.write_offsets_many(&writes);
            return Ok(written);
        }

        let ext4 = self.fs.ext4.lock();
        ext4.write_at(self.inode_num, offset, buf).map_err(FS_ERRNO::from)
    }

    fn ino(&self) -> u64 {
        self.inode_num as u64
    }

    fn fs_id(&self) -> u64 {
        Arc::as_ptr(&self.fs) as usize as u64
    }

    fn nlink(&self) -> u32 {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        inode_ref.inode.links_count() as u32
    }

    fn size(&self) -> usize {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        inode_ref.inode.size() as usize
    }

    fn mode(&self) -> Option<u32> {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        Some(inode_ref.inode.mode() as u32)
    }

    fn uid(&self) -> Option<u32> {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        Some(inode_ref.inode.uid() as u32)
    }

    fn gid(&self) -> Option<u32> {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        Some(inode_ref.inode.gid() as u32)
    }

    fn set_mode(&self, mode: u32) -> Result<(), FS_ERRNO> {
        info!("Ext4Inode set_mode: ino={} mode={:#o}", self.inode_num, mode);
        let ext4 = self.fs.ext4.lock();
        let mut inode_ref = ext4.get_inode_ref(self.inode_num);
        inode_ref.inode.set_mode(mode as u16);
        ext4.write_back_inode(&mut inode_ref);
        Ok(())
    }

    fn set_owner(&self, uid: u32, gid: u32) -> Result<(), FS_ERRNO> {
        if uid > u16::MAX as u32 || gid > u16::MAX as u32 {
            return Err(FS_ERRNO::EOVERFLOW);
        }
        let ext4 = self.fs.ext4.lock();
        let mut inode_ref = ext4.get_inode_ref(self.inode_num);
        inode_ref.inode.set_uid(uid as u16);
        inode_ref.inode.set_gid(gid as u16);
        ext4.write_back_inode(&mut inode_ref);
        Ok(())
    }

    fn check_access(&self, uid: u32, gid: u32, mode: u32) -> bool {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        inode_ref
            .inode
            .check_access(uid as u16, gid as u16, mode as u16, 0)
    }

    fn link(&self, _old_name: &str, _new_name: &str) -> Result<(), FS_ERRNO> {
        if self.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        let (child_ino, _) = self.lookup_child_meta(_old_name).ok_or(FS_ERRNO::ENOENT)?;
        let ext4 = self.fs.ext4.lock();
        let mut parent_ref = ext4.get_inode_ref(self.inode_num);
        let mut child_ref = ext4.get_inode_ref(child_ino);
        ext4.link(&mut parent_ref, &mut child_ref, _new_name)?;
        // Persist updated link counts/dir entries.
        ext4.write_back_inode(&mut parent_ref);
        ext4.write_back_inode(&mut child_ref);
        Ok(())
    }

    fn link_inode(&self, child: &Arc<dyn VfsNode>, new_name: &str) -> Result<(), FS_ERRNO> {
        if self.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        let child = child.as_any().downcast_ref::<Self>().ok_or(FS_ERRNO::EINVAL)?;
        if child.file_type == VfsFileType::Directory {
            return Err(FS_ERRNO::EISDIR);
        }
        let ext4 = self.fs.ext4.lock();
        let mut parent_ref = ext4.get_inode_ref(self.inode_num);
        let mut child_ref = ext4.get_inode_ref(child.inode_num);
        ext4.link(&mut parent_ref, &mut child_ref, new_name)?;
        ext4.write_back_inode(&mut parent_ref);
        ext4.write_back_inode(&mut child_ref);
        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), FS_ERRNO> {
        if self.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        debug!("Ext4Inode unlink: parent_inode={}, name='{}'", self.inode_num, name);
        loop {
            let (child_ino, child_type) =
                self.lookup_child_meta(name).ok_or(FS_ERRNO::ENOENT)?;
            if child_type == VfsFileType::Directory {
                return Err(FS_ERRNO::EISDIR);
            }
            let child_mapping_lock = self.fs.inode_mapping_lock(child_ino);
            let _child_mapping_guard = child_mapping_lock.lock();
            let ext4 = self.fs.ext4.lock();
            let locked_child_ino = ext4
                .ext4_dir_lookup_with_stats(self.inode_num, name)
                .map(|(inode_num, _, _, _)| inode_num);
            if locked_child_ino != Some(child_ino) {
                drop(ext4);
                continue;
            }

            let mut parent_ref = ext4.get_inode_ref(self.inode_num);
            let mut child_ref = ext4.get_inode_ref(child_ino);
            // Hard-link case: remove only this directory entry and decrement nlink.
            if child_ref.inode.links_count() > 1 {
                ext4.dir_remove_entry(&mut parent_ref, name)?;
                let new_links = child_ref.inode.links_count() - 1;
                child_ref.inode.set_links_count(new_links);
                ext4.write_back_inode(&mut parent_ref);
                ext4.write_back_inode(&mut child_ref);
                log::debug!("Ext4Inode unlink: removed link '{}', new links_count={}", name, new_links);
                return Ok(());
            }
            // Normal case: remove directory entry, decrement nlink, and truncate if this is the last link.
            if child_ref.inode.links_count() == 1 {
                ext4.truncate_inode(&mut child_ref, 0)?;
                log::debug!("Ext4Inode unlink: truncated inode {} to 0 length", child_ino);
            }
            ext4.unlink(&mut parent_ref, &mut child_ref, name)
                .map(|_| ())?;
            return Ok(());
        }
    }

    fn rmdir(&self, name: &str) -> Result<(), FS_ERRNO> {
        if self.file_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        let (_, child_type) = self.lookup_child_meta(name).ok_or(FS_ERRNO::ENOENT)?;
        if child_type != VfsFileType::Directory {
            return Err(FS_ERRNO::ENOTDIR);
        }
        let ext4 = self.fs.ext4.lock();
        debug!("Ext4Inode rmdir: parent_inode={}, name='{}'", self.inode_num, name);
        ext4.dir_remove(self.inode_num, name).map(|_| ())?;
        Ok(())
    }

    fn atime(&self) -> Option<InodeTime> {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        Some(decode_ext4_time(inode_ref.inode.atime(), inode_ref.inode.i_atime_extra()))
    }

    fn mtime(&self) -> Option<InodeTime> {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        Some(decode_ext4_time(inode_ref.inode.mtime(), inode_ref.inode.i_mtime_extra()))
    }

    fn ctime(&self) -> Option<InodeTime> {
        let ext4 = self.fs.ext4.lock();
        let inode_ref = ext4.get_inode_ref(self.inode_num);
        Some(decode_ext4_time(inode_ref.inode.ctime(), inode_ref.inode.i_ctime_extra()))
    }

    fn set_times(
        &self,
        atime: Option<InodeTime>,
        mtime: Option<InodeTime>,
        ctime: Option<InodeTime>,
    ) -> Result<(), FS_ERRNO> {
        let ext4 = self.fs.ext4.lock();
        let mut inode_ref = ext4.get_inode_ref(self.inode_num);

        if let Some(ts) = atime {
            let (sec_lo, extra) = encode_ext4_time(ts);
            inode_ref.inode.set_atime(sec_lo);
            inode_ref.inode.set_i_atime_extra(extra);
        }
        if let Some(ts) = mtime {
            let (sec_lo, extra) = encode_ext4_time(ts);
            inode_ref.inode.set_mtime(sec_lo);
            inode_ref.inode.set_i_mtime_extra(extra);
        }
        if let Some(ts) = ctime {
            let (sec_lo, extra) = encode_ext4_time(ts);
            inode_ref.inode.set_ctime(sec_lo);
            inode_ref.inode.set_i_ctime_extra(extra);
        }

        ext4.write_back_inode(&mut inode_ref);
        Ok(())
    }

    fn rename_child(
        &self,
        old_name: &str,
        new_parent: &Arc<dyn VfsNode>,
        new_name: &str,
    ) -> Result<(), FS_ERRNO> {
        let new_parent = new_parent.as_any().downcast_ref::<Self>().ok_or(FS_ERRNO::EXDEV)?;
        self.rename_child_to(old_name, new_parent, new_name)
    }
}
