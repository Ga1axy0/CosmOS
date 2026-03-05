use alloc::{string::String, sync::Arc, vec::Vec};
use spin::Mutex;

use crate::BLOCK_SZ;
use crate::block_cache::get_block_cache;
use crate::vfs::VfsNode;

use super::{Fat32FileSystem, dir, fat};

#[derive(Clone, Debug)]
struct DirentPos {
    dir_start_cluster: u32,
    entry_offset: usize,
    name_raw: [u8; 11],
    attr: u8,
}

#[derive(Debug)]
struct FatInodeInner {
    start_cluster: u32,
    is_dir: bool,
    size: u32,
    pos: Option<DirentPos>,
}

pub struct FatInode {
    fs: Arc<Fat32FileSystem>,
    inner: Mutex<FatInodeInner>,
}

impl FatInode {
    pub fn new_root(fs: Arc<Fat32FileSystem>) -> Self {
        let root = fs.bpb().root_cluster;
        Self {
            fs,
            inner: Mutex::new(FatInodeInner {
                start_cluster: root,
                is_dir: true,
                size: 0,
                pos: None,
            }),
        }
    }

    fn cluster_size(&self) -> usize {
        self.fs.bpb().cluster_size_bytes()
    }

    fn read_sector(&self, lba: u32, buf: &mut [u8; BLOCK_SZ]) {
        get_block_cache(lba as usize, Arc::clone(self.fs.device()))
            .lock()
            .read_bytes(0, buf);
    }

    fn write_sector(&self, lba: u32, data: &[u8; BLOCK_SZ]) {
        get_block_cache(lba as usize, Arc::clone(self.fs.device()))
            .lock()
            .write_bytes(0, data);
    }

    fn read_cluster_bytes(&self, cluster: u32, offset: usize, buf: &mut [u8]) {
        assert!(offset + buf.len() <= self.cluster_size());
        let bpb = self.fs.bpb();
        let first = bpb.first_sector_of_cluster(cluster);
        let mut remaining = buf;
        let mut off = offset;
        while !remaining.is_empty() {
            let sec_index = off / BLOCK_SZ;
            let sec_off = off % BLOCK_SZ;
            let lba = first + sec_index as u32;
            let mut sector = [0u8; BLOCK_SZ];
            self.read_sector(lba, &mut sector);
            let n = remaining.len().min(BLOCK_SZ - sec_off);
            remaining[..n].copy_from_slice(&sector[sec_off..sec_off + n]);
            remaining = &mut remaining[n..];
            off += n;
        }
    }

    fn write_cluster_bytes(&self, cluster: u32, offset: usize, buf: &[u8]) {
        assert!(offset + buf.len() <= self.cluster_size());
        let bpb = self.fs.bpb();
        let first = bpb.first_sector_of_cluster(cluster);
        let mut remaining = buf;
        let mut off = offset;
        while !remaining.is_empty() {
            let sec_index = off / BLOCK_SZ;
            let sec_off = off % BLOCK_SZ;
            let lba = first + sec_index as u32;
            let mut sector = [0u8; BLOCK_SZ];
            // read-modify-write
            self.read_sector(lba, &mut sector);
            let n = remaining.len().min(BLOCK_SZ - sec_off);
            sector[sec_off..sec_off + n].copy_from_slice(&remaining[..n]);
            self.write_sector(lba, &sector);
            remaining = &remaining[n..];
            off += n;
        }
    }

    fn nth_cluster(&self, start: u32, n: u32) -> Option<u32> {
        if start < 2 {
            return None;
        }
        let bpb = self.fs.bpb();
        let mut cur = start;
        for _ in 0..n {
            let next = fat::read_fat_entry(bpb, self.fs.device(), cur);
            if fat::is_eoc(next) || next < 2 {
                return None;
            }
            cur = next;
        }
        Some(cur)
    }

