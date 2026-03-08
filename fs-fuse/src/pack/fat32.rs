use crate::pack::PackConfig;
use crate::source::AppFile;
use rand::Rng;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};

/// Pack files into a **raw FAT32 volume** (no MBR/GPT partition table).
///
/// Implementation note:
/// We intentionally rely on the third-party `fatfs` crate to format/write the
/// filesystem so that the generated image is standards-compliant.
pub fn pack(cfg: &PackConfig, apps: &[AppFile]) -> std::io::Result<()> {
    // Create/truncate image and pre-size it.
    let mut img = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&cfg.img_path)?;
    img.set_len(cfg.image_size_bytes)?;

    // Format as a FAT32 volume directly on the image (sector 0 is BPB).
    let volume_id: u32 = rand::rng().random();

    let fmt_opts = fatfs::FormatVolumeOptions::new()
        .bytes_per_sector(512)
        .bytes_per_cluster(512)
        .fat_type(fatfs::FatType::Fat32)
        .volume_id(volume_id)
        .volume_label(*b"OSDISK     ");

    fatfs::format_volume(&mut img, fmt_opts)
        .map_err(std::io::Error::other)?;

    // Rewind before mounting.
    img.seek(SeekFrom::Start(0))?;

    // Mount and populate root directory.
    let fs = fatfs::FileSystem::new(img, fatfs::FsOptions::new())
        .map_err(std::io::Error::other)?;

    {
        let root = fs.root_dir();

        for app in apps {
            let mut host_file = std::fs::File::open(&app.host_path)?;
            let mut all_data: Vec<u8> = Vec::new();
            host_file.read_to_end(&mut all_data)?;

            println!("Adding file: {} ({} bytes)", app.name, all_data.len());

            // `fatfs` supports long file names with the `lfn` feature enabled.
            let mut f = root
                .create_file(&app.name)
                .map_err(std::io::Error::other)?;
            f.write_all(&all_data)?;
            f.flush()?;
        }
    } // root is dropped here

    let stats = fs.stats().unwrap();
    let (cluster_size, total_clusters, free_clusters) = (stats.cluster_size(), stats.total_clusters(), stats.free_clusters());
    println!("Packing complete! Cluster size: {}, Total clusters: {}, Free clusters: {} (Used: {:<.2}%)",
        stats.cluster_size(), stats.total_clusters(), stats.free_clusters(), (total_clusters - free_clusters) as f32 /total_clusters as f32 * 100.0);
        
    // Ensure filesystem structures are written.
    fs.unmount()
        .map_err(std::io::Error::other)?;

    Ok(())
}
