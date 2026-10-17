// ==============================================================================
//  Subtitles Detection & On-the-fly SRT to WebVTT Conversion
// ==============================================================================

use std::fs;
use std::path::Path;
use crate::scanner::SubtitleItem;

pub const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "vtt"];

/// Detects subtitle files matching a video's filename in the same directory.
/// E.g. for `movie.mp4`, matches `movie.srt`, `movie.en.srt`, `movie_fa.vtt`, `movie-fr.srt`.
pub fn detect_subtitles(video_path: &Path, root: &Path) -> Vec<SubtitleItem> {
    let parent = match video_path.parent() {
        Some(p) => p,
        None => return Vec::new(),
    };

    let video_stem = match video_path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let video_stem_lower = video_stem.to_lowercase();

    let entries = match fs::read_dir(parent) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut subs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let file_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext_lower = ext.to_lowercase();
            if SUBTITLE_EXTENSIONS.contains(&ext_lower.as_str()) {
                let name_lower = file_name.to_lowercase();
                if name_lower.starts_with(&video_stem_lower) {
                    let remainder = &name_lower[video_stem_lower.len()..];
                    // Remainder must be empty before extension delimiter (e.g. .srt)
                    // or start with '.', '_', or '-' (e.g. .en.srt, _fa.vtt, -es.srt)
                    if remainder.starts_with('.') || remainder.starts_with('_') || remainder.starts_with('-') {
                        let rel = path.strip_prefix(root).unwrap_or(&path);
                        let rel_str = rel.to_string_lossy().replace('\\', "/");
                        subs.push(SubtitleItem {
                            name: file_name.to_string(),
                            rel_path: rel_str,
                            format: ext_lower,
                        });
                    }
                }
            }
        }
    }

    subs.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    subs
}

/// Converts SRT subtitle content to WebVTT format on-the-fly.
pub fn srt_to_vtt(srt: &str) -> String {
    let clean = srt.trim_start_matches('\u{feff}'); // Remove BOM if present
    if clean.trim_start().starts_with("WEBVTT") {
        return clean.to_string();
    }

    let mut vtt = String::from("WEBVTT\n\n");
    let mut in_cues = false;

    for line in clean.lines() {
        let line_trimmed = line.trim_end();
        if line_trimmed.contains("-->") {
            in_cues = true;
            // Convert timestamps: 00:00:20,000 --> 00:00:24,400 to 00:00:20.000 --> 00:00:24.400
            let converted_line = line_trimmed.replace(',', ".");
            vtt.push_str(&converted_line);
            vtt.push('\n');
        } else {
            vtt.push_str(line_trimmed);
            vtt.push('\n');
        }
    }

    if !in_cues && vtt == "WEBVTT\n\n" {
        return "WEBVTT\n".to_string();
    }

    vtt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_srt_to_vtt_conversion() {
        let srt = "1\r\n00:00:01,000 --> 00:00:04,500\r\nHello World\r\n\r\n2\r\n00:00:05,200 --> 00:00:08,700\r\nSecond line\r\n";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.starts_with("WEBVTT\n\n"));
        assert!(vtt.contains("00:00:01.000 --> 00:00:04.500"));
        assert!(vtt.contains("Hello World"));
        assert!(vtt.contains("00:00:05.200 --> 00:00:08.700"));
    }

    #[test]
    fn test_srt_to_vtt_already_vtt() {
        let original = "WEBVTT\n\n00:00:01.000 --> 00:00:04.000\nAlready VTT\n";
        let vtt = srt_to_vtt(original);
        assert_eq!(vtt, original);
    }
}
