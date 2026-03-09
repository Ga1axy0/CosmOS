use crate::pack::PackConfig;
use crate::source::AppFile;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run_checked(cmd: &mut Command, what: &str) -> io::Result<()> {
    let output = cmd.output()?;
    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(io::Error::other(format!(
        "{what} failed (status: {}).\nstdout: {}\nstderr: {}",
        output.status, stdout, stderr
    )))
}

fn resolve_tool(tool: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            candidates.push(dir.join(tool));
        }
    }

    for dir in [
        "/opt/homebrew/opt/e2fsprogs/sbin",
        "/opt/homebrew/sbin",
        "/usr/local/sbin",
        "/usr/sbin",
        "/sbin",
    ] {
        candidates.push(Path::new(dir).join(tool));
    }

    candidates.into_iter().find(|p| p.is_file())
}

/// Pack files into a raw ext4 image.
///
/// This implementation uses host tools from e2fsprogs:
/// - `mkfs.ext4` to format the image
/// - `debugfs` to write files into the root directory
pub fn pack(cfg: &PackConfig, apps: &[AppFile]) -> io::Result<()> {
    let debugfs = resolve_tool("debugfs").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Cannot find `debugfs`. Install e2fsprogs or add it to PATH.",
        )
    })?;

    if let Some(base_img) = &cfg.ext4_base_img {
        if !base_img.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Base ext4 image not found: {}", base_img.display()),
            ));
        }
        if base_img != &cfg.img_path {
            fs::copy(base_img, &cfg.img_path).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "Failed to copy base image from {} to {}: {e}",
                        base_img.display(),
                        cfg.img_path.display()
                    ),
                )
            })?;
        }
    } else {
        let mkfs_ext4 = resolve_tool("mkfs.ext4").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Cannot find `mkfs.ext4`. Install e2fsprogs or add it to PATH.",
            )
        })?;

        let img = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&cfg.img_path)?;
        img.set_len(cfg.image_size_bytes)?;
        drop(img);

        run_checked(
            Command::new(mkfs_ext4)
                .arg("-q")
                .arg("-F")
                .arg("-b")
                .arg("4096")
                .arg("-L")
                .arg("OSDISK")
                .arg(&cfg.img_path),
            "mkfs.ext4",
        )?;
    }

    for app in apps {
        println!("Adding file: {}", app.name);
        let _ = Command::new(&debugfs)
            .arg("-w")
            .arg("-R")
            .arg(format!("rm /{}", app.name))
            .arg(&cfg.img_path)
            .output();
        run_checked(
            Command::new(&debugfs)
                .arg("-w")
                .arg("-R")
                .arg(format!("write {} /{}", app.host_path.display(), app.name))
                .arg(&cfg.img_path),
            &format!("debugfs write {}", app.name),
        )?;
    }

    // 打包完成后，打印镜像元数据
    println!("\n==== ext4 镜像元数据 ====");
    // 镜像文件大小
    let img_metadata = std::fs::metadata(&cfg.img_path)?;
    println!("镜像文件路径: {}", cfg.img_path.display());
    println!(
        "镜像文件大小: {} bytes ({:.2} MiB)",
        img_metadata.len(),
        img_metadata.len() as f64 / 1024.0 / 1024.0
    );

    // 使用 debugfs 查询分区空间和 block 使用情况
    let output = Command::new(&debugfs)
        .arg("-R")
        .arg("stats")
        .arg(&cfg.img_path)
        .output()?;
    println!(
        "debugfs stats:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    let output_df = Command::new(&debugfs)
        .arg("-R")
        .arg("df")
        .arg(&cfg.img_path)
        .output()?;
    println!(
        "debugfs df:\n{}",
        String::from_utf8_lossy(&output_df.stdout)
    );

    // 查询根目录文件列表及大小
    let output_ls = Command::new(&debugfs)
        .arg("-R")
        .arg("ls -l /")
        .arg(&cfg.img_path)
        .output()?;
    println!(
        "debugfs ls -l /:\n{}",
        String::from_utf8_lossy(&output_ls.stdout)
    );

    Ok(())
}
