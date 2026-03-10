mod pack;
mod source;

use clap::{Arg, Command};
use pack::{FsFormat, PackConfig};
use std::path::PathBuf;

fn main() {
    run().expect("Error when packing filesystem image!");
}

fn run() -> std::io::Result<()> {
    let matches = Command::new("Filesystem image packer")
        .arg(
            Arg::new("source")
                .short('s')
                .long("source")
                .num_args(1)
                .required(true)
                .help("Executable source dir"),
        )
        .arg(
            Arg::new("target")
                .short('t')
                .long("target")
                .num_args(1)
                .required(true)
                .help("Output image path (e.g. fs-fuse/target/fs.img)"),
        )
        .arg(
            Arg::new("format")
                .short('f')
                .long("format")
                .num_args(1)
                .default_value("easyfs")
                .value_parser(["easyfs", "fat32", "ext4"])
                .help("Filesystem format to pack: easyfs | fat32 | ext4"),
        )
        .arg(
            Arg::new("size_mib")
                .long("size-mib")
                .num_args(1)
                .value_parser(clap::value_parser!(u64))
                .help("Image size in MiB (default: easyfs=16, fat32=64, ext4=64)"),
        )
        .arg(
            Arg::new("ext4_base")
                .long("ext4-base")
                .num_args(1)
                .help("Existing ext4 image used as base (ext4 only)"),
        )
        .get_matches();

    let src_path = PathBuf::from(
        matches
            .get_one::<String>("source")
            .expect("Missing source path"),
    );
    let img_path = PathBuf::from(
        matches
            .get_one::<String>("target")
            .expect("Missing target path"),
    );
    let format_str = matches
        .get_one::<String>("format")
        .expect("Missing format");
    let format = FsFormat::from_str(format_str).expect("Invalid format value");
    let ext4_base_img = matches.get_one::<String>("ext4_base").map(PathBuf::from);
    if ext4_base_img.is_some() && format != FsFormat::Ext4 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "--ext4-base can only be used with --format ext4",
        ));
    }

    let default_size_mib = match format {
        FsFormat::EasyFs => 16,
        // FAT32 needs a sufficiently large volume to be standards-compliant.
        // 64MiB is a safe default for FAT32 on common formatters.
        FsFormat::Fat32 => 64,
        // ext4 also benefits from a larger default size for metadata overhead.
        FsFormat::Ext4 => 64,
    };
    let size_mib = matches
        .get_one::<u64>("size_mib")
        .copied()
        .unwrap_or(default_size_mib);

    let cfg = PackConfig {
        src_path,
        img_path,
        image_size_bytes: size_mib * 1024 * 1024,
        ext4_base_img,
    };

    let case_insensitive = format == FsFormat::Fat32;
    let apps = source::collect_apps(&cfg.src_path, case_insensitive)?;

    pack::ensure_parent_dir(&cfg.img_path)?;
    pack::print_header(format, &cfg, &apps);

    let result = match format {
        FsFormat::EasyFs => pack::easyfs::pack(&cfg, &apps),
        FsFormat::Fat32 => pack::fat32::pack(&cfg, &apps),
        FsFormat::Ext4 => pack::ext4::pack(&cfg, &apps),
    };

    if let Err(e) = result {
        if format == FsFormat::Fat32 {
            eprintln!(
                "FAT32 packing failed: {e}. If your image is too small, try a larger --size-mib (e.g. 64)."
            );
        } else if format == FsFormat::Ext4 {
            eprintln!(
                "EXT4 packing failed: {e}. Ensure `mkfs.ext4` and `debugfs` are installed (e2fsprogs)."
            );
        }
        return Err(e);
    }

    Ok(())
}
