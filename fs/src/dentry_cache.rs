use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::sync::Arc;

use lazy_static::lazy_static;
use spin::Mutex;

use crate::vfs::Inode;

// Cargo freshness checks keep directory entries for workspace sources,
// registry crates, fingerprints, dep-info and build outputs live at the same
// time.  A 4K cache cycles several times during one no-op `cargo build`, which
// sends an otherwise hot metadata workload back through ext4.  Keep enough
// entries for that working set while retaining a lower watermark for bounded
// CLOCK reclaim once larger workloads exceed it.
const DENTRY_CACHE_HIGH_WATERMARK: usize = 16 * 1024;
const DENTRY_CACHE_LOW_WATERMARK: usize = 12 * 1024;

/// Key for the dentry cache: `(fs_id, parent_inode_number, child_name)`.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct DentryKey {
    fs_id: u64,
    parent_ino: u64,
    name: String,
}

/// Directory-local bucket key.  Keeping the owned `String` one level below
/// this key lets lookup/remove query it with `&str` instead of allocating a
/// temporary `String` for every dentry-cache hit.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct DentryParentKey {
    fs_id: u64,
    parent_ino: u64,
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
    /// Parent directory to its cached child names.  `String: Borrow<str>`
    /// makes the overwhelmingly common hit path allocation-free.
    table: BTreeMap<DentryParentKey, BTreeMap<String, DentryEntry>>,
    /// Total entries across all parent buckets.
    entries: usize,
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
            entries: 0,
            inactive: VecDeque::new(),
            high_watermark: DENTRY_CACHE_HIGH_WATERMARK,
            low_watermark: DENTRY_CACHE_LOW_WATERMARK,
            negative_entries: 0,
        }
    }

    /// Look up a dentry by `(fs_id, parent_ino, name)`.
    fn lookup(&mut self, fs_id: u64, parent_ino: u64, name: &str) -> DentryLookup {
        let parent = DentryParentKey { fs_id, parent_ino };
        if let Some(entry) = self
            .table
            .get_mut(&parent)
            .and_then(|children| children.get_mut(name))
        {
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
        let parent = DentryParentKey { fs_id, parent_ino };
        if let Some(entry) = self
            .table
            .get_mut(&parent)
            .and_then(|children| children.get_mut(name))
        {
            if entry.child.is_none() {
                self.negative_entries = self.negative_entries.saturating_sub(1);
            }
            entry.child = Some(Arc::clone(child));
            entry.ref_bit = true;
            return;
        }
        let name = String::from(name);
        self.table.entry(parent).or_default().insert(
            name.clone(),
            DentryEntry {
                child: Some(Arc::clone(child)),
                ref_bit: true,
            },
        );
        self.entries += 1;
        self.inactive.push_back(DentryKey {
            fs_id,
            parent_ino,
            name,
        });
        self.reclaim_if_needed();
    }

    /// Insert or replace a negative `(parent, name) → ENOENT` mapping.
    fn insert_negative(&mut self, fs_id: u64, parent_ino: u64, name: &str) {
        let parent = DentryParentKey { fs_id, parent_ino };
        if let Some(entry) = self
            .table
            .get_mut(&parent)
            .and_then(|children| children.get_mut(name))
        {
            if entry.child.is_some() {
                self.negative_entries += 1;
            }
            entry.child = None;
            entry.ref_bit = true;
            return;
        }
        let name = String::from(name);
        self.table.entry(parent).or_default().insert(
            name.clone(),
            DentryEntry {
                child: None,
                ref_bit: true,
            },
        );
        self.entries += 1;
        self.inactive.push_back(DentryKey {
            fs_id,
            parent_ino,
            name,
        });
        self.negative_entries += 1;
        self.reclaim_if_needed();
    }

    /// Remove a single dentry (called on unlink / rmdir / rename).
    fn remove(&mut self, fs_id: u64, parent_ino: u64, name: &str) {
        let parent = DentryParentKey { fs_id, parent_ino };
        let mut remove_parent = false;
        let removed = self.table.get_mut(&parent).and_then(|children| {
            let removed = children.remove(name);
            remove_parent = children.is_empty();
            removed
        });
        if remove_parent {
            self.table.remove(&parent);
        }
        if let Some(entry) = removed {
            self.entries = self.entries.saturating_sub(1);
            if entry.child.is_none() {
                self.negative_entries = self.negative_entries.saturating_sub(1);
            }
        }
    }

    /// Remove every cached child of one directory.
    ///
    /// Namespace operations such as `pivot_root(2)` can replace the backing
    /// directory represented by a synthetic VFS inode without knowing which
    /// child names have previously been looked up.  Invalidating the complete
    /// parent is required in that case so positive and negative entries from
    /// the old namespace cannot leak into the new one.
    fn remove_parent(&mut self, fs_id: u64, parent_ino: u64) {
        let parent = DentryParentKey { fs_id, parent_ino };
        let Some(children) = self.table.remove(&parent) else {
            return;
        };
        let removed_entries = children.len();
        let removed_negative = children
            .values()
            .filter(|entry| entry.child.is_none())
            .count();
        self.entries = self.entries.saturating_sub(removed_entries);
        self.negative_entries = self.negative_entries.saturating_sub(removed_negative);
    }

    // ------------------------------------------------------------------
    // CLOCK eviction
    // ------------------------------------------------------------------

    fn reclaim_if_needed(&mut self) {
        while self.entries > self.high_watermark {
            if !self.reclaim_one() {
                break;
            }
        }
    }

    fn reclaim_one(&mut self) -> bool {
        if self.entries <= self.low_watermark {
            return false;
        }
        let Some(key) = self.inactive.pop_front() else {
            return false;
        };
        let parent = DentryParentKey {
            fs_id: key.fs_id,
            parent_ino: key.parent_ino,
        };
        let Some(entry) = self
            .table
            .get_mut(&parent)
            .and_then(|children| children.get_mut(key.name.as_str()))
        else {
            // Already removed (e.g. via explicit remove()).
            return true;
        };
        if entry.ref_bit {
            entry.ref_bit = false;
            self.inactive.push_back(key);
            return true;
        }
        let mut remove_parent = false;
        let removed = self.table.get_mut(&parent).and_then(|children| {
            let removed = children.remove(key.name.as_str());
            remove_parent = children.is_empty();
            removed
        });
        if remove_parent {
            self.table.remove(&parent);
        }
        if let Some(entry) = removed {
            self.entries = self.entries.saturating_sub(1);
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

/// Explicitly invalidate every cached child of one directory.
pub fn remove_parent_dentries(fs_id: u64, parent_ino: u64) {
    DENTRY_CACHE.lock().remove_parent(fs_id, parent_ino)
}

/// Return the current global dentry-cache footprint and queue depths.
pub fn dentry_cache_stats() -> DentryCacheStats {
    let cache = DENTRY_CACHE.lock();
    DentryCacheStats {
        entries: cache.entries,
        negative_entries: cache.negative_entries,
        inactive_entries: cache.inactive.len(),
        high_watermark: cache.high_watermark,
        low_watermark: cache.low_watermark,
    }
}
