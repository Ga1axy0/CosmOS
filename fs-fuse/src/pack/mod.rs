pub mod easyfs;
pub mod ext4;
pub mod fat32;

use std::path::Path;

use crate::source::AppFile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsFormat {
    EasyFs,
    Fat32,
    Ext4,
}

impl FsFormat {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "easyfs" | "easy-fs" | "easy_fs" => Some(Self::EasyFs),
            "fat32" | "fat" => Some(Self::Fat32),
            "ext4" | "ext" => Some(Self::Ext4),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::EasyFs => "easyfs",
            Self::Fat32 => "fat32",
            Self::Ext4 => "ext4",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PackConfig {
    pub src_path: std::path::PathBuf,
    pub img_path: std::path::PathBuf,
    pub image_size_bytes: u64,
    /// Optional base ext4 image to clone before writing user apps.
    /// Only used when format is ext4.
    pub ext4_base_img: Option<std::path::PathBuf>,
}

pub fn ensure_parent_dir(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

pub fn print_header(format: FsFormat, cfg: &PackConfig, apps: &[AppFile]) {
    println!(
        "{} Packing \x1b[1;38;5;45m{}\x1b[0m image {}",
        "-".repeat(10),
        format.as_str(),
        "-".repeat(10)
    );
    println!(
        "src_path = {}\ntarget_img = {}\nimage_size = {} bytes\nfile_count = {}",
        cfg.src_path.display(),
        cfg.img_path.display(),
        cfg.image_size_bytes,
        apps.len()
    );
}
