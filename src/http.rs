// ==============================================================================
//  Networking & HTTP Request Handling
// ==============================================================================

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use crate::ServerConfig;
use crate::scanner::{self, VideoItem};
use crate::streamer::stream_video;
use crate::thumbnail::serve_or_generate_thumbnail;
use crate::upnp;
use crate::subtitles::{detect_subtitles, srt_to_vtt};

const EMBEDDED_HTML: &str = include_str!("web/index.html");

const PIN_PAGE_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>PLAYBOX // PIN</title>
  <style>
    body {
      background: #000000;
      color: #f0f0f0;
      font-family: 'JetBrains Mono', monospace, monospace;
      display: flex;
      align-items: center;
      justify-content: center;
      height: 100vh;
      margin: 0;
    }
    .box {
      border: 1px solid #222;
      background: #080808;
      padding: 32px;
      border-radius: 4px;
      text-align: center;
      max-width: 320px;
      width: 100%;
    }
    h1 {
      font-size: 14px;
      letter-spacing: 3px;
      margin: 0 0 20px 0;
      color: #888;
    }
    input {
      background: #111;
      color: #fff;
      border: 1px solid #333;
      padding: 10px 14px;
      font-family: inherit;
      font-size: 16px;
      letter-spacing: 4px;
      text-align: center;
      border-radius: 4px;
      outline: none;
      width: calc(100% - 30px);
      margin-bottom: 16px;
    }
    input:focus { border-color: #777; }
    button {
      background: #fff;
      color: #000;
      border: none;
      padding: 10px 20px;
      font-family: inherit;
      font-weight: 700;
      cursor: pointer;
      border-radius: 4px;
      width: 100%;
    }
    button:hover { background: #ccc; }
  </style>
</head>
<body>
  <div class="box">
    <h1>PLAYBOX // AUTH</h1>
    <form method="GET" action="/">
      <input type="password" name="pin" autofocus placeholder="••••" required>
      <button type="submit">UNLOCK</button>
    </form>
  </div>
</body>
</html>"#;

pub fn handle_connection(mut stream: TcpStream, config: ServerConfig) -> io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let mut reader = BufReader::new(stream.try_clone()?);

    loop {
        let mut request_line = String::new();
        match reader.read_line(&mut request_line) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut => return Ok(()),
            Err(e) => return Err(e),
        }

        let parts: Vec<&str> = request_line.split_whitespace().collect();
        if parts.len() < 2 {
            return Ok(());
        }

        let method = parts[0];
        let raw_uri = parts[1];

        let mut range_header = None;
        let mut close_requested = false;
        let mut content_length: usize = 0;
        let mut headers: Vec<(String, String)> = Vec::new();

        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            if line == "\r\n" || line == "\n" || line.is_empty() {
                break;
            }
            if let Some(pos) = line.find(':') {
                let name = line[..pos].trim().to_lowercase();
                let val = line[pos + 1..].trim().to_string();
                if name == "range" {
                    range_header = Some(val.clone());
                } else if name == "connection" && val.to_lowercase().contains("close") {
                    close_requested = true;
                } else if name == "content-length" {
                    if let Ok(n) = val.parse::<usize>() {
                        content_length = n;
                    }
                }
                headers.push((name, val));
            }
        }

        // Read body if present
        let mut body = String::new();
        if content_length > 0 {
            let mut buf = vec![0u8; content_length];
            let _ = reader.read_exact(&mut buf);
            body = String::from_utf8_lossy(&buf).to_string();
        }

        if method != "GET" && method != "HEAD" && method != "POST" {
            send_response(&mut stream, 405, "Method Not Allowed", "text/plain", b"Method Not Allowed")?;
            if close_requested { break; }
            continue;
        }

        let (path_part, query_part) = match raw_uri.find('?') {
            Some(pos) => (&raw_uri[..pos], &raw_uri[pos + 1..]),
            None => (raw_uri, ""),
        };

        let decoded_path = url_decode(path_part);
        let query_map = parse_query(query_part);

        // Check PIN authorization
        let is_authed = verify_pin(&headers, &query_map, config.pin.as_deref());

        let res = match decoded_path.as_str() {
            // UPnP / DLNA Discovery endpoints (do not require web PIN)
            "/description.xml" => {
                let desc = upnp::generate_device_description(&config.local_ip, config.port);
                send_response(&mut stream, 200, "OK", "text/xml; charset=utf-8", desc.as_bytes())
            }
            "/ContentDirectory/Control" | "/upnp/control/ContentDirectory" => {
                let videos = config.videos.lock().unwrap().clone();
                let resp = upnp::generate_browse_response(&videos, &config.local_ip, config.port);
                send_response(&mut stream, 200, "OK", "text/xml; charset=utf-8", resp.as_bytes())
            }
            "/ContentDirectory/SCPD" => {
                let scpd = upnp::generate_scpd_content_directory();
                send_response(&mut stream, 200, "OK", "text/xml; charset=utf-8", scpd.as_bytes())
            }
            "/ConnectionManager/SCPD" => {
                let scpd = upnp::generate_scpd_connection_manager();
                send_response(&mut stream, 200, "OK", "text/xml; charset=utf-8", scpd.as_bytes())
            }
            "/ConnectionManager/Control" => {
                let resp = upnp::generate_protocol_info_response();
                send_response(&mut stream, 200, "OK", "text/xml; charset=utf-8", resp.as_bytes())
            }

            // Authentication verification endpoint
            "/api/auth" | "/api/pin" => {
                let submitted_pin = query_map.get("pin").cloned().or_else(|| {
                    if !body.is_empty() {
                        let post_map = parse_query(&body);
                        post_map.get("pin").cloned()
                    } else {
                        None
                    }
                });

                match config.pin.as_deref() {
                    None => {
                        send_response(&mut stream, 200, "OK", "application/json", b"{\"status\":\"ok\",\"authenticated\":true}")
                    }
                    Some(expected) => {
                        if submitted_pin.as_deref() == Some(expected) || is_authed {
                            let cookie = format!("pin={}; Path=/; SameSite=Lax", expected);
                            send_response_with_headers(
                                &mut stream,
                                200,
                                "OK",
                                "application/json",
                                b"{\"status\":\"ok\",\"authenticated\":true}",
                                &[("Set-Cookie", &cookie)],
                            )
                        } else {
                            send_response(&mut stream, 401, "Unauthorized", "application/json", b"{\"error\":\"Invalid PIN\"}")
                        }
                    }
                }
            }

            // Web Frontend
            "/" | "/index.html" => {
                if config.pin.is_some() && !is_authed {
                    send_response_with_cache(&mut stream, 200, "OK", "text/html; charset=utf-8", PIN_PAGE_HTML.as_bytes(), Some("no-cache"))
                } else if let Some(pin) = config.pin.as_ref() {
                    if query_map.get("pin").map(|s| s.as_str()) == Some(pin.as_str()) {
                        let cookie = format!("pin={}; Path=/; SameSite=Lax", pin);
                        send_response_with_headers(
                            &mut stream,
                            200,
                            "OK",
                            "text/html; charset=utf-8",
                            EMBEDDED_HTML.as_bytes(),
                            &[("Set-Cookie", &cookie), ("Cache-Control", "no-cache")],
                        )
                    } else {
                        send_response_with_cache(&mut stream, 200, "OK", "text/html; charset=utf-8", EMBEDDED_HTML.as_bytes(), Some("no-cache"))
                    }
                } else {
                    send_response_with_cache(&mut stream, 200, "OK", "text/html; charset=utf-8", EMBEDDED_HTML.as_bytes(), Some("no-cache"))
                }
            }

            // Video listing API
            "/api/videos" => {
                if !is_authed {
                    send_response(&mut stream, 401, "Unauthorized", "application/json", b"{\"error\":\"Unauthorized\"}")
                } else {
                    let videos = config.videos.lock().unwrap().clone();
                    let json = serialize_video_list_json(&videos);
                    send_response(&mut stream, 200, "OK", "application/json", json.as_bytes())
                }
            }

            // Rescan filesystem API
            "/api/rescan" => {
                if !is_authed {
                    send_response(&mut stream, 401, "Unauthorized", "application/json", b"{\"error\":\"Unauthorized\"}")
                } else {
                    let videos = scanner::scan_videos(&config.root_dir);
                    {
                        let mut cache = config.videos.lock().unwrap();
                        *cache = videos.clone();
                    }
                    let json = serialize_video_list_json(&videos);
                    send_response(&mut stream, 200, "OK", "application/json", json.as_bytes())
                }
            }

            // Video stream API
            "/api/stream" => {
                if !is_authed {
                    send_response(&mut stream, 401, "Unauthorized", "text/plain", b"Unauthorized")
                } else if let Some(rel_path) = query_map.get("path") {
                    stream_video(&mut stream, &config, rel_path, range_header.as_deref())
                } else {
                    send_response(&mut stream, 400, "Bad Request", "text/plain", b"Missing 'path' parameter")
                }
            }

            // Thumbnail API
            "/api/thumb" => {
                if !is_authed {
                    send_response(&mut stream, 401, "Unauthorized", "text/plain", b"Unauthorized")
                } else if let Some(rel_path) = query_map.get("path") {
                    serve_or_generate_thumbnail(&mut stream, &config, rel_path)
                } else {
                    send_response(&mut stream, 400, "Bad Request", "text/plain", b"Missing 'path' parameter")
                }
            }

            // Subtitle API (retrieve matching subtitles or on-the-fly convert SRT to WebVTT)
            "/api/subtitles" | "/api/subtitle" | "/api/vtt" => {
                if !is_authed {
                    send_response(&mut stream, 401, "Unauthorized", "application/json", b"{\"error\":\"Unauthorized\"}")
                } else if let Some(rel_path) = query_map.get("path") {
                    if let Some(full_path) = scanner::resolve_safe_path(&config.root_dir, rel_path) {
                        let ext = full_path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
                        if ext == "srt" {
                            match fs::read_to_string(&full_path) {
                                Ok(content) => {
                                    let vtt = srt_to_vtt(&content);
                                    send_response(&mut stream, 200, "OK", "text/vtt; charset=utf-8", vtt.as_bytes())
                                }
                                Err(_) => send_response(&mut stream, 500, "Internal Server Error", "text/plain", b"Failed to read subtitle file"),
                            }
                        } else if ext == "vtt" {
                            match fs::read(&full_path) {
                                Ok(bytes) => send_response(&mut stream, 200, "OK", "text/vtt; charset=utf-8", &bytes),
                                Err(_) => send_response(&mut stream, 500, "Internal Server Error", "text/plain", b"Failed to read subtitle file"),
                            }
                        } else {
                            // Video file: return list of matching subtitles
                            let subs = detect_subtitles(&full_path, &config.root_dir);
                            let mut s = String::from("[\n");
                            for (i, sub) in subs.iter().enumerate() {
                                s.push_str(&format!(
                                    "  {{\"name\": \"{}\", \"rel_path\": \"{}\", \"format\": \"{}\"}}",
                                    escape_json(&sub.name),
                                    escape_json(&sub.rel_path),
                                    escape_json(&sub.format)
                                ));
                                if i + 1 < subs.len() { s.push(','); }
                                s.push('\n');
                            }
                            s.push(']');
                            send_response(&mut stream, 200, "OK", "application/json", s.as_bytes())
                        }
                    } else {
                        send_response(&mut stream, 404, "Not Found", "text/plain", b"Subtitle not found")
                    }
                } else {
                    send_response(&mut stream, 400, "Bad Request", "text/plain", b"Missing 'path' parameter")
                }
            }

            // M3U Playlist API
            "/playlist.m3u" | "/api/playlist" => {
                if !is_authed {
                    send_response(&mut stream, 401, "Unauthorized", "text/plain", b"Unauthorized")
                } else {
                    let videos = config.videos.lock().unwrap().clone();
                    let local_ip = &config.local_ip;
                    let port = config.port;
                    let pin_query = match &config.pin {
                        Some(p) => format!("&pin={}", url_encode(p)),
                        None => String::new(),
                    };
                    let mut body = String::from("#EXTM3U\n");
                    for v in &videos {
                        body.push_str(&format!("#EXTINF:-1,{}\n", v.name));
                        body.push_str(&format!(
                            "http://{}:{}/api/stream?path={}{}\n",
                            local_ip, port, url_encode(&v.rel_path), pin_query
                        ));
                    }
                    send_response(&mut stream, 200, "OK", "audio/x-mpegurl", body.as_bytes())
                }
            }

            _ => {
                send_response(&mut stream, 404, "Not Found", "text/plain", b"404 Not Found")
            }
        };

        if res.is_err() {
            return res;
        }

        if close_requested {
            break;
        }
    }

    Ok(())
}

