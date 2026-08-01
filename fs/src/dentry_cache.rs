use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;

use lazy_static::lazy_static;
use spin::Mutex;

use crate::vfs::Inode;

/// Key for the dentry cache: `(fs_id, parent_inode_number, child_name)`.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct DentryKey {
    fs_id: u64,
    parent_ino: u64,
    name: String,
}

/// A single dentry cache entry.
struct DentryEntry {
    /// Strong reference to the child inode so hot dentries stay reusable even if
    /// the standalone inode cache decides to reclaim its own copy.
    /// `None` represents a negative dentry: the name was looked up and did not
    /// exist in this parent directory.
    child: Option<Arc<Inode>>,
    /// CLOCK second-chance bit.
    ref_bit: bool,
}

/// Result of a dentry-cache lookup.
///
/// This must remain distinct from `Option<Arc<Inode>>`: `Negative` means the
/// cache contains a known ENOENT result, while `Miss` means the backend still
/// needs to be queried.
pub enum DentryLookup {
    Positive(Arc<Inode>),
    Negative,
    Miss,
}

/// Global dentry cache manager.
struct DentryCache {
    table: BTreeMap<DentryKey, DentryEntry>,
    /// CLOCK queue; the same key may appear more than once.
    inactive: VecDeque<DentryKey>,
    /// Start eviction when the table exceeds this size.
    high_watermark: usize,
    /// Stop eviction once the table shrinks to this size.
    low_watermark: usize,
    /// Number of entries whose child is `None`.
    negative_entries: usize,
}

impl DentryCache {
    fn new() -> Self {
        Self {
            table: BTreeMap::new(),
            inactive: VecDeque::new(),
            high_watermark: 4096,
            low_watermark: 2304,
            negative_entries: 0,
        }
    }

    /// Look up a dentry by `(fs_id, parent_ino, name)`.
    fn lookup(&mut self, fs_id: u64, parent_ino: u64, name: &str) -> DentryLookup {
        let key = DentryKey {
            fs_id,
            parent_ino,
            name: String::from(name),
        };
        if let Some(entry) = self.table.get_mut(&key) {
            entry.ref_bit = true;
            return match entry.child.as_ref() {
                Some(child) => DentryLookup::Positive(Arc::clone(child)),
                None => DentryLookup::Negative,
            };
        }
        DentryLookup::Miss
    }

    /// Insert or replace a positive `(parent, name) → child` mapping.
    fn insert(&mut self, fs_id: u64, parent_ino: u64, name: &str, child: &Arc<Inode>) {
        let key = DentryKey {
            fs_id,
            parent_ino,
            name: String::from(name),
        };
        if self.table.contains_key(&key) {
            let was_negative = self
                .table
                .get(&key)
                .map(|entry| entry.child.is_none())
                .unwrap_or(false);
            if was_negative {
                self.negative_entries = self.negative_entries.saturating_sub(1);
            }
            let Some(entry) = self.table.get_mut(&key) else {
                return;
            };
            entry.child = Some(Arc::clone(child));
            entry.ref_bit = true;
            return;
        }
        self.table.insert(
            key.clone(),
            DentryEntry {
                child: Some(Arc::clone(child)),
                ref_bit: true,
            },
        );
        self.inactive.push_back(key);
        self.reclaim_if_needed();
    }

    /// Insert or replace a negative `(parent, name) → ENOENT` mapping.
    fn insert_negative(&mut self, fs_id: u64, parent_ino: u64, name: &str) {
        let key = DentryKey {
            fs_id,
            parent_ino,
            name: String::from(name),
        };
        if self.table.contains_key(&key) {
            let was_positive = self
                .table
                .get(&key)
                .map(|entry| entry.child.is_some())
                .unwrap_or(false);
            if was_positive {
                self.negative_entries += 1;
            }
            let Some(entry) = self.table.get_mut(&key) else {
                return;
            };
            entry.child = None;
            entry.ref_bit = true;
            return;
        }
        self.table.insert(
            key.clone(),
            DentryEntry {
                child: None,
                ref_bit: true,
            },
        );
        self.inactive.push_back(key);
        self.negative_entries += 1;
        self.reclaim_if_needed();
    }

