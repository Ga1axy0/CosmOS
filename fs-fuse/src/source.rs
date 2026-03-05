use std::collections::HashSet;
use std::fs::read_dir;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AppFile {
    pub name: String,
    pub host_path: PathBuf,
}

/// Collect app-like files from `src_path`.
///
/// Current policy (kept consistent with the original implementation):
/// - Only regular files
/// - Skip hidden files (name starts with '.')
/// - Only files without extension
/// - Name is `file_stem` (for no-extension files, it's the full name)
///
/// If `case_insensitive_dedup` is true, names are deduplicated by lowercasing
/// (useful for FAT which is case-insensitive).
pub fn collect_apps(src_path: &Path, case_insensitive_dedup: bool) -> io::Result<Vec<AppFile>> {
    let mut out: Vec<AppFile> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    for entry in read_dir(src_path)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let file_name = match path.file_name().and_then(|s| s.to_str()) {
            Some(s) => s,
            None => continue,
        };
        if file_name.starts_with('.') {
            continue;
        }

        if path.extension().is_some() {
            continue;
        }

        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };

        let key = if case_insensitive_dedup {
            name.to_ascii_lowercase()
        } else {
            name.to_string()
        };

        if !seen.insert(key) {
            // Keep behavior deterministic: keep the first, ignore later duplicates.
            // Caller prints warnings if desired.
            continue;
        }

        out.push(AppFile {
            name: name.to_string(),
            host_path: path,
        });
    }

    // Deterministic output for reproducible images.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}
