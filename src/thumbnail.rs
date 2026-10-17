// ==============================================================================
//  Thumbnails (FFmpeg on-demand or fallback, DJB2 hashing)
// ==============================================================================

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::process::Command;
use crate::ServerConfig;
use crate::scanner::resolve_safe_path;
use crate::streamer::CHUNK_SIZE;
use crate::http::send_response;

/// Simple fast DJB2 hash converted to 16-char hex string
pub fn sanitize_filename_hash(rel_path: &str) -> String {
    let mut hash: u64 = 5381;
    for byte in rel_path.bytes() {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(byte as u64);
    }
    format!("{:016x}", hash)
}

pub fn pregenerate_thumbnails(config: ServerConfig) {
    if !config.has_ffmpeg {
        return;
    }
    let videos = config.videos.lock().unwrap().clone();
    for v in videos {
        let thumb_filename = format!("{}.jpg", sanitize_filename_hash(&v.rel_path));
        let thumb_path = config.thumbs_dir.join(&thumb_filename);
        if thumb_path.exists() {
            continue;
        }
        {
            let mut gen = config.generating_thumbs.lock().unwrap();
            if gen.contains(&v.rel_path) {
                continue;
            }
            gen.insert(v.rel_path.clone());
        }

        let video_path = match resolve_safe_path(&config.root_dir, &v.rel_path) {
            Some(p) => p,
            None => {
                let mut gen = config.generating_thumbs.lock().unwrap();
                gen.remove(&v.rel_path);
                continue;
            }
        };

        let _ = Command::new("ffmpeg")
            .arg("-ss")
            .arg("00:00:03")
            .arg("-i")
            .arg(&video_path)
            .arg("-vframes")
            .arg("1")
            .arg("-vf")
            .arg("scale=480:-2")
            .arg("-q:v")
            .arg("3")
            .arg("-y")
            .arg(&thumb_path)
            .output();

        {
            let mut gen = config.generating_thumbs.lock().unwrap();
            gen.remove(&v.rel_path);
        }
    }
}

pub fn serve_or_generate_thumbnail(
    stream: &mut TcpStream,
    config: &ServerConfig,
    rel_path: &str,
) -> io::Result<()> {
    let video_path = match resolve_safe_path(&config.root_dir, rel_path) {
        Some(p) => p,
        None => {
            return send_response(stream, 403, "Forbidden", "text/plain", b"Access denied");
        }
    };

    if !video_path.exists() {
        return send_response(stream, 404, "Not Found", "text/plain", b"Video file not found");
    }

    // Thumb filename based on hashed/sanitized rel_path
    let thumb_filename = format!("{}.jpg", sanitize_filename_hash(rel_path));
    let thumb_path = config.thumbs_dir.join(&thumb_filename);

    if thumb_path.exists() {
        let mut f = File::open(&thumb_path)?;
        let size = f.metadata()?.len();
        let headers = format!(
            "HTTP/1.1 200 OK\r\n\
            Content-Type: image/jpeg\r\n\
            Content-Length: {}\r\n\
            Cache-Control: public, max-age=86400\r\n\
            Access-Control-Allow-Origin: *\r\n\
            Connection: keep-alive\r\n\r\n",
            size
        );
        stream.write_all(headers.as_bytes())?;
        let mut buffer = [0u8; CHUNK_SIZE];
        loop {
            let n = f.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            stream.write_all(&buffer[..n])?;
        }
        return Ok(());
    }

    if !config.has_ffmpeg {
        // Return 404 so frontend knows to use its client-side canvas fallback
        return send_response(stream, 404, "Not Found", "text/plain", b"FFmpeg unavailable");
    }

    // Deduplicate thumbnail generation
    {
        let mut gen = config.generating_thumbs.lock().unwrap();
        if gen.contains(rel_path) {
            // Already generating, return 204
            return send_response(stream, 204, "No Content", "text/plain", b"Generating");
        }
        gen.insert(rel_path.to_string());
    }

    // Run ffmpeg to grab frame at 3 seconds
    let status = Command::new("ffmpeg")
        .arg("-ss")
        .arg("00:00:03")
        .arg("-i")
        .arg(&video_path)
        .arg("-vframes")
        .arg("1")
        .arg("-vf")
        .arg("scale=480:-2")
        .arg("-q:v")
        .arg("3")
        .arg("-y")
        .arg(&thumb_path)
        .output();

    {
        let mut gen = config.generating_thumbs.lock().unwrap();
        gen.remove(rel_path);
    }

    match status {
        Ok(out) if out.status.success() && thumb_path.exists() => {
            let mut f = File::open(&thumb_path)?;
            let size = f.metadata()?.len();
            let headers = format!(
                "HTTP/1.1 200 OK\r\n\
                Content-Type: image/jpeg\r\n\
                Content-Length: {}\r\n\
                Cache-Control: public, max-age=86400\r\n\
                Access-Control-Allow-Origin: *\r\n\
                Connection: keep-alive\r\n\r\n",
                size
            );
            stream.write_all(headers.as_bytes())?;
            let mut buffer = [0u8; CHUNK_SIZE];
            loop {
                let n = f.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                stream.write_all(&buffer[..n])?;
            }
            Ok(())
        }
        _ => {
            send_response(stream, 404, "Not Found", "text/plain", b"Thumbnail generation failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_djb2_hash() {
        let hash1 = sanitize_filename_hash("test/video.mp4");
        let hash2 = sanitize_filename_hash("test/video.mp4");
        let hash3 = sanitize_filename_hash("test/other.mp4");
        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
        assert_eq!(hash1.len(), 16);
    }
}
