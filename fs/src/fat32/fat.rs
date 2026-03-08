use alloc::sync::Arc;

use crate::block_cache::get_block_cache;
use crate::block_dev::BlockDevice;
use crate::BLOCK_SZ;

use super::Fat32Bpb;

pub const FAT32_EOC: u32 = 0x0FFFFFFF;

#[inline]
pub fn is_eoc(v: u32) -> bool {
    v >= 0x0FFFFFF8
}

#[derive(Debug)]
pub struct Fat32Inner {
    pub next_free_hint: u32,
}

impl Fat32Inner {
    pub fn new(bpb: &Fat32Bpb) -> Self {
        Self {
            next_free_hint: 2.min(bpb.max_cluster),
        }
    }
}

fn read_u32_from_sector(block_device: &Arc<dyn BlockDevice>, lba: u32, offset: usize) -> u32 {
    let mut bytes = [0u8; 4];
    get_block_cache(lba as usize, Arc::clone(block_device))
        .lock()
        .read_bytes(offset, &mut bytes);
    u32::from_le_bytes(bytes)
}

fn write_u32_to_sector(block_device: &Arc<dyn BlockDevice>, lba: u32, offset: usize, val: u32) {
    let bytes = val.to_le_bytes();
    get_block_cache(lba as usize, Arc::clone(block_device))
        .lock()
        .write_bytes(offset, &bytes);
}

pub fn fat_entry_pos(bpb: &Fat32Bpb, cluster: u32, fat_index: u8) -> (u32, usize) {
    // each entry is 4 bytes
    let fat_base = bpb.fat_start_lba + fat_index as u32 * bpb.fat_sectors;
    let byte_offset = cluster * 4;
    let sector = fat_base + (byte_offset / BLOCK_SZ as u32);
    let offset = (byte_offset % BLOCK_SZ as u32) as usize;
    (sector, offset)
}

pub fn read_fat_entry(bpb: &Fat32Bpb, block_device: &Arc<dyn BlockDevice>, cluster: u32) -> u32 {
    let (sector, offset) = fat_entry_pos(bpb, cluster, 0);
    read_u32_from_sector(block_device, sector, offset) & 0x0FFFFFFF
}

pub fn write_fat_entry(
    bpb: &Fat32Bpb,
    block_device: &Arc<dyn BlockDevice>,
    cluster: u32,
    value: u32,
) {
    let value = value & 0x0FFFFFFF;
    for fat_i in 0..bpb.num_fats {
        let (sector, offset) = fat_entry_pos(bpb, cluster, fat_i);
        let old = read_u32_from_sector(block_device, sector, offset);
        let new = (old & 0xF0000000) | value;
        write_u32_to_sector(block_device, sector, offset, new);
    }
}

pub fn set_next(
    bpb: &Fat32Bpb,
    block_device: &Arc<dyn BlockDevice>,
    cluster: u32,
    next: u32,
) {
    write_fat_entry(bpb, block_device, cluster, next);
}

pub fn alloc_cluster(
    bpb: &Fat32Bpb,
    block_device: &Arc<dyn BlockDevice>,
    inner: &mut Fat32Inner,
) -> Option<u32> {
    if bpb.max_cluster < 2 {
        return None;
    }

    // Two-pass scan: from hint..end, then 2..hint
    let start = inner.next_free_hint.max(2).min(bpb.max_cluster);
    for pass in 0..2 {
        let (from, to) = if pass == 0 {
            (start, bpb.max_cluster)
        } else {
            (2, start.saturating_sub(1))
        };
        if from > to {
            continue;
        }
        for c in from..=to {
            if read_fat_entry(bpb, block_device, c) == 0 {
                write_fat_entry(bpb, block_device, c, FAT32_EOC);
                inner.next_free_hint = (c + 1).min(bpb.max_cluster);
                return Some(c);
            }
        }
    }

    None
}

pub fn free_chain(bpb: &Fat32Bpb, block_device: &Arc<dyn BlockDevice>, start: u32) {
    if start < 2 {
        return;
    }
    let mut cur = start;
    loop {
        let next = read_fat_entry(bpb, block_device, cur);
        write_fat_entry(bpb, block_device, cur, 0);
        if is_eoc(next) || next < 2 {
            break;
        }
        cur = next;
    }
}

pub fn last_cluster(bpb: &Fat32Bpb, block_device: &Arc<dyn BlockDevice>, start: u32) -> u32 {
    assert!(start >= 2);
    let mut cur = start;
    loop {
        let next = read_fat_entry(bpb, block_device, cur);
        if is_eoc(next) || next < 2 {
            return cur;
        }
        cur = next;
    }
}
