// ==============================================================================
//  Video Streaming with HTTP 206 Partial Content (Range Requests)
// ==============================================================================

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use crate::ServerConfig;
use crate::scanner::resolve_safe_path;
use crate::http::{get_mime_type, send_response};

pub const CHUNK_SIZE: usize = 256 * 1024; // 256 KB streaming buffer

pub fn stream_video(
    stream: &mut TcpStream,
    config: &ServerConfig,
    rel_path: &str,
    range_header: Option<&str>,
) -> io::Result<()> {
    let full_path = match resolve_safe_path(&config.root_dir, rel_path) {
        Some(p) => p,
        None => {
            return send_response(stream, 403, "Forbidden", "text/plain", b"Access denied");
        }
    };

    let mut file = match File::open(&full_path) {
        Ok(f) => f,
        Err(_) => {
            return send_response(stream, 404, "Not Found", "text/plain", b"Video not found");
        }
    };

    let total_size = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => {
            return send_response(stream, 500, "Internal Server Error", "text/plain", b"Error reading metadata");
        }
    };

    let mime_type = get_mime_type(&full_path);

    if let Some(range_str) = range_header {
        // Expected format: "bytes=start-end" or "bytes=start-" or "bytes=-suffix"
        if let Some((start, end)) = parse_range(range_str, total_size) {
            let content_len = end - start + 1;

            if let Err(e) = file.seek(SeekFrom::Start(start)) {
                return Err(e);
            }

            let response_headers = format!(
                "HTTP/1.1 206 Partial Content\r\n\
                Content-Type: {}\r\n\
                Content-Range: bytes {}-{}/{}\r\n\
                Content-Length: {}\r\n\
                Accept-Ranges: bytes\r\n\
                Connection: keep-alive\r\n\r\n",
                mime_type, start, end, total_size, content_len
            );

            stream.write_all(response_headers.as_bytes())?;

            let mut remaining = content_len;
            let mut buffer = [0u8; CHUNK_SIZE];

            while remaining > 0 {
                let to_read = (buffer.len() as u64).min(remaining) as usize;
                let bytes_read = file.read(&mut buffer[..to_read])?;
                if bytes_read == 0 {
                    break;
                }
                if let Err(_) = stream.write_all(&buffer[..bytes_read]) {
                    // Client disconnected or seeked away (normal during video scrubbing)
                    break;
                }
                remaining -= bytes_read as u64;
            }
            return Ok(());
        } else {
            // Invalid range
            let resp = format!(
                "HTTP/1.1 416 Range Not Satisfiable\r\n\
                Content-Range: bytes */{}\r\n\
                Content-Length: 0\r\n\r\n",
                total_size
            );
            return stream.write_all(resp.as_bytes());
        }
    }

    // No range requested -> full file
    let headers = format!(
        "HTTP/1.1 200 OK\r\n\
        Content-Type: {}\r\n\
        Content-Length: {}\r\n\
        Accept-Ranges: bytes\r\n\
        Connection: keep-alive\r\n\r\n",
        mime_type, total_size
    );
    stream.write_all(headers.as_bytes())?;

    let mut buffer = [0u8; CHUNK_SIZE];
    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        if let Err(_) = stream.write_all(&buffer[..bytes_read]) {
            break;
        }
    }

    Ok(())
}

pub fn parse_range(range_str: &str, total_size: u64) -> Option<(u64, u64)> {
    let clean = range_str.trim();
    if !clean.starts_with("bytes=") {
        return None;
    }
    let range_val = &clean["bytes=".len()..];
    let parts: Vec<&str> = range_val.split('-').collect();

    if parts.len() != 2 {
        return None;
    }

    if parts[0].is_empty() {
        // Suffix range: "-500" means last 500 bytes
        let suffix: u64 = parts[1].parse().ok()?;
        let start = total_size.saturating_sub(suffix);
        let end = total_size.saturating_sub(1);
        Some((start, end))
    } else if parts[1].is_empty() {
        // Prefix range: "500-" means from 500 to end
        let start: u64 = parts[0].parse().ok()?;
        if start >= total_size {
            return None;
        }
        Some((start, total_size - 1))
    } else {
        // Closed range: "500-999"
        let start: u64 = parts[0].parse().ok()?;
        let end: u64 = parts[1].parse().ok()?;
        if start > end || start >= total_size {
            return None;
        }
        let end = end.min(total_size - 1);
        Some((start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_range_valid() {
        assert_eq!(parse_range("bytes=0-499", 1000), Some((0, 499)));
        assert_eq!(parse_range("bytes=500-", 1000), Some((500, 999)));
        assert_eq!(parse_range("bytes=-500", 1000), Some((500, 999)));
    }

    #[test]
    fn test_parse_range_invalid() {
        assert_eq!(parse_range("invalid", 1000), None);
        assert_eq!(parse_range("bytes=1500-", 1000), None);
        assert_eq!(parse_range("bytes=500-400", 1000), None);
    }
}