/// Verifies whether the request is authorized when a PIN is configured.
/// Accepts PIN from:
/// 1. Query parameter `pin`
/// 2. `X-Pin` HTTP header
/// 3. `Cookie` header (`pin=...`)
/// 4. `Authorization` header (`Bearer <PIN>` or `<PIN>`)
pub fn verify_pin(
    headers: &[(String, String)],
    query: &HashMap<String, String>,
    expected_pin: Option<&str>,
) -> bool {
    let pin = match expected_pin {
        Some(p) if !p.is_empty() => p,
        _ => return true, // No PIN configured -> authorized
    };

    // 1. Check query parameter
    if let Some(q_pin) = query.get("pin") {
        if q_pin == pin {
            return true;
        }
    }

    // 2. Check headers
    for (name, val) in headers {
        if name == "x-pin" && val.trim() == pin {
            return true;
        }
        if name == "authorization" {
            let auth = val.trim();
            if auth == pin || auth.strip_prefix("Bearer ").map(|s| s.trim()) == Some(pin) {
                return true;
            }
        }
        if name == "cookie" {
            for cookie in val.split(';') {
                let cookie = cookie.trim();
                if let Some(cookie_val) = cookie.strip_prefix("pin=") {
                    if cookie_val.trim() == pin {
                        return true;
                    }
                }
            }
        }
    }

    false
}