    fn read_chain_at(&self, start: u32, offset: usize, buf: &mut [u8]) -> usize {
        if start < 2 {
            return 0;
        }
        let cluster_sz = self.cluster_size();
        let mut remaining = buf;
        let mut off = offset;
        let mut total = 0usize;

        while !remaining.is_empty() {
            let cluster_index = (off / cluster_sz) as u32;
            let inner_off = off % cluster_sz;
            let cluster = match self.nth_cluster(start, cluster_index) {
                Some(c) => c,
                None => break,
            };
            let n = remaining.len().min(cluster_sz - inner_off);
            self.read_cluster_bytes(cluster, inner_off, &mut remaining[..n]);
            remaining = &mut remaining[n..];
            off += n;
            total += n;
        }

        total
    }

    fn write_chain_at(&self, start: u32, offset: usize, buf: &[u8]) -> usize {
        if start < 2 {
            return 0;
        }
        let cluster_sz = self.cluster_size();
        let mut remaining = buf;
        let mut off = offset;
        let mut total = 0usize;

        while !remaining.is_empty() {
            let cluster_index = (off / cluster_sz) as u32;
            let inner_off = off % cluster_sz;
            let cluster = match self.nth_cluster(start, cluster_index) {
                Some(c) => c,
                None => break,
            };
            let n = remaining.len().min(cluster_sz - inner_off);
            self.write_cluster_bytes(cluster, inner_off, &remaining[..n]);
            remaining = &remaining[n..];
            off += n;
            total += n;
        }

        total
    }

    fn dir_read_entry_raw(
        &self,
        dir_start_cluster: u32,
        entry_offset: usize,
        out: &mut [u8; 32],
    ) -> usize {
        self.read_chain_at(dir_start_cluster, entry_offset, out)
    }

    fn dir_write_entry_raw(&self, dir_start_cluster: u32, entry_offset: usize, raw: &[u8; 32]) {
        let written = self.write_chain_at(dir_start_cluster, entry_offset, raw);
        assert_eq!(written, 32);
    }

    fn iter_dir_sfn(&self, dir_start_cluster: u32) -> Vec<dir::SfnDirEntry> {
        let mut entries = Vec::new();
        let mut off = 0usize;
        let mut raw = [0u8; 32];

        loop {
            let read = self.dir_read_entry_raw(dir_start_cluster, off, &mut raw);
            if read != 32 {
                break;
            }
            if raw[0] == 0x00 {
                break;
            }
            if let Some(e) = dir::parse_sfn_dir_entry(&raw, off) {
                // Ignore LFN entries for now.
                if e.is_lfn() {
                    off += 32;
                    continue;
                }
                // Skip deleted/free.
                if e.name_raw[0] == 0xE5 {
                    off += 32;
                    continue;
                }
                // Skip volume label.
                if e.is_volume_label() {
                    off += 32;
                    continue;
                }
                entries.push(e);
            }
            off += 32;
        }

        entries
    }

    fn iter_dir(&self, dir_start_cluster: u32) -> Vec<dir::DirEntry> {
        let mut entries = Vec::new();
        let mut off = 0usize;
        let mut raw = [0u8; 32];
        let mut lfn_parts: Vec<dir::LfnPart> = Vec::new();

        loop {
            let read = self.dir_read_entry_raw(dir_start_cluster, off, &mut raw);
            if read != 32 {
                break;
            }
            if raw[0] == 0x00 {
                break;
            }

            // Deleted entry: drop any pending LFN parts.
            if raw[0] == 0xE5 {
                lfn_parts.clear();
                off += 32;
                continue;
            }

            if let Some(p) = dir::parse_lfn_part(&raw) {
                lfn_parts.push(p);
                off += 32;
                continue;
            }

            if let Some(sfn) = dir::parse_sfn_dir_entry(&raw, off) {
                // Skip volume label.
                if sfn.is_volume_label() {
                    lfn_parts.clear();
                    off += 32;
                    continue;
                }
                // Skip free/deleted (already handled by raw[0], but keep safe).
                if sfn.name_raw[0] == 0xE5 {
                    lfn_parts.clear();
                    off += 32;
                    continue;
                }
                // Try assemble long name if we have pending LFN parts.
                let long_name = if lfn_parts.is_empty() {
                    None
                } else {
                    let ln = dir::assemble_lfn(&lfn_parts, &sfn.name_raw);
                    lfn_parts.clear();
                    ln
                };
                entries.push(dir::DirEntry { sfn, long_name });
            } else {
                // Unknown/invalid entry: clear pending LFN.
                lfn_parts.clear();
            }

            off += 32;
        }

        entries
    }

