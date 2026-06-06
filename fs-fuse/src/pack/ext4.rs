use crate::pack::PackConfig;
use crate::source::AppFile;
use std::env;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const LINUX_DIRS: &[&str] = &[
    "/bin", "/etc", "/home", "/root", "/tmp", "/usr", "/usr/bin", "/usr/lib", "/var",
];

const RUNTIME_DIRS: &[&str] = &["/musl", "/musl/lib", "/glibc", "/glibc/lib"];
const LTP_DATA_DIRS: &[&str] = &[
    "/musl/ltp/testcases/bin/datafiles",
    "/glibc/ltp/testcases/bin/datafiles",
];

const ROOT_SEED_APPS: &[&str] = &["initproc", "setupsh", "sh", "bash", "busybox"];

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

fn run_debugfs_ignore_error(debugfs: &Path, img_path: &Path, command: String) {
    let _ = Command::new(debugfs)
        .arg("-w")
        .arg("-R")
        .arg(command)
        .arg(img_path)
        .output();
}

fn run_debugfs_checked(
    debugfs: &Path,
    img_path: &Path,
    command: String,
    what: &str,
) -> io::Result<()> {
    run_checked(
        Command::new(debugfs)
            .arg("-w")
            .arg("-R")
            .arg(command)
            .arg(img_path),
        what,
    )
}

fn is_root_seed_app(name: &str) -> bool {
    ROOT_SEED_APPS.contains(&name)
}

fn image_target_path(app: &AppFile) -> String {
    if is_root_seed_app(app.name.as_str()) {
        format!("/{}", app.name)
    } else {
        format!("/root/{}", app.name)
    }
}

fn repo_root_from_source(src_path: &Path) -> io::Result<PathBuf> {
    let src_path = fs::canonicalize(src_path)?;
    src_path
        .ancestors()
        .nth(4)
        .map(Path::to_path_buf)
        .ok_or_else(|| io::Error::other(format!("cannot infer repo root from {}", src_path.display())))
}

fn extra_runtime_files(src_path: &Path) -> io::Result<Vec<(PathBuf, &'static str)>> {
    let repo_root = repo_root_from_source(src_path)?;
    let candidates = [
        (repo_root.join("lib/musl/ar"), "/musl/lib/ar"),
        (repo_root.join("lib/glibc/ar"), "/glibc/lib/ar"),
    ];

    Ok(candidates
        .iter()
        .cloned()
        .filter(|(host_path, _)| host_path.is_file())
        .collect())
}

fn extra_ltp_datafiles(src_path: &Path) -> io::Result<Vec<(PathBuf, String)>> {
    let repo_root = repo_root_from_source(src_path)?;
    let host_dir = repo_root
        .parent()
        .ok_or_else(|| io::Error::other(format!("cannot infer sibling test suite from {}", repo_root.display())))?
        .join("testsuits-for-oskernel/ltp-full-20240524/testcases/commands/ar/datafiles");

    if !host_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    for entry in fs::read_dir(host_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".in") {
            continue;
        }
        files.push((path.clone(), format!("/musl/ltp/testcases/data/ar01/{name}")));
        files.push((path.clone(), format!("/glibc/ltp/testcases/data/ar01/{name}")));
        files.push((path.clone(), format!("/musl/ltp/testcases/bin/datafiles/{name}")));
        files.push((path.clone(), format!("/glibc/ltp/testcases/bin/datafiles/{name}")));
    }
    Ok(files)
}

/// Pack files into a raw ext4 image.
///
/// This implementation uses host tools from e2fsprogs:
/// - `mkfs.ext4` to format the image
/// - `debugfs` to write files into a Linux-style directory layout
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

    for dir in LINUX_DIRS {
        run_debugfs_ignore_error(&debugfs, &cfg.img_path, format!("mkdir {}", dir));
    }
    for dir in RUNTIME_DIRS {
        run_debugfs_ignore_error(&debugfs, &cfg.img_path, format!("mkdir {}", dir));
    }
    for dir in LTP_DATA_DIRS {
        run_debugfs_ignore_error(&debugfs, &cfg.img_path, format!("mkdir {}", dir));
    }

    for app in apps {
        let target_path = image_target_path(app);
        println!("Adding file: {} -> {}", app.name, target_path);
        run_debugfs_ignore_error(&debugfs, &cfg.img_path, format!("rm /{}", app.name));
        run_debugfs_ignore_error(&debugfs, &cfg.img_path, format!("rm /root/{}", app.name));
        run_debugfs_checked(
            &debugfs,
            &cfg.img_path,
            format!("write {} {}", app.host_path.display(), target_path),
            &format!("debugfs write {}", target_path),
        )?;
    }

    for (host_path, target_path) in extra_runtime_files(&cfg.src_path)? {
        println!("Adding runtime file: {} -> {}", host_path.display(), target_path);
        run_debugfs_ignore_error(&debugfs, &cfg.img_path, format!("rm {}", target_path));
        run_debugfs_checked(
            &debugfs,
            &cfg.img_path,
            format!("write {} {}", host_path.display(), target_path),
            &format!("debugfs write {}", target_path),
        )?;
    }

    for (host_path, target_path) in extra_ltp_datafiles(&cfg.src_path)? {
        println!("Adding LTP datafile: {} -> {}", host_path.display(), target_path);
        run_debugfs_ignore_error(&debugfs, &cfg.img_path, format!("rm {}", target_path));
        run_debugfs_checked(
            &debugfs,
            &cfg.img_path,
            format!("write {} {}", host_path.display(), target_path),
            &format!("debugfs write {}", target_path),
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

    let output_ls_root = Command::new(&debugfs)
        .arg("-R")
        .arg("ls -l /root")
        .arg(&cfg.img_path)
        .output()?;
    println!(
        "debugfs ls -l /root:\n{}",
        String::from_utf8_lossy(&output_ls_root.stdout)
    );

    Ok(())
}