pub fn url_decode(s: &str) -> String {
    let mut res = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                res.push(byte);
                i += 3;
                continue;
            }
        } else if bytes[i] == b'+' {
            res.push(b' ');
            i += 1;
            continue;
        }
        res.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&res).to_string()
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    out
}

pub fn parse_query(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut kv = pair.splitn(2, '=');
        if let Some(key) = kv.next() {
            let val = kv.next().unwrap_or("");
            map.insert(url_decode(key), url_decode(val));
        }
    }
    map
}

pub fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

pub fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

pub fn serialize_video_list_json(videos: &[VideoItem]) -> String {
    let mut s = String::from("[\n");
    for (i, v) in videos.iter().enumerate() {
        s.push_str("  {\n");
        s.push_str(&format!("    \"name\": \"{}\",\n", escape_json(&v.name)));
        s.push_str(&format!("    \"rel_path\": \"{}\",\n", escape_json(&v.rel_path)));
        s.push_str(&format!("    \"size\": {},\n", v.size));
        s.push_str(&format!("    \"size_fmt\": \"{}\",\n", escape_json(&v.size_fmt)));
        s.push_str("    \"subtitles\": [\n");
        for (j, sub) in v.subtitles.iter().enumerate() {
            s.push_str(&format!(
                "      {{\"name\": \"{}\", \"rel_path\": \"{}\", \"format\": \"{}\"}}",
                escape_json(&sub.name),
                escape_json(&sub.rel_path),
                escape_json(&sub.format)
            ));
            if j + 1 < v.subtitles.len() { s.push(','); }
            s.push('\n');
        }
        s.push_str("    ]\n");
        s.push_str("  }");
        if i + 1 < videos.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push(']');
    s
}

pub fn get_mime_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()) {
        Some(ref ext) if ext == "mp4" || ext == "m4v" => "video/mp4",
        Some(ref ext) if ext == "webm" => "video/webm",
        Some(ref ext) if ext == "mkv" => "video/x-matroska",
        Some(ref ext) if ext == "mov" => "video/quicktime",
        Some(ref ext) if ext == "avi" => "video/x-msvideo",
        Some(ref ext) if ext == "flv" => "video/x-flv",
        Some(ref ext) if ext == "ogv" => "video/ogg",
        _ => "application/octet-stream",
    }
}