    fn find_in_dir(&self, dir_start_cluster: u32, sfn: &[u8; 11]) -> Option<dir::SfnDirEntry> {
        self.iter_dir_sfn(dir_start_cluster)
            .into_iter()
            .find(|e| &e.name_raw == sfn)
    }

    fn find_by_name_in_dir(&self, dir_start_cluster: u32, name: &str) -> Option<dir::SfnDirEntry> {
        self.iter_dir(dir_start_cluster)
            .into_iter()
            .find(|e| {
                if e
                    .long_name
                    .as_ref()
                    .map(|ln| dir::name_eq(ln, name))
                    .unwrap_or(false)
                {
                    return true;
                }
                dir::name_eq(&e.sfn.name_string(), name)
            })
            .map(|e| e.sfn)
    }

    fn find_free_dirent_offset(&self, dir_start_cluster: u32) -> usize {
        let mut off = 0usize;
        let mut raw = [0u8; 32];
        loop {
            let read = self.dir_read_entry_raw(dir_start_cluster, off, &mut raw);
            if read != 32 {
                return off;
            }
            if raw[0] == 0x00 || raw[0] == 0xE5 {
                return off;
            }
            off += 32;
        }
    }

    fn find_free_dirent_range(&self, dir_start_cluster: u32, slots: usize) -> usize {
        assert!(slots >= 1);
        let mut off = 0usize;
        let mut raw = [0u8; 32];
        let mut run = 0usize;
        let mut run_start = 0usize;

        loop {
            let read = self.dir_read_entry_raw(dir_start_cluster, off, &mut raw);
            if read != 32 {
                // beyond current directory: treat as free
                return if run == 0 { off } else { run_start };
            }

            let free = raw[0] == 0x00 || raw[0] == 0xE5;
            if free {
                if run == 0 {
                    run_start = off;
                }
                run += 1;
                if run >= slots {
                    return run_start;
                }
                // If this is 0x00, rest are free; we can allocate immediately.
                if raw[0] == 0x00 {
                    return run_start;
                }
            } else {
                run = 0;
            }

            off += 32;
        }
    }

    fn ensure_dir_entry_slot(&self, dir_start_cluster: u32, want_end_offset: usize) {
        // Ensure directory has clusters to cover want_end_offset+32.
        let cluster_sz = self.cluster_size();
        let need_clusters = (want_end_offset + 31) / cluster_sz + 1;

        // Count current clusters by walking chain.
        let bpb = self.fs.bpb();
        let mut cur = dir_start_cluster;
        let mut have_clusters = 1usize;
        loop {
            let next = fat::read_fat_entry(bpb, self.fs.device(), cur);
            if fat::is_eoc(next) || next < 2 {
                break;
            }
            cur = next;
            have_clusters += 1;
        }

        if have_clusters >= need_clusters {
            return;
        }

        let mut inner = self.fs.inner().lock();
        let mut last = fat::last_cluster(bpb, self.fs.device(), dir_start_cluster);
        while have_clusters < need_clusters {
            let newc = fat::alloc_cluster(bpb, self.fs.device(), &mut inner)
                .expect("FAT32: out of clusters while extending directory");
            fat::set_next(bpb, self.fs.device(), last, newc);
            fat::set_next(bpb, self.fs.device(), newc, fat::FAT32_EOC);
            // zero-fill new cluster
            let zero = [0u8; BLOCK_SZ];
            let first = bpb.first_sector_of_cluster(newc);
            for i in 0..bpb.sectors_per_cluster {
                self.write_sector(first + i as u32, &zero);
            }
            last = newc;
            have_clusters += 1;
        }
    }