    /// Remove a single dentry (called on unlink / rmdir / rename).
    fn remove(&mut self, fs_id: u64, parent_ino: u64, name: &str) {
        let key = DentryKey {
            fs_id,
            parent_ino,
            name: String::from(name),
        };
        if let Some(entry) = self.table.remove(&key) {
            if entry.child.is_none() {
                self.negative_entries = self.negative_entries.saturating_sub(1);
            }
        }
    }

    // ------------------------------------------------------------------
    // CLOCK eviction
    // ------------------------------------------------------------------

    fn reclaim_if_needed(&mut self) {
        while self.table.len() > self.high_watermark {
            if !self.reclaim_one() {
                break;
            }
        }
    }

    fn reclaim_one(&mut self) -> bool {
        if self.table.len() <= self.low_watermark {
            return false;
        }
        let Some(key) = self.inactive.pop_front() else {
            return false;
        };
        let Some(entry) = self.table.get_mut(&key) else {
            // Already removed (e.g. via explicit remove()).
            return true;
        };
        if entry.ref_bit {
            entry.ref_bit = false;
            self.inactive.push_back(key);
            return true;
        }
        if let Some(entry) = self.table.remove(&key) {
            if entry.child.is_none() {
                self.negative_entries = self.negative_entries.saturating_sub(1);
            }
        }
        true
    }
}

/// Snapshot of the global dentry cache state.
#[derive(Clone, Copy, Debug, Default)]
pub struct DentryCacheStats {
    /// Number of live positive and negative `(parent, name)` cache entries.
    pub entries: usize,
    /// Number of live negative `(parent, name) -> ENOENT` entries.
    pub negative_entries: usize,
    /// Number of queued CLOCK candidates, including stale duplicates.
    pub inactive_entries: usize,
    /// Entry-count threshold that starts eviction.
    pub high_watermark: usize,
    /// Entry-count threshold that stops eviction.
    pub low_watermark: usize,
}

lazy_static! {
    static ref DENTRY_CACHE: Mutex<DentryCache> = Mutex::new(DentryCache::new());
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Try to resolve `(fs_id, parent_ino, name)` from the dentry cache.
pub fn lookup_dentry(fs_id: u64, parent_ino: u64, name: &str) -> DentryLookup {
    DENTRY_CACHE.lock().lookup(fs_id, parent_ino, name)
}

/// Store `(fs_id, parent_ino, name) → child` in the dentry cache.
pub fn insert_dentry(fs_id: u64, parent_ino: u64, name: &str, child: &Arc<Inode>) {
    DENTRY_CACHE.lock().insert(fs_id, parent_ino, name, child)
}

/// Store a negative `(fs_id, parent_ino, name) -> ENOENT` mapping.
pub fn insert_negative_dentry(fs_id: u64, parent_ino: u64, name: &str) {
    DENTRY_CACHE.lock().insert_negative(fs_id, parent_ino, name)
}

/// Explicitly invalidate a dentry (unlink / rmdir / rename).
pub fn remove_dentry(fs_id: u64, parent_ino: u64, name: &str) {
    DENTRY_CACHE.lock().remove(fs_id, parent_ino, name)
}

/// Return the current global dentry-cache footprint and queue depths.
pub fn dentry_cache_stats() -> DentryCacheStats {
    let cache = DENTRY_CACHE.lock();
    DentryCacheStats {
        entries: cache.table.len(),
        negative_entries: cache.negative_entries,
        inactive_entries: cache.inactive.len(),
        high_watermark: cache.high_watermark,
        low_watermark: cache.low_watermark,
    }
}
