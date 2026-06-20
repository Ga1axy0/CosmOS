use alloc::sync::Arc;

use crate::block_cache::get_block_cache;
use crate::block_dev::BlockDevice;
use crate::errno::FS_ERRNO;
use crate::BLOCK_SZ;

/// Parsed FAT32 BIOS Parameter Block (BPB) with derived fields.
#[derive(Clone, Debug)]
pub struct Fat32Bpb {
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sectors: u16,
    pub num_fats: u8,
    pub total_sectors: u32,
    pub fat_sectors: u32,
    pub root_cluster: u32,

    /// LBA of first FAT.
    pub fat_start_lba: u32,
    /// LBA of first data sector.
    pub data_start_lba: u32,
    /// Maximum cluster number (inclusive).
    pub max_cluster: u32,
}

impl Fat32Bpb {
    pub fn read_from(block_device: &Arc<dyn BlockDevice>) -> Result<Self, FS_ERRNO> {
        let mut sector = [0u8; BLOCK_SZ];
        get_block_cache(0, Arc::clone(block_device))
            .lock()
            .read_bytes(0, &mut sector);

        let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]);
        let sectors_per_cluster = sector[13];
        let reserved_sectors = u16::from_le_bytes([sector[14], sector[15]]);
        let num_fats = sector[16];

        let total_sectors_16 = u16::from_le_bytes([sector[19], sector[20]]) as u32;
        let total_sectors_32 = u32::from_le_bytes([sector[32], sector[33], sector[34], sector[35]]);
        let total_sectors = if total_sectors_16 != 0 {
            total_sectors_16
        } else {
            total_sectors_32
        };

        let fat_sz_16 = u16::from_le_bytes([sector[22], sector[23]]) as u32;
        let fat_sz_32 = u32::from_le_bytes([sector[36], sector[37], sector[38], sector[39]]);
        let fat_sectors = if fat_sz_16 != 0 { fat_sz_16 } else { fat_sz_32 };

        let root_cluster = u32::from_le_bytes([sector[44], sector[45], sector[46], sector[47]]);

        if bytes_per_sector as usize != BLOCK_SZ {
            return Err(FS_ERRNO::EINVAL);
        }
        if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
            return Err(FS_ERRNO::EINVAL);
        }
        if num_fats < 1 || fat_sectors == 0 {
            return Err(FS_ERRNO::EINVAL);
        }

        let fat_start_lba = reserved_sectors as u32;
        let fat_span = (num_fats as u32)
            .checked_mul(fat_sectors)
            .ok_or(FS_ERRNO::EINVAL)?;
        let data_start_lba = fat_start_lba
            .checked_add(fat_span)
            .ok_or(FS_ERRNO::EINVAL)?;

        let data_sectors = total_sectors
            .checked_sub(data_start_lba)
            .ok_or(FS_ERRNO::EINVAL)?;
        let total_clusters = data_sectors / sectors_per_cluster as u32;
        // Cluster numbers start at 2.
        let max_cluster = total_clusters.checked_add(1).ok_or(FS_ERRNO::EINVAL)?;
        if root_cluster < 2 || root_cluster > max_cluster {
            return Err(FS_ERRNO::EINVAL);
        }

        Ok(Self {
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            num_fats,
            total_sectors,
            fat_sectors,
            root_cluster,
            fat_start_lba,
            data_start_lba,
            max_cluster,
        })
    }

    #[inline]
    pub fn cluster_size_bytes(&self) -> usize {
        self.sectors_per_cluster as usize * BLOCK_SZ
    }

    #[inline]
    pub fn first_sector_of_cluster(&self, cluster: u32) -> u32 {
        assert!(cluster >= 2);
        self.data_start_lba + (cluster - 2) * self.sectors_per_cluster as u32
    }
}