pub fn send_response(
    stream: &mut TcpStream,
    status_code: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> io::Result<()> {
    let header = format!(
        "HTTP/1.1 {} {}\r\n\
        Content-Type: {}\r\n\
        Content-Length: {}\r\n\
        Access-Control-Allow-Origin: *\r\n\
        Connection: keep-alive\r\n\r\n",
        status_code, reason, content_type, body.len()
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)
}

pub fn send_response_with_cache(
    stream: &mut TcpStream,
    status_code: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    cache_control: Option<&str>,
) -> io::Result<()> {
    let cache_line = match cache_control {
        Some(v) => format!("Cache-Control: {}\r\n", v),
        None => String::new(),
    };
    let header = format!(
        "HTTP/1.1 {} {}\r\n\
        Content-Type: {}\r\n\
        Content-Length: {}\r\n\
        {}Access-Control-Allow-Origin: *\r\n\
        Connection: keep-alive\r\n\r\n",
        status_code, reason, content_type, body.len(), cache_line
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)
}

pub fn send_response_with_headers(
    stream: &mut TcpStream,
    status_code: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> io::Result<()> {
    let mut extras = String::new();
    for (k, v) in extra_headers {
        extras.push_str(&format!("{}: {}\r\n", k, v));
    }
    let header = format!(
        "HTTP/1.1 {} {}\r\n\
        Content-Type: {}\r\n\
        Content-Length: {}\r\n\
        {}Access-Control-Allow-Origin: *\r\n\
        Connection: keep-alive\r\n\r\n",
        status_code, reason, content_type, body.len(), extras
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_decode() {
        assert_eq!(url_decode("hello%20world"), "hello world");
        assert_eq!(url_decode("hello+world"), "hello world");
        assert_eq!(url_decode("foo%2Fbar"), "foo/bar");
    }

    #[test]
    fn test_url_encode() {
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("foo/bar"), "foo/bar");
    }

    #[test]
    fn test_parse_query() {
        let q = parse_query("path=videos%2Fclip.mp4&pin=1234");
        assert_eq!(q.get("path").map(|s| s.as_str()), Some("videos/clip.mp4"));
        assert_eq!(q.get("pin").map(|s| s.as_str()), Some("1234"));
    }

    #[test]
    fn test_verify_pin() {
        let mut query = HashMap::new();
        let headers = vec![("x-pin".to_string(), "9999".to_string())];

        // No PIN required
        assert!(verify_pin(&[], &query, None));

        // Required PIN, no match
        assert!(!verify_pin(&[], &query, Some("1234")));

        // Match via query
        query.insert("pin".to_string(), "1234".to_string());
        assert!(verify_pin(&[], &query, Some("1234")));

        // Match via header
        assert!(verify_pin(&headers, &HashMap::new(), Some("9999")));

        // Match via cookie
        let cookie_headers = vec![("cookie".to_string(), "theme=dark; pin=5678; other=1".to_string())];
        assert!(verify_pin(&cookie_headers, &HashMap::new(), Some("5678")));
    }

    #[test]
    fn test_escape_json() {
        assert_eq!(escape_json("Hello \"World\"\n"), "Hello \\\"World\\\"\\n");
    }

    #[test]
    fn test_escape_xml() {
        assert_eq!(escape_xml("<title>Tom & Jerry</title>"), "&lt;title&gt;Tom &amp; Jerry&lt;/title&gt;");
    }
}
