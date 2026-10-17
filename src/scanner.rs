// ==============================================================================
//  Filesystem Scanning & Path Security
// ==============================================================================

use std::fs;
use std::path::{Path, PathBuf};

// Supported video extensions
pub const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "mov", "avi", "m4v", "flv", "ogv"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubtitleItem {
    pub name: String,
    pub rel_path: String,
    pub format: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoItem {
    pub name: String,
    pub rel_path: String,
    pub size: u64,
    pub size_fmt: String,
    pub subtitles: Vec<SubtitleItem>,
}

pub fn scan_videos(root: &Path) -> Vec<VideoItem> {
    let mut list = Vec::new();
    walk_directory(root, root, &mut list);
    // Sort alphabetically by name
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

pub fn walk_directory(current: &Path, root: &Path, list: &mut Vec<VideoItem>) {
    let entries = match fs::read_dir(current) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        // Skip hidden files/folders (e.g. .git, .playbox_thumbs)
        if file_name.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            walk_directory(&path, root, list);
        } else if path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                let ext_lower = ext.to_lowercase();
                if VIDEO_EXTENSIONS.contains(&ext_lower.as_str()) {
                    if let Ok(meta) = path.metadata() {
                        let rel = path.strip_prefix(root).unwrap_or(&path);
                        let rel_str = rel.to_string_lossy().replace('\\', "/");
                        let subtitles = crate::subtitles::detect_subtitles(&path, root);
                        list.push(VideoItem {
                            name: file_name.to_string(),
                            rel_path: rel_str,
                            size: meta.len(),
                            size_fmt: format_file_size(meta.len()),
                            subtitles,
                        });
                    }
                }
            }
        }
    }
}

pub fn resolve_safe_path(root: &Path, rel_path: &str) -> Option<PathBuf> {
    // Prevent path traversal
    let clean_rel = rel_path.trim_start_matches('/').replace('\\', "/");
    let joined = root.join(&clean_rel);

    // Cache canonicalized root: check joined path starts_with root (string/prefix) before canonicalize
    if !joined.starts_with(root) {
        return None;
    }

    // Canonicalize check
    let canon_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if let Ok(canonical) = fs::canonicalize(&joined) {
        if canonical.starts_with(&canon_root) {
            return Some(canonical);
        }
    }
    None
}

pub fn format_file_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_file_size() {
        assert_eq!(format_file_size(500), "500 B");
        assert_eq!(format_file_size(1024), "1 KB");
        assert_eq!(format_file_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_file_size(1024 * 1024 * 1024), "1.00 GB");
    }

    #[test]
    fn test_resolve_safe_path_traversal() {
        let cur = std::env::current_dir().unwrap();
        assert!(resolve_safe_path(&cur, "../../../etc/passwd").is_none());
        assert!(resolve_safe_path(&cur, "Cargo.toml").is_some());
    }
}
