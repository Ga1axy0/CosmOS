//! Firmware-described AHCI block device support for LoongArch boards.

use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{compiler_fence, Ordering};

use fs::{BlockDevice, BLOCK_SZ};
use simple_ahci::{AhciDriver, Hal as AhciHal};

use crate::config::PAGE_SIZE;
use crate::mm::{frame_alloc_contiguous, phys_to_virt, virt_to_phys, ContiguousFrames, PhysAddr};
use crate::sync::SpinNoIrqLock;

use super::{block_device_name, BLOCK_DEVICES};

const MAX_TRANSFER_BYTES: usize = 128 * 1024;
const MBR_SIGNATURE_OFFSET: usize = 510;
const MBR_PARTITION_OFFSET: usize = 446;
const MBR_PARTITION_SIZE: usize = 16;
const GPT_HEADER_LBA: usize = 1;
const GPT_SIGNATURE: &[u8; 8] = b"EFI PART";
const EXT4_MAGIC_OFFSET_IN_SUPERBLOCK: usize = 0x38;
const EXT4_SUPERBLOCK_SECTOR_OFFSET: usize = 2;
const EXT4_MAGIC: [u8; 2] = [0x53, 0xef];

struct CosmOsAhciHal;

impl AhciHal for CosmOsAhciHal {
    fn virt_to_phys(va: usize) -> usize {
        virt_to_phys(va)
    }

    fn current_ms() -> u64 {
        crate::timer::get_time_ms() as u64
    }

    fn flush_dcache() {
        // LS2K1000 describes the SoC bus as dma-coherent. The hardware dbar
        // still orders CPU accesses against controller MMIO and DMA visibility.
        unsafe { core::arch::asm!("dbar 0", options(nostack, preserves_flags)) };
        compiler_fence(Ordering::SeqCst);
    }
}

struct DmaBuffer {
    frames: ContiguousFrames,
    len: usize,
}

impl DmaBuffer {
    fn new(len: usize) -> Option<Self> {
        let pages = len.div_ceil(PAGE_SIZE).max(1).next_power_of_two();
        let frames = frame_alloc_contiguous(pages, pages)?;
        Some(Self { frames, len })
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        let pa: PhysAddr = self.frames.start_ppn().into();
        // SAFETY: `frames` owns at least `len` contiguous, direct-mapped bytes
        // and remains alive for the returned slice's complete lifetime.
        unsafe { core::slice::from_raw_parts_mut(phys_to_virt(pa.0) as *mut u8, self.len) }
    }
}

/// Synchronous AHCI adapter implementing CosmOS's filesystem block contract.
struct AhciBlock {
    driver: SpinNoIrqLock<AhciDriver<CosmOsAhciHal>>,
}

/// A bounded view of one partition on an AHCI disk.
struct AhciPartition {
    disk: Arc<AhciBlock>,
    start_sector: usize,
    sector_count: usize,
}

impl AhciPartition {
    fn checked_start(&self, block_id: usize, byte_len: usize) -> usize {
        assert_eq!(byte_len % BLOCK_SZ, 0);
        let blocks = byte_len / BLOCK_SZ;
        let end = block_id
            .checked_add(blocks)
            .expect("AHCI partition request overflow");
        assert!(
            end <= self.sector_count,
            "AHCI partition request out of range"
        );
        self.start_sector
            .checked_add(block_id)
            .expect("AHCI partition LBA overflow")
    }
}

impl BlockDevice for AhciPartition {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        let start = self.checked_start(block_id, buf.len());
        self.disk.read_range(start, buf);
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) {
        let start = self.checked_start(block_id, buf.len());
        self.disk.write_range(start, buf);
    }

    fn read_blocks(&self, start_block: usize, buf: &mut [u8]) {
        let start = self.checked_start(start_block, buf.len());
        self.disk.read_range(start, buf);
    }

    fn write_blocks(&self, start_block: usize, buf: &[u8]) {
        let start = self.checked_start(start_block, buf.len());
        self.disk.write_range(start, buf);
    }
}

#[derive(Clone, Copy)]
struct PartitionInfo {
    index: usize,
    start_sector: usize,
    sector_count: usize,
}

#[inline]
fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

#[inline]
fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn read_sector(disk: &AhciBlock, lba: usize) -> [u8; BLOCK_SZ] {
    let mut sector = [0u8; BLOCK_SZ];
    disk.read_range(lba, &mut sector);
    sector
}