    fn update_dirent_after_change(&self, pos: &DirentPos, first_cluster: u32, size: u32) {
        let raw = dir::build_sfn_entry(&pos.name_raw, pos.attr, first_cluster, size);
        self.dir_write_entry_raw(pos.dir_start_cluster, pos.entry_offset, &raw);
    }

    fn ensure_file_clusters(&self, inner: &mut FatInodeInner, new_size: u32) {
        if inner.is_dir {
            return;
        }
        let cluster_sz = self.cluster_size() as u32;
        let need_clusters = if new_size == 0 {
            0
        } else {
            ((new_size - 1) / cluster_sz + 1) as usize
        };

        let bpb = self.fs.bpb();

        // Count existing clusters.
        let mut have_clusters = 0usize;
        if inner.start_cluster >= 2 {
            have_clusters = 1;
            let mut cur = inner.start_cluster;
            loop {
                let next = fat::read_fat_entry(bpb, self.fs.device(), cur);
                if fat::is_eoc(next) || next < 2 {
                    break;
                }
                cur = next;
                have_clusters += 1;
            }
        }

        if have_clusters >= need_clusters {
            return;
        }

        let mut fs_inner = self.fs.inner().lock();

        // If empty, allocate first cluster.
        if inner.start_cluster < 2 {
            if need_clusters == 0 {
                return;
            }
            let first = fat::alloc_cluster(bpb, self.fs.device(), &mut fs_inner)
                .expect("FAT32: out of clusters");
            inner.start_cluster = first;
            have_clusters = 1;
            // zero-fill
            let zero = [0u8; BLOCK_SZ];
            let lba0 = bpb.first_sector_of_cluster(first);
            for i in 0..bpb.sectors_per_cluster {
                self.write_sector(lba0 + i as u32, &zero);
            }
        }

        let mut last = fat::last_cluster(bpb, self.fs.device(), inner.start_cluster);
        while have_clusters < need_clusters {
            let newc = fat::alloc_cluster(bpb, self.fs.device(), &mut fs_inner)
                .expect("FAT32: out of clusters");
            fat::set_next(bpb, self.fs.device(), last, newc);
            fat::set_next(bpb, self.fs.device(), newc, fat::FAT32_EOC);
            // zero-fill
            let zero = [0u8; BLOCK_SZ];
            let lba0 = bpb.first_sector_of_cluster(newc);
            for i in 0..bpb.sectors_per_cluster {
                self.write_sector(lba0 + i as u32, &zero);
            }
            last = newc;
            have_clusters += 1;
        }
    }
}

impl VfsNode for FatInode {
    fn ls(&self) -> Vec<String> {
        let inner = self.inner.lock();
        if !inner.is_dir {
            return Vec::new();
        }
        let dir_cluster = inner.start_cluster;
        drop(inner);

        self.iter_dir(dir_cluster)
            .into_iter()
            .map(|e| e.name_string())
            .collect()
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let inner = self.inner.lock();
        if !inner.is_dir {
            return None;
        }
        let dir_cluster = inner.start_cluster;
        drop(inner);

        let e = self.find_by_name_in_dir(dir_cluster, name)?;
        let is_dir = e.is_dir();

        let inode = FatInode {
            fs: Arc::clone(&self.fs),
            inner: Mutex::new(FatInodeInner {
                start_cluster: e.first_cluster,
                is_dir,
                size: e.file_size,
                pos: Some(DirentPos {
                    dir_start_cluster: dir_cluster,
                    entry_offset: e.entry_offset,
                    name_raw: e.name_raw,
                    attr: e.attr,
                }),
            }),
        };
        Some(Arc::new(inode) as Arc<dyn VfsNode>)
    }

