use fs::{BlockDevice, EasyFileSystem};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};
use std::convert::TryInto;

use crate::pack::PackConfig;
use crate::source::AppFile;

const BLOCK_SZ: usize = 512;

struct BlockFile(Mutex<File>);

impl BlockDevice for BlockFile {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        let mut file = self.0.lock().unwrap();
        file.seek(SeekFrom::Start((block_id * BLOCK_SZ) as u64))
            .expect("Error when seeking!");
        assert_eq!(file.read(buf).unwrap(), BLOCK_SZ, "Not a complete block!");
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) {
        let mut file = self.0.lock().unwrap();
        file.seek(SeekFrom::Start((block_id * BLOCK_SZ) as u64))
            .expect("Error when seeking!");
        assert_eq!(file.write(buf).unwrap(), BLOCK_SZ, "Not a complete block!");
    }
}

pub fn pack(cfg: &PackConfig, apps: &[AppFile]) -> std::io::Result<()> {
    let block_file = Arc::new(BlockFile(Mutex::new({
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&cfg.img_path)?;
        f.set_len(cfg.image_size_bytes)?;
        f
    })));

    let total_blocks: u32 = ((cfg.image_size_bytes as usize) / BLOCK_SZ)
        .try_into()
        .expect("image is too large (block count overflows u32)");

    // NOTE: The on-disk layout is defined by the `fs` crate (easy-fs).
    // The last argument is the starting data block (kept as original: 1).
    let efs = EasyFileSystem::create(block_file, total_blocks, 1);
    let root_inode = EasyFileSystem::root_inode(&efs);

    for app in apps {
        let mut host_file = File::open(&app.host_path).unwrap_or_else(|_| {
            panic!(
                "Fail to open host file {} for app [{}]",
                app.host_path.display(),
                app.name
            )
        });

        let mut all_data: Vec<u8> = Vec::new();
        host_file.read_to_end(&mut all_data).unwrap();

        let inode = root_inode
            .create(&app.name)
            .unwrap_or_else(|| panic!("Fail to create inode for {}", app.name));

        println!(
            "Adding file: {} ({} bytes)",
            app.name,
            all_data.len()
        );
        inode.write_at(0, all_data.as_slice());
    }

    Ok(())
}