fn has_ext4_superblock(disk: &AhciBlock, start_sector: usize, sector_count: usize) -> bool {
    if sector_count <= EXT4_SUPERBLOCK_SECTOR_OFFSET {
        return false;
    }
    let sector = read_sector(disk, start_sector + EXT4_SUPERBLOCK_SECTOR_OFFSET);
    sector[EXT4_MAGIC_OFFSET_IN_SUPERBLOCK..EXT4_MAGIC_OFFSET_IN_SUPERBLOCK + 2] == EXT4_MAGIC
}

fn valid_partition(start: u64, count: u64, disk_sectors: u64) -> Option<(usize, usize)> {
    let end = start.checked_add(count)?;
    if start == 0 || count == 0 || end > disk_sectors {
        return None;
    }
    Some((usize::try_from(start).ok()?, usize::try_from(count).ok()?))
}

fn scan_mbr_ext4(disk: &AhciBlock, disk_sectors: u64) -> alloc::vec::Vec<PartitionInfo> {
    let mbr = read_sector(disk, 0);
    if mbr[MBR_SIGNATURE_OFFSET..] != [0x55, 0xaa] {
        return alloc::vec::Vec::new();
    }

    let mut partitions = alloc::vec::Vec::new();
    for index in 0..4 {
        let offset = MBR_PARTITION_OFFSET + index * MBR_PARTITION_SIZE;
        let kind = mbr[offset + 4];
        if kind == 0 || kind == 0xee {
            continue;
        }
        let start = le_u32(&mbr, offset + 8) as u64;
        let count = le_u32(&mbr, offset + 12) as u64;
        let Some((start_sector, sector_count)) = valid_partition(start, count, disk_sectors) else {
            continue;
        };
        if has_ext4_superblock(disk, start_sector, sector_count) {
            partitions.push(PartitionInfo {
                index: index + 1,
                start_sector,
                sector_count,
            });
        }
    }
    partitions
}

fn scan_gpt_ext4(disk: &AhciBlock, disk_sectors: u64) -> alloc::vec::Vec<PartitionInfo> {
    let header = read_sector(disk, GPT_HEADER_LBA);
    if &header[..GPT_SIGNATURE.len()] != GPT_SIGNATURE {
        return alloc::vec::Vec::new();
    }

    let entries_lba = le_u64(&header, 72);
    let entry_count = le_u32(&header, 80).min(128) as usize;
    let entry_size = le_u32(&header, 84) as usize;
    if entry_size < 128 || entry_size > BLOCK_SZ || !entry_size.is_power_of_two() {
        return alloc::vec::Vec::new();
    }

    let mut partitions = alloc::vec::Vec::new();
    let Some(entries_byte_base) = entries_lba.checked_mul(BLOCK_SZ as u64) else {
        return partitions;
    };
    for index in 0..entry_count {
        let Some(entry_byte) = (index as u64)
            .checked_mul(entry_size as u64)
            .and_then(|offset| entries_byte_base.checked_add(offset))
        else {
            break;
        };
        let sector_lba = entry_byte / BLOCK_SZ as u64;
        let sector_offset = (entry_byte % BLOCK_SZ as u64) as usize;
        if sector_offset + 48 > BLOCK_SZ || sector_lba >= disk_sectors {
            continue;
        }
        let sector = read_sector(disk, sector_lba as usize);
        let entry = &sector[sector_offset..];
        if entry[..16].iter().all(|byte| *byte == 0) {
            continue;
        }
        let first = le_u64(entry, 32);
        let last = le_u64(entry, 40);
        let Some(count) = last.checked_sub(first).and_then(|span| span.checked_add(1)) else {
            continue;
        };
        let Some((start_sector, sector_count)) = valid_partition(first, count, disk_sectors) else {
            continue;
        };
        if has_ext4_superblock(disk, start_sector, sector_count) {
            partitions.push(PartitionInfo {
                index: index + 1,
                start_sector,
                sector_count,
            });
        }
    }
    partitions
}