    fn create(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        let inner = self.inner.lock();
        if !inner.is_dir {
            return None;
        }
        let dir_cluster = inner.start_cluster;
        drop(inner);

        // Reject if name already exists (either as LFN or SFN).
        if self.find_by_name_in_dir(dir_cluster, name).is_some() {
            return None;
        }

        // If name fits 8.3, create SFN-only entry.
        if let Some(sfn) = dir::sfn_from_str(name) {
            if self.find_in_dir(dir_cluster, &sfn).is_some() {
                return None;
            }
            let off = self.find_free_dirent_offset(dir_cluster);
            self.ensure_dir_entry_slot(dir_cluster, off);

            let attr = dir::ATTR_ARCHIVE;
            let raw = dir::build_sfn_entry(&sfn, attr, 0, 0);
            self.dir_write_entry_raw(dir_cluster, off, &raw);

            let inode = FatInode {
                fs: Arc::clone(&self.fs),
                inner: Mutex::new(FatInodeInner {
                    start_cluster: 0,
                    is_dir: false,
                    size: 0,
                    pos: Some(DirentPos {
                        dir_start_cluster: dir_cluster,
                        entry_offset: off,
                        name_raw: sfn,
                        attr,
                    }),
                }),
            };
            return Some(Arc::new(inode) as Arc<dyn VfsNode>);
        }

        // Otherwise, create LFN + SFN alias.
        if !dir::is_valid_lfn_name(name) {
            return None;
        }

        // Generate a unique SFN alias.
        let mut alias: Option<[u8; 11]> = None;
        for n in 1u32..=9999u32 {
            let cand = dir::sfn_alias_from_lfn(name, n)?;
            if self.find_in_dir(dir_cluster, &cand).is_none() {
                alias = Some(cand);
                break;
            }
        }
        let sfn = alias?;

        let lfn_entries = dir::build_lfn_entries(name, &sfn)?;
        let slots = lfn_entries.len() + 1;
        let off0 = self.find_free_dirent_range(dir_cluster, slots);
        let sfn_off = off0 + lfn_entries.len() * 32;
        self.ensure_dir_entry_slot(dir_cluster, sfn_off);

        // Write LFN entries then SFN entry.
        for (i, e) in lfn_entries.iter().enumerate() {
            self.dir_write_entry_raw(dir_cluster, off0 + i * 32, e);
        }
        let attr = dir::ATTR_ARCHIVE;
        let raw = dir::build_sfn_entry(&sfn, attr, 0, 0);
        self.dir_write_entry_raw(dir_cluster, sfn_off, &raw);

        let inode = FatInode {
            fs: Arc::clone(&self.fs),
            inner: Mutex::new(FatInodeInner {
                start_cluster: 0,
                is_dir: false,
                size: 0,
                pos: Some(DirentPos {
                    dir_start_cluster: dir_cluster,
                    entry_offset: sfn_off,
                    name_raw: sfn,
                    attr,
                }),
            }),
        };
        Some(Arc::new(inode) as Arc<dyn VfsNode>)
    }

    fn clear(&self) {
        let mut inner = self.inner.lock();
        if inner.is_dir {
            return;
        }
        if inner.start_cluster >= 2 {
            let bpb = self.fs.bpb();
            fat::free_chain(bpb, self.fs.device(), inner.start_cluster);
        }
        inner.start_cluster = 0;
        inner.size = 0;
        if let Some(pos) = &inner.pos {
            self.update_dirent_after_change(pos, 0, 0);
        }
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let inner = self.inner.lock();
        if inner.is_dir {
            return 0;
        }
        if offset as u32 >= inner.size {
            return 0;
        }
        let size = inner.size;
        let start_cluster = inner.start_cluster;
        drop(inner);

        let max = (size as usize).saturating_sub(offset);
        let to_read = buf.len().min(max);
        self.read_chain_at(start_cluster, offset, &mut buf[..to_read])
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let mut inner = self.inner.lock();
        if inner.is_dir {
            return 0;
        }

        let new_end = offset.saturating_add(buf.len()) as u32;
        if new_end > inner.size {
            self.ensure_file_clusters(&mut inner, new_end);
            inner.size = new_end;
        } else if inner.start_cluster < 2 && !buf.is_empty() {
            self.ensure_file_clusters(&mut inner, new_end);
        }

        let start_cluster = inner.start_cluster;
        let written = if buf.is_empty() {
            0
        } else {
            self.write_chain_at(start_cluster, offset, buf)
        };

        if let Some(pos) = &inner.pos {
            self.update_dirent_after_change(pos, inner.start_cluster, inner.size);
        }

        written
    }
}