impl AhciBlock {
    /// Construct an adapter for an exclusively owned AHCI MMIO register block.
    ///
    /// # Safety
    ///
    /// `mmio_base` must be a valid uncached virtual mapping of a working AHCI
    /// controller, and no other driver may access the controller concurrently.
    unsafe fn try_new(mmio_base: usize) -> Option<Self> {
        // SAFETY: the FDT resource and exclusive platform probe establish the
        // requirements documented by this constructor.
        let driver = unsafe { AhciDriver::<CosmOsAhciHal>::try_new(mmio_base) }?;
        (driver.block_size() == BLOCK_SZ).then_some(Self {
            driver: SpinNoIrqLock::new(driver),
        })
    }

    fn capacity(&self) -> u64 {
        self.driver.lock().capacity()
    }

    fn read_range(&self, start_block: usize, output: &mut [u8]) {
        assert_eq!(output.len() % BLOCK_SZ, 0);
        let mut driver = self.driver.lock();
        for (chunk_index, chunk) in output.chunks_mut(MAX_TRANSFER_BYTES).enumerate() {
            let mut dma = DmaBuffer::new(chunk.len()).expect("AHCI read DMA allocation failed");
            let dma_slice = dma.as_mut_slice();
            let block = start_block + chunk_index * (MAX_TRANSFER_BYTES / BLOCK_SZ);
            assert!(driver.read(block as u64, dma_slice), "AHCI read failed");
            chunk.copy_from_slice(dma_slice);
        }
    }

    fn write_range(&self, start_block: usize, input: &[u8]) {
        assert_eq!(input.len() % BLOCK_SZ, 0);
        let mut driver = self.driver.lock();
        for (chunk_index, chunk) in input.chunks(MAX_TRANSFER_BYTES).enumerate() {
            let mut dma = DmaBuffer::new(chunk.len()).expect("AHCI write DMA allocation failed");
            let dma_slice = dma.as_mut_slice();
            dma_slice.copy_from_slice(chunk);
            let block = start_block + chunk_index * (MAX_TRANSFER_BYTES / BLOCK_SZ);
            assert!(driver.write(block as u64, dma_slice), "AHCI write failed");
        }
    }
}

impl BlockDevice for AhciBlock {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        self.read_range(block_id, buf);
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) {
        self.write_range(block_id, buf);
    }

    fn read_blocks(&self, start_block: usize, buf: &mut [u8]) {
        self.read_range(start_block, buf);
    }

    fn write_blocks(&self, start_block: usize, buf: &[u8]) {
        self.write_range(start_block, buf);
    }
}

/// Probe the FDT-selected AHCI controller and register it as the primary disk.
pub fn probe_ahci() {
    let Some(resource) = crate::boot::context::get().devices().ahci() else {
        return;
    };
    let base = crate::platform::mmio_phys_to_virt(resource.start);
    println!(
        "[ahci] probing controller at pa={:#x} size={:#x}",
        resource.start, resource.size
    );
    // SAFETY: early OF discovery accepted an enabled AHCI-compatible FDT node, the MMIO
    // direct map covers its `reg`, and this one-time probe owns the controller.
    let Some(device) = (unsafe { AhciBlock::try_new(base) }) else {
        panic!(
            "[ahci] failed to initialize controller at {:#x}",
            resource.start
        );
    };
    let sectors = device.capacity();
    let name = block_device_name(0);
    let device = Arc::new(device);
    let mut ext4_partitions = scan_mbr_ext4(&device, sectors);
    if ext4_partitions.is_empty() {
        ext4_partitions = scan_gpt_ext4(&device, sectors);
    }

    let mut devices = BLOCK_DEVICES.lock();
    devices.insert(name.clone(), device.clone());
    println!(
        "[ahci] {} online: {} sectors ({} MiB)",
        name,
        sectors,
        sectors.saturating_mul(BLOCK_SZ as u64) / (1024 * 1024)
    );
    if has_ext4_superblock(&device, 0, sectors as usize) {
        println!("[ahci] {} contains a whole-disk ext4 filesystem", name);
    }
    for partition in ext4_partitions {
        let partition_name = alloc::format!("{}{}", name, partition.index);
        println!(
            "[ahci] {} ext4: start={} sectors={} ({} MiB)",
            partition_name,
            partition.start_sector,
            partition.sector_count,
            partition.sector_count.saturating_mul(BLOCK_SZ) / (1024 * 1024)
        );
        devices.insert(
            partition_name,
            Arc::new(AhciPartition {
                disk: device.clone(),
                start_sector: partition.start_sector,
                sector_count: partition.sector_count,
            }),
        );
    }
}
