// ==============================================================================
//  PLAYBOX - Minimalist LAN Video Streaming Server
//  100% Rust Standard Library (std) - Zero External Dependencies
//  Compile: rustc -O main.rs -o playbox
//  Run:     ./playbox [OPTIONAL_DIR] [OPTIONAL_PORT]
// ==============================================================================

use std::collections::HashSet;
use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::time::Duration;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

const DEFAULT_PORT: u16 = 8080;
const CHUNK_SIZE: usize = 256 * 1024; // 256 KB streaming buffer
const THUMBS_DIR_NAME: &str = ".playbox_thumbs";

// Supported video extensions
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "mov", "avi", "m4v", "flv", "ogv"];

#[derive(Clone)]
struct ServerConfig {
    root_dir: PathBuf,
    thumbs_dir: PathBuf,
    port: u16,
    local_ip: String,
    has_ffmpeg: bool,
    generating_thumbs: Arc<Mutex<HashSet<String>>>,
    videos: Arc<Mutex<Vec<VideoItem>>>,
}

#[derive(Clone, Debug)]
struct VideoItem {
    name: String,
    rel_path: String,
    size: u64,
    size_fmt: String,
}

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() > 1 && (args[1] == "-h" || args[1] == "--help") {
        println!("PLAYBOX - Minimalist LAN Video Streaming Server (100% Rust std)");
        println!();
        println!("USAGE:");
        println!("  playbox                   Serve current directory on port 8080");
        println!("  playbox <PORT>            Serve current directory on specified port");
        println!("  playbox <DIR>             Serve specified directory on port 8080");
        println!("  playbox <DIR> <PORT>      Serve specified directory on specified port");
        println!();
        println!("OPTIONS:");
        println!("  -h, --help                Show this help message");
        println!();
        return;
    }
    
    // Parse target directory
    let target_dir = if args.len() > 1 && !args[1].parse::<u16>().is_ok() {
        PathBuf::from(&args[1])
    } else {
        env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    };

    let canonical_root = match fs::canonicalize(&target_dir) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[!] Failed to resolve directory {:?}: {}", target_dir, e);
            std::process::exit(1);
        }
    };


    // Set up thumbnails directory in the target folder
    let thumbs_dir = canonical_root.join(THUMBS_DIR_NAME);
    let _ = fs::create_dir_all(&thumbs_dir);

    // Check if ffmpeg is available
    let has_ffmpeg = Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);

    // Bind listener (with fallback to next open port if default is occupied)
    let (listener, active_port) = {
        let explicit_port = if args.len() > 1 && args[1].parse::<u16>().is_ok() {
            Some(args[1].parse::<u16>().unwrap())
        } else if args.len() > 2 && args[2].parse::<u16>().is_ok() {
            Some(args[2].parse::<u16>().unwrap())
        } else {
            None
        };

        if let Some(p) = explicit_port {
            let addr = format!("0.0.0.0:{}", p);
            match TcpListener::bind(&addr) {
                Ok(l) => (l, p),
                Err(e) => {
                    eprintln!("[!] Could not bind to {}: {}", addr, e);
                    std::process::exit(1);
                }
            }
        } else {
            let mut bound = None;
            for candidate in DEFAULT_PORT..(DEFAULT_PORT + 25) {
                let addr = format!("0.0.0.0:{}", candidate);
                if let Ok(l) = TcpListener::bind(&addr) {
                    bound = Some((l, candidate));
                    break;
                }
            }
            match bound {
                Some(pair) => pair,
                None => {
                    eprintln!("[!] Could not find an open port from {} to {}", DEFAULT_PORT, DEFAULT_PORT + 25);
                    std::process::exit(1);
                }
            }
        }
    };

    let local_ip = get_local_ip().unwrap_or_else(|| "127.0.0.1".to_string());

    let config = ServerConfig {
        root_dir: canonical_root.clone(),
        thumbs_dir: thumbs_dir.clone(),
        port: active_port,
        local_ip: local_ip.clone(),
        has_ffmpeg,
        generating_thumbs: Arc::new(Mutex::new(HashSet::new())),
        videos: Arc::new(Mutex::new(Vec::new())),
    };

    let initial_videos = scan_videos(&canonical_root);
    {
        let mut v = config.videos.lock().unwrap();
        *v = initial_videos.clone();
    }

    // Spawn background thread to pre-generate thumbnails for all scanned videos
    if has_ffmpeg {
        let cfg = config.clone();
        thread::spawn(move || {
            pregenerate_thumbnails(cfg);
        });
    }
    let network_url = format!("http://{}:{}", local_ip, active_port);

    println!();
    println!("  ┌────────────────────────────────────────────────────────┐");
    println!("  │                     PLAYBOX SERVER                     │");
    println!("  │      Minimalist LAN Video Streaming (100% Rust std)    │");
    println!("  └────────────────────────────────────────────────────────┘");
    println!();
    println!("  [●] Local:    http://localhost:{}", active_port);
    println!("  [●] Network:  {}", network_url);
    println!("  [●] Root:     {}", canonical_root.display());
    println!("  [●] Videos:   {} found", initial_videos.len());
    println!("  [●] Thumbs:   {}", if has_ffmpeg { "FFmpeg active (Hardware/Server)" } else { "Client canvas fallback" });
    println!();

    // Firewall hint: check if ufw or firewalld is active
    let fw_hint = detect_firewall_hint(active_port);
    if let Some(hint) = fw_hint {
        println!("  ⚠  FIREWALL DETECTED — to allow LAN access, run:");
        println!("     {}", hint);
        println!();
    }

    // Print QR code for the network URL
    println!("  Scan to connect from your phone:");
    println!();
    print_qr_code(&network_url);
    println!();
    println!("  Press Ctrl+C to terminate the server.");
    println!();

    // Spawn SSDP responder thread for UPnP discovery
    {
        let cfg = config.clone();
        thread::spawn(move || {
            ssdp_responder(cfg);
        });
    }

    // Fixed-size thread pool (16 workers) consuming jobs from an mpsc channel
    let (tx, rx) = mpsc::channel::<(TcpStream, ServerConfig)>();
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..16 {
        let rx = Arc::clone(&rx);
        thread::spawn(move || {
            loop {
                let job = {
                    let rx = rx.lock().unwrap();
                    rx.recv()
                };
                match job {
                    Ok((stream, cfg)) => {
                        let _ = handle_connection(stream, cfg);
                    }
                    Err(_) => break,
                }
            }
        });
    }

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                // Larger socket buffers (best-effort)
                let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
                // set_recv_buffer_size / set_send_buffer_size are not in std stable; attempt via raw fd not available portably.
                let cfg = config.clone();
                if tx.send((stream, cfg)).is_err() {
                    eprintln!("[!] Thread pool channel closed");
                    break;
                }
            }
            Err(e) => eprintln!("[!] Incoming connection error: {}", e),
        }
    }
}

// ==============================================================================
//  Networking & HTTP Request Handling
// ==============================================================================

fn handle_connection(mut stream: TcpStream, config: ServerConfig) -> io::Result<()> {
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
            let lower = line.to_lowercase();
            if lower.starts_with("range:") {
                range_header = Some(line.trim()["range:".len()..].trim().to_string());
            }
            if lower.starts_with("connection:") && lower.contains("close") {
                close_requested = true;
            }
            if lower.starts_with("content-length:") {
                if let Ok(n) = line.trim()["content-length:".len()..].trim().parse::<usize>() {
                    content_length = n;
                }
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

        let res = match decoded_path.as_str() {
            "/" | "/index.html" => {
                send_response_with_cache(&mut stream, 200, "OK", "text/html; charset=utf-8", EMBEDDED_HTML.as_bytes(), Some("no-cache"))
            }
            "/api/videos" => {
                let videos = config.videos.lock().unwrap().clone();
                let json = serialize_video_list_json(&videos);
                send_response(&mut stream, 200, "OK", "application/json", json.as_bytes())
            }
            "/api/rescan" => {
                let videos = scan_videos(&config.root_dir);
                {
                    let mut cache = config.videos.lock().unwrap();
                    *cache = videos.clone();
                }
                let json = serialize_video_list_json(&videos);
                send_response(&mut stream, 200, "OK", "application/json", json.as_bytes())
            }
            "/api/stream" => {
                let query_map = parse_query(query_part);
                if let Some(rel_path) = query_map.get("path") {
                    stream_video(&mut stream, &config, rel_path, range_header.as_deref())
                } else {
                    send_response(&mut stream, 400, "Bad Request", "text/plain", b"Missing 'path' parameter")
                }
            }
            "/api/thumb" => {
                let query_map = parse_query(query_part);
                if let Some(rel_path) = query_map.get("path") {
                    serve_or_generate_thumbnail(&mut stream, &config, rel_path)
                } else {
                    send_response(&mut stream, 400, "Bad Request", "text/plain", b"Missing 'path' parameter")
                }
            }
            "/playlist.m3u" | "/api/playlist" => {
                let videos = config.videos.lock().unwrap().clone();
                let local_ip = get_local_ip().unwrap_or_else(|| "127.0.0.1".to_string());
                let port = config.port;
                let mut body = String::from("#EXTM3U\n");
                for v in &videos {
                    body.push_str(&format!("#EXTINF:-1,{}\n", v.name));
                    body.push_str(&format!("http://{}:{}/api/stream?path={}\n", local_ip, port, url_encode(&v.rel_path)));
                }
                send_response(&mut stream, 200, "OK", "audio/x-mpegurl", body.as_bytes())
            }
            "/description.xml" => {
                let desc = format!(r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaServer:1</deviceType>
    <friendlyName>Playbox</friendlyName>
    <manufacturer>Playbox</manufacturer>
    <modelName>Playbox</modelName>
    <UDN>uuid:playbox-{}-{}</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId>
        <SCPDURL>/ContentDirectory/SCPD</SCPDURL>
        <controlURL>/ContentDirectory/Control</controlURL>
        <eventSubURL>/ContentDirectory/Event</eventSubURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId>
        <SCPDURL>/ConnectionManager/SCPD</SCPDURL>
        <controlURL>/ConnectionManager/Control</controlURL>
        <eventSubURL>/ConnectionManager/Event</eventSubURL>
      </service>
    </serviceList>
  </device>
</root>"#, config.local_ip, config.port);
                send_response(&mut stream, 200, "OK", "text/xml; charset=utf-8", desc.as_bytes())
            }
            "/ContentDirectory/Control" | "/upnp/control/ContentDirectory" => {
                // SOAP Browse
                let videos = config.videos.lock().unwrap().clone();
                let ip = config.local_ip.clone();
                let port = config.port;
                let mut items = String::new();
                for v in &videos {
                    let url = format!("http://{}:{}/api/stream?path={}", ip, port, url_encode(&v.rel_path));
                    let mime = get_mime_type(std::path::Path::new(&v.name));
                    items.push_str(&format!(
                        "<item id=\"{}\" parentID=\"0\" restricted=\"1\"><dc:title xmlns:dc=\"http://purl.org/dc/elements/1.1/\">{}</dc:title><res protocolInfo=\"http-get:*:{}:*\">{}</res></item>",
                        escape_xml(&v.rel_path), escape_xml(&v.name), mime, url
                    ));
                }
                let didl = format!("<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\"><container id=\"0\" parentID=\"-1\" restricted=\"1\"><dc:title>Root</dc:title></container>{}</DIDL-Lite>", items);
                let resp = format!(
                    "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:BrowseResponse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\"><Result>{}</Result><NumberReturned>{}</NumberReturned><TotalMatches>{}</TotalMatches><UpdateID>1</UpdateID></u:BrowseResponse></s:Body></s:Envelope>",
                    escape_xml(&didl), videos.len(), videos.len()
                );
                // send with SOAPAction handling ignored
                send_response(&mut stream, 200, "OK", "text/xml; charset=utf-8", resp.as_bytes())
            }
            "/ContentDirectory/SCPD" => {
                let scpd = r#"<?xml version="1.0"?><scpd xmlns="urn:schemas-upnp-org:service-1-0"><specVersion><major>1</major><minor>0</minor></specVersion><actionList><action><name>Browse</name><argumentList><argument><name>ObjectID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument><argument><name>BrowseFlag</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_BrowseFlag</relatedStateVariable></argument><argument><name>Filter</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Filter</relatedStateVariable></argument><argument><name>StartingIndex</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Index</relatedStateVariable></argument><argument><name>RequestedCount</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument><argument><name>SortCriteria</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SortCriteria</relatedStateVariable></argument><argument><name>Result</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument><argument><name>NumberReturned</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument><argument><name>TotalMatches</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument><argument><name>UpdateID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_UpdateID</relatedStateVariable></argument></argumentList></action></actionList><serviceStateTable><stateVariable sendEvents="no"><name>A_ARG_TYPE_ObjectID</name><dataType>string</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_BrowseFlag</name><dataType>string</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_Filter</name><dataType>string</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_Index</name><dataType>ui4</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_Count</name><dataType>ui4</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_SortCriteria</name><dataType>string</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_Result</name><dataType>string</dataType></stateVariable><stateVariable sendEvents="no"><name>A_ARG_TYPE_UpdateID</name><dataType>ui4</dataType></stateVariable></serviceStateTable></scpd>"#;
                send_response(&mut stream, 200, "OK", "text/xml; charset=utf-8", scpd.as_bytes())
            }
            "/ConnectionManager/SCPD" => {
                let scpd = r#"<?xml version="1.0"?><scpd xmlns="urn:schemas-upnp-org:service-1-0"><specVersion><major>1</major><minor>0</minor></specVersion><actionList><action><name>GetProtocolInfo</name><argumentList><argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument><argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument></argumentList></action></actionList><serviceStateTable><stateVariable sendEvents="no"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable><stateVariable sendEvents="no"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable></serviceStateTable></scpd>"#;
                send_response(&mut stream, 200, "OK", "text/xml; charset=utf-8", scpd.as_bytes())
            }
            "/ConnectionManager/Control" => {
                let resp = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetProtocolInfoResponse xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1"><Source>http-get:*:video/mp4:*,http-get:*:video/*:*</Source><Sink></Sink></u:GetProtocolInfoResponse></s:Body></s:Envelope>"#;
                send_response(&mut stream, 200, "OK", "text/xml; charset=utf-8", resp.as_bytes())
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

// ==============================================================================
//  Video Streaming with HTTP 206 Partial Content (Range Requests)
// ==============================================================================

fn stream_video(
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

fn parse_range(range_str: &str, total_size: u64) -> Option<(u64, u64)> {
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
        let end = total_size - 1;
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

// ==============================================================================
//  Thumbnails (FFmpeg on-demand or fallback)
// ==============================================================================

fn serve_or_generate_thumbnail(
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
            // Already generating, return 204 or wait briefly
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

fn sanitize_filename_hash(rel_path: &str) -> String {
    // Simple fast DJB2 hash converted to hex + cleaned path characters
    let mut hash: u64 = 5381;
    for byte in rel_path.bytes() {
        hash = ((hash << 5).wrapping_add(hash)).wrapping_add(byte as u64);
    }
    format!("{:016x}", hash)
}

// ==============================================================================
//  Filesystem Scanning & Path Security
// ==============================================================================

fn pregenerate_thumbnails(config: ServerConfig) {
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

        if let Some(video_path) = resolve_safe_path(&config.root_dir, &v.rel_path) {
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
        }

        {
            let mut gen = config.generating_thumbs.lock().unwrap();
            gen.remove(&v.rel_path);
        }
    }
}

fn scan_videos(root: &Path) -> Vec<VideoItem> {
    let mut list = Vec::new();
    walk_directory(root, root, &mut list);
    // Sort alphabetically by name
    list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    list
}

fn walk_directory(current: &Path, root: &Path, list: &mut Vec<VideoItem>) {
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
                        list.push(VideoItem {
                            name: file_name.to_string(),
                            rel_path: rel_str,
                            size: meta.len(),
                            size_fmt: format_file_size(meta.len()),
                        });
                    }
                }
            }
        }
    }
}

fn resolve_safe_path(root: &Path, rel_path: &str) -> Option<PathBuf> {
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

// ==============================================================================
//  Helpers & Utilities
// ==============================================================================

fn ssdp_responder(config: ServerConfig) {
    let ip = config.local_ip.clone();
    let port = config.port;
    let location = format!("http://{}:{}/description.xml", ip, port);
    let usn = format!("uuid:playbox-{}::urn:schemas-upnp-org:device:MediaServer:1", ip);
    let server_hdr = "Playbox/1.0 UPnP/1.0";

    // Notifier thread
    let loc2 = location.clone();
    let usn2 = usn.clone();
    thread::spawn(move || {
        loop {
            // send NOTIFY alive
            if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
                let _ = sock.set_multicast_ttl_v4(4);
                let msg = format!(
                    "NOTIFY * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nCACHE-CONTROL: max-age=1800\r\nLOCATION: {}\r\nNT: urn:schemas-upnp-org:device:MediaServer:1\r\nNTS: ssdp:alive\r\nSERVER: {}\r\nUSN: {}\r\n\r\n",
                    loc2, server_hdr, usn2
                );
                let _ = sock.send_to(msg.as_bytes(), "239.255.255.250:1900");
            }
            thread::sleep(Duration::from_secs(30));
        }
    });

    // Responder socket
    let sock = match UdpSocket::bind("0.0.0.0:1900") {
        Ok(s) => s,
        Err(e) => { eprintln!("[!] SSDP bind 1900 failed: {}", e); return; }
    };
    let _ = sock.set_multicast_loop_v4(true);
    let multi: std::net::Ipv4Addr = "239.255.255.250".parse().unwrap();
    let iface: std::net::Ipv4Addr = config.local_ip.parse().unwrap_or("0.0.0.0".parse().unwrap());
    let _ = sock.join_multicast_v4(&multi, &iface);
    let _ = sock.set_read_timeout(Some(Duration::from_secs(1)));
    let mut buf = [0u8; 2048];
    loop {
        match sock.recv_from(&mut buf) {
            Ok((n, src)) => {
                let req = String::from_utf8_lossy(&buf[..n]);
                let lower = req.to_lowercase();
                if lower.contains("m-search") {
                    let st_req = if lower.contains("ssdp:all") {
                        "ssdp:all"
                    } else if lower.contains("mediaserver:1") {
                        "urn:schemas-upnp-org:device:MediaServer:1"
                    } else {
                        continue;
                    };
                    // determine ST to respond
                    let st = if st_req == "ssdp:all" {
                        "upnp:rootdevice"
                    } else {
                        "urn:schemas-upnp-org:device:MediaServer:1"
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=1800\r\nDATE: \r\nEXT: \r\nLOCATION: {}\r\nSERVER: {}\r\nST: {}\r\nUSN: {}\r\nContent-Length: 0\r\n\r\n",
                        location, server_hdr, st, usn
                    );
                    let _ = sock.send_to(resp.as_bytes(), src);
                    // also respond rootdevice and mediaserver? minimal enough
                    let _ = st_req;
                }
            }
            Err(_) => {}
        }
    }
}

fn get_local_ip() -> Option<String> {
    // UDP socket route inquiry - queries OS routing table without sending packets
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

fn detect_firewall_hint(port: u16) -> Option<String> {
    // Check if ufw is active
    if let Ok(output) = Command::new("systemctl")
        .args(&["is-active", "ufw"])
        .output()
    {
        let status = String::from_utf8_lossy(&output.stdout);
        if status.trim() == "active" {
            return Some(format!("sudo ufw allow {}/tcp comment \"Playbox\"", port));
        }
    }
    // Check if firewalld is active
    if let Ok(output) = Command::new("systemctl")
        .args(&["is-active", "firewalld"])
        .output()
    {
        let status = String::from_utf8_lossy(&output.stdout);
        if status.trim() == "active" {
            return Some(format!(
                "sudo firewall-cmd --add-port={}/tcp --permanent && sudo firewall-cmd --reload",
                port
            ));
        }
    }
    None
}

// ==============================================================================
//  QR Code Generator (100% std — no external crates)
//  Supports Version 1-6, Byte mode, ECC Level L
// ==============================================================================

fn print_qr_code(text: &str) {
    if let Some(modules) = qr_encode(text) {
        let size = modules.len();
        // Use Unicode half-block characters: upper half = top row, lower half = bottom row
        // ██ = both black, ▀▀ = top black / bottom white, ▄▄ = top white / bottom black, "  " = both white
        // QR spec: dark module = true. Terminal: we want dark=black on white bg or inverted.
        // Use inverted: dark module = █ (dark on terminal)
        let quiet = 2; // quiet zone modules
        let total = size + quiet * 2;

        // Process two rows at a time using half-block characters
        let get = |r: usize, c: usize| -> bool {
            if r < quiet || r >= size + quiet || c < quiet || c >= size + quiet {
                false // quiet zone is white (light)
            } else {
                modules[r - quiet][c - quiet]
            }
        };

        let mut row = 0;
        while row < total {
            print!("  "); // left indent
            for col in 0..total {
                let top = get(row, col);
                let bot = if row + 1 < total {
                    get(row + 1, col)
                } else {
                    false
                };
                // We want dark modules to be dark blocks on terminal
                // Using inverted rendering (dark = █):
                match (top, bot) {
                    (true, true) => print!("█"),
                    (true, false) => print!("▀"),
                    (false, true) => print!("▄"),
                    (false, false) => print!(" "),
                }
            }
            println!();
            row += 2;
        }
    } else {
        println!("  [QR generation failed — URL may be too long]");
    }
}

// QR Version capacities for Byte mode, ECC Level L (data codewords)
const QR_DATA_CAPACITY: &[(usize, usize)] = &[
    // (version, data_codewords)
    (1, 17),
    (2, 32),
    (3, 53),
    (4, 78),
    (5, 106),
    (6, 134),
];

// Total codewords per version
const QR_TOTAL_CODEWORDS: &[usize] = &[26, 44, 70, 100, 134, 172];

// ECC codewords per block for Level L
const QR_ECC_PER_BLOCK: &[usize] = &[7, 10, 15, 20, 26, 36];

// Number of ECC blocks for Level L
const QR_NUM_BLOCKS: &[usize] = &[1, 1, 1, 1, 1, 2];

// Alignment pattern center coords per version (version 1 has none)
const QR_ALIGN_CENTERS: &[&[usize]] = &[
    &[],           // v1
    &[6, 18],      // v2
    &[6, 22],      // v3
    &[6, 26],      // v4
    &[6, 30],      // v5
    &[6, 34],      // v6
];

fn qr_encode(text: &str) -> Option<Vec<Vec<bool>>> {
    let data_bytes = text.as_bytes();
    let data_len = data_bytes.len();

    // Find smallest version that fits
    let mut version = 0usize;
    let mut data_cap = 0usize;
    for &(v, cap) in QR_DATA_CAPACITY {
        // In byte mode: 4 bits mode + 8/16 bits count + data*8 bits + 4 bits terminator
        // For version 1-9: count indicator is 8 bits
        let overhead = 4 + 8 + 4; // mode(4) + count(8) + terminator(4) = 16 bits = 2 bytes
        let usable = if cap * 8 >= overhead {
            (cap * 8 - overhead) / 8
        } else {
            0
        };
        if data_len <= usable {
            version = v;
            data_cap = cap;
            break;
        }
    }
    if version == 0 {
        return None; // Too long
    }

    let vi = version - 1; // version index
    let size = 17 + version * 4; // module size
    let total_codewords = QR_TOTAL_CODEWORDS[vi];
    let ecc_per_block = QR_ECC_PER_BLOCK[vi];
    let num_blocks = QR_NUM_BLOCKS[vi];

    // === Step 1: Encode data into bit stream ===
    let mut bits: Vec<bool> = Vec::new();

    // Mode indicator: Byte mode = 0100
    push_bits(&mut bits, 0b0100, 4);

    // Character count indicator (8 bits for version 1-9)
    push_bits(&mut bits, data_len as u32, 8);

    // Data
    for &b in data_bytes {
        push_bits(&mut bits, b as u32, 8);
    }

    // Terminator (up to 4 zero bits, but don't exceed capacity)
    let cap_bits = data_cap * 8;
    let term_len = 4.min(cap_bits.saturating_sub(bits.len()));
    for _ in 0..term_len {
        bits.push(false);
    }

    // Pad to byte boundary
    while bits.len() % 8 != 0 && bits.len() < cap_bits {
        bits.push(false);
    }

    // Pad with alternating 0xEC, 0x11
    let pad_bytes = [0xECu8, 0x11u8];
    let mut pi = 0;
    while bits.len() < cap_bits {
        push_bits(&mut bits, pad_bytes[pi % 2] as u32, 8);
        pi += 1;
    }

    // Convert to bytes
    let mut data_codewords: Vec<u8> = Vec::new();
    for chunk in bits.chunks(8) {
        let mut byte = 0u8;
        for (i, &b) in chunk.iter().enumerate() {
            if b {
                byte |= 1 << (7 - i);
            }
        }
        data_codewords.push(byte);
    }
    data_codewords.truncate(data_cap);

    // === Step 2: Error correction ===
    let _ecc_total = ecc_per_block * num_blocks;
    let data_per_block_base = data_cap / num_blocks;
    let blocks_with_extra = data_cap % num_blocks;

    let mut data_blocks: Vec<Vec<u8>> = Vec::new();
    let mut ecc_blocks: Vec<Vec<u8>> = Vec::new();
    let mut offset = 0;

    for i in 0..num_blocks {
        let block_data_len = data_per_block_base + if i >= num_blocks - blocks_with_extra { 1 } else { 0 };
        let block_data = data_codewords[offset..offset + block_data_len.min(data_codewords.len() - offset)].to_vec();
        offset += block_data.len();
        let ecc = rs_encode(&block_data, ecc_per_block);
        data_blocks.push(block_data);
        ecc_blocks.push(ecc);
    }

    // Interleave data blocks
    let max_data_len = data_blocks.iter().map(|b| b.len()).max().unwrap_or(0);
    let mut interleaved: Vec<u8> = Vec::with_capacity(total_codewords);
    for i in 0..max_data_len {
        for block in &data_blocks {
            if i < block.len() {
                interleaved.push(block[i]);
            }
        }
    }
    // Interleave ECC blocks
    for i in 0..ecc_per_block {
        for block in &ecc_blocks {
            if i < block.len() {
                interleaved.push(block[i]);
            }
        }
    }

    // === Step 3: Build matrix ===
    let mut modules = vec![vec![false; size]; size];
    let mut is_function = vec![vec![false; size]; size]; // marks function pattern areas

    // Place finder patterns
    place_finder_pattern(&mut modules, &mut is_function, 0, 0, size);
    place_finder_pattern(&mut modules, &mut is_function, 0, size - 7, size);
    place_finder_pattern(&mut modules, &mut is_function, size - 7, 0, size);

    // Place alignment patterns (version >= 2)
    let centers = QR_ALIGN_CENTERS[vi];
    if centers.len() >= 2 {
        for &r in centers {
            for &c in centers {
                // Skip if it overlaps with finder patterns
                if is_function[r][c] {
                    continue;
                }
                place_alignment_pattern(&mut modules, &mut is_function, r, c, size);
            }
        }
    }

    // Timing patterns
    for i in 8..size - 8 {
        let on = i % 2 == 0;
        if !is_function[6][i] {
            modules[6][i] = on;
            is_function[6][i] = true;
        }
        if !is_function[i][6] {
            modules[i][6] = on;
            is_function[i][6] = true;
        }
    }

    // Dark module
    modules[size - 8][8] = true;
    is_function[size - 8][8] = true;

    // Reserve format info areas
    for i in 0..9 {
        if i < size {
            is_function[8][i] = true;
            is_function[i][8] = true;
        }
    }
    for i in 0..8 {
        is_function[8][size - 1 - i] = true;
        is_function[size - 1 - i][8] = true;
    }

    // === Step 4: Place data bits ===
    let mut data_bits: Vec<bool> = Vec::new();
    for &byte in &interleaved {
        for j in (0..8).rev() {
            data_bits.push((byte >> j) & 1 == 1);
        }
    }
    // Pad remainder bits
    let remainder_bits = match version {
        2 | 3 | 4 | 5 | 6 => 7,
        _ => 0,
    };
    for _ in 0..remainder_bits {
        data_bits.push(false);
    }

    let mut bit_idx = 0;
    let mut right = size as isize - 1;
    while right >= 1 {
        if right == 6 {
            right -= 1; // skip timing column
        }
        // Traverse upward then downward alternately
        let going_up = ((size as isize - 1 - right) / 2) % 2 == 0;
        let rows: Vec<usize> = if going_up {
            (0..size).rev().collect()
        } else {
            (0..size).collect()
        };
        for row in rows {
            for &col_offset in &[0isize, -1isize] {
                let col = (right + col_offset) as usize;
                if col >= size {
                    continue;
                }
                if is_function[row][col] {
                    continue;
                }
                if bit_idx < data_bits.len() {
                    modules[row][col] = data_bits[bit_idx];
                    bit_idx += 1;
                }
            }
        }
        right -= 2;
    }

    // === Step 5: Apply masking (try all 8 masks, pick best) ===
    let mut _best_mask = 0u8;
    let mut best_penalty = u32::MAX;
    let mut best_matrix = modules.clone();

    for mask in 0..8u8 {
        let mut candidate = modules.clone();
        apply_mask(&mut candidate, &is_function, mask, size);
        write_format_info(&mut candidate, 0b01, mask, size); // ECC L = 01
        let penalty = evaluate_penalty(&candidate, size);
        if penalty < best_penalty {
            best_penalty = penalty;
            _best_mask = mask;
            best_matrix = candidate;
        }
    }

    Some(best_matrix)
}

fn push_bits(bits: &mut Vec<bool>, value: u32, count: usize) {
    for i in (0..count).rev() {
        bits.push((value >> i) & 1 == 1);
    }
}

// Reed-Solomon encoding over GF(256) with primitive polynomial 0x11D
fn rs_encode(data: &[u8], ecc_len: usize) -> Vec<u8> {
    let gen = rs_generator_poly(ecc_len);
    let mut msg = data.to_vec();
    msg.resize(data.len() + ecc_len, 0);

    for i in 0..data.len() {
        let coef = msg[i];
        if coef != 0 {
            for j in 0..gen.len() {
                msg[i + j] ^= gf_mul(gen[j], coef);
            }
        }
    }

    msg[data.len()..].to_vec()
}

fn rs_generator_poly(degree: usize) -> Vec<u8> {
    let mut gen = vec![1u8];
    for i in 0..degree {
        let mut new_gen = vec![0u8; gen.len() + 1];
        let factor = gf_exp(i as u8);
        for j in 0..gen.len() {
            new_gen[j] ^= gen[j];
            new_gen[j + 1] ^= gf_mul(gen[j], factor);
        }
        gen = new_gen;
    }
    gen
}

// GF(256) lookup tables
fn gf_exp(a: u8) -> u8 {
    GF_EXP_TABLE[a as usize]
}

#[allow(dead_code)]
fn gf_log(a: u8) -> u8 {
    GF_LOG_TABLE[a as usize]
}

fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        let log_a = GF_LOG_TABLE[a as usize] as usize;
        let log_b = GF_LOG_TABLE[b as usize] as usize;
        GF_EXP_TABLE[(log_a + log_b) % 255]
    }
}

// Precomputed GF(256) tables (primitive poly 0x11D = x^8 + x^4 + x^3 + x^2 + 1)
const GF_EXP_TABLE: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut val = 1u16;
    let mut i = 0;
    while i < 256 {
        table[i] = val as u8;
        val <<= 1;
        if val >= 256 {
            val ^= 0x11D;
        }
        i += 1;
    }
    table
};

const GF_LOG_TABLE: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 255 {
        table[GF_EXP_TABLE[i] as usize] = i as u8;
        i += 1;
    }
    table
};

fn place_finder_pattern(
    modules: &mut Vec<Vec<bool>>,
    is_function: &mut Vec<Vec<bool>>,
    row: usize,
    col: usize,
    size: usize,
) {
    // 7x7 finder + 1 cell separator (white border)
    for dr in 0..9isize {
        for dc in 0..9isize {
            let r = row as isize + dr - 1;
            let c = col as isize + dc - 1;
            if r < 0 || r >= size as isize || c < 0 || c >= size as isize {
                continue;
            }
            let ru = r as usize;
            let cu = c as usize;
            // The actual 7x7 pattern
            let _in_outer = dr >= 0 && dr <= 8 && dc >= 0 && dc <= 8;
            let in_finder = dr >= 1 && dr <= 7 && dc >= 1 && dc <= 7;
            let on_border = dr == 1 || dr == 7 || dc == 1 || dc == 7;
            let in_inner = dr >= 3 && dr <= 5 && dc >= 3 && dc <= 5;

            is_function[ru][cu] = true;
            if in_finder && (on_border || in_inner) {
                modules[ru][cu] = true;
            } else {
                modules[ru][cu] = false;
            }
        }
    }
}

fn place_alignment_pattern(
    modules: &mut Vec<Vec<bool>>,
    is_function: &mut Vec<Vec<bool>>,
    center_row: usize,
    center_col: usize,
    size: usize,
) {
    for dr in -2i32..=2 {
        for dc in -2i32..=2 {
            let r = center_row as i32 + dr;
            let c = center_col as i32 + dc;
            if r < 0 || r >= size as i32 || c < 0 || c >= size as i32 {
                continue;
            }
            let ru = r as usize;
            let cu = c as usize;
            is_function[ru][cu] = true;
            modules[ru][cu] = dr.abs() == 2 || dc.abs() == 2 || (dr == 0 && dc == 0);
        }
    }
}

fn apply_mask(modules: &mut Vec<Vec<bool>>, is_function: &[Vec<bool>], mask: u8, size: usize) {
    for r in 0..size {
        for c in 0..size {
            if is_function[r][c] {
                continue;
            }
            let invert = match mask {
                0 => (r + c) % 2 == 0,
                1 => r % 2 == 0,
                2 => c % 3 == 0,
                3 => (r + c) % 3 == 0,
                4 => (r / 2 + c / 3) % 2 == 0,
                5 => (r * c) % 2 + (r * c) % 3 == 0,
                6 => ((r * c) % 2 + (r * c) % 3) % 2 == 0,
                7 => ((r + c) % 2 + (r * c) % 3) % 2 == 0,
                _ => false,
            };
            if invert {
                modules[r][c] = !modules[r][c];
            }
        }
    }
}

fn write_format_info(modules: &mut Vec<Vec<bool>>, ecc_level: u8, mask: u8, size: usize) {
    let data = ((ecc_level as u16) << 3) | (mask as u16);
    let mut rem = data << 10;
    // Generator polynomial for format info: x^10 + x^8 + x^5 + x^4 + x^2 + x + 1 = 0x537
    let gen = 0x537u16;
    for i in (0..=4).rev() {
        if rem & (1 << (i + 10)) != 0 {
            rem ^= gen << i;
        }
    }
    let format_bits = ((data << 10) | rem) ^ 0x5412; // XOR with mask pattern

    // Place format bits around finder patterns
    // Horizontal: left side of top-left finder, then right side
    let format_positions_h: [(usize, usize); 15] = [
        (8, 0), (8, 1), (8, 2), (8, 3), (8, 4), (8, 5), (8, 7), (8, 8),
        (8, size - 8), (8, size - 7), (8, size - 6), (8, size - 5),
        (8, size - 4), (8, size - 3), (8, size - 2),
    ];
    let format_positions_v: [(usize, usize); 15] = [
        (0, 8), (1, 8), (2, 8), (3, 8), (4, 8), (5, 8), (7, 8), (8, 8),
        (size - 7, 8), (size - 6, 8), (size - 5, 8), (size - 4, 8),
        (size - 3, 8), (size - 2, 8), (size - 1, 8),
    ];
    for i in 0..15 {
        let bit = (format_bits >> (14 - i)) & 1 == 1;
        let (r, c) = format_positions_h[i];
        modules[r][c] = bit;
        let (r2, c2) = format_positions_v[i];
        modules[r2][c2] = bit;
    }
}

fn evaluate_penalty(modules: &[Vec<bool>], size: usize) -> u32 {
    let mut penalty = 0u32;

    // Rule 1: runs of same color (5+ in a row/col)
    for r in 0..size {
        let mut run = 1u32;
        for c in 1..size {
            if modules[r][c] == modules[r][c - 1] {
                run += 1;
            } else {
                if run >= 5 {
                    penalty += run - 2;
                }
                run = 1;
            }
        }
        if run >= 5 {
            penalty += run - 2;
        }
    }
    for c in 0..size {
        let mut run = 1u32;
        for r in 1..size {
            if modules[r][c] == modules[r - 1][c] {
                run += 1;
            } else {
                if run >= 5 {
                    penalty += run - 2;
                }
                run = 1;
            }
        }
        if run >= 5 {
            penalty += run - 2;
        }
    }

    // Rule 2: 2x2 blocks of same color
    for r in 0..size - 1 {
        for c in 0..size - 1 {
            let color = modules[r][c];
            if color == modules[r][c + 1]
                && color == modules[r + 1][c]
                && color == modules[r + 1][c + 1]
            {
                penalty += 3;
            }
        }
    }

    // Rule 3: finder-like patterns (1011101 with 4 white)
    let pattern_a: [bool; 11] = [true, false, true, true, true, false, true, false, false, false, false];
    let pattern_b: [bool; 11] = [false, false, false, false, true, false, true, true, true, false, true];
    for r in 0..size {
        for c in 0..size.saturating_sub(10) {
            let mut match_a = true;
            let mut match_b = true;
            for i in 0..11 {
                if modules[r][c + i] != pattern_a[i] {
                    match_a = false;
                }
                if modules[r][c + i] != pattern_b[i] {
                    match_b = false;
                }
            }
            if match_a || match_b {
                penalty += 40;
            }
        }
    }
    for c in 0..size {
        for r in 0..size.saturating_sub(10) {
            let mut match_a = true;
            let mut match_b = true;
            for i in 0..11 {
                if modules[r + i][c] != pattern_a[i] {
                    match_a = false;
                }
                if modules[r + i][c] != pattern_b[i] {
                    match_b = false;
                }
            }
            if match_a || match_b {
                penalty += 40;
            }
        }
    }

    // Rule 4: proportion of dark modules
    let total = (size * size) as u32;
    let dark: u32 = modules.iter().flatten().filter(|&&m| m).count() as u32;
    let pct = (dark * 100) / total;
    let prev5 = (pct / 5) * 5;
    let next5 = prev5 + 5;
    let dev_prev = if prev5 >= 50 { prev5 - 50 } else { 50 - prev5 };
    let dev_next = if next5 >= 50 { next5 - 50 } else { 50 - next5 };
    penalty += (dev_prev.min(dev_next) / 5) * 10;

    penalty
}

fn get_mime_type(path: &Path) -> &'static str {
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

fn format_file_size(bytes: u64) -> String {
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

fn url_decode(s: &str) -> String {
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

fn url_encode(s: &str) -> String {
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

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
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

fn escape_xml(s: &str) -> String {
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

fn escape_json(s: &str) -> String {
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

fn serialize_video_list_json(videos: &[VideoItem]) -> String {
    let mut s = String::from("[\n");
    for (i, v) in videos.iter().enumerate() {
        s.push_str("  {\n");
        s.push_str(&format!("    \"name\": \"{}\",\n", escape_json(&v.name)));
        s.push_str(&format!("    \"rel_path\": \"{}\",\n", escape_json(&v.rel_path)));
        s.push_str(&format!("    \"size\": {},\n", v.size));
        s.push_str(&format!("    \"size_fmt\": \"{}\"\n", escape_json(&v.size_fmt)));
        s.push_str("  }");
        if i + 1 < videos.len() {
            s.push(',');
        }
        s.push('\n');
    }
    s.push(']');
    s
}

fn send_response(
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

fn send_response_with_cache(
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

// ==============================================================================
//  Embedded Minimalist Frontend (Black & White + JetBrains Mono)
// ==============================================================================

const EMBEDDED_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>PLAYBOX // STREAM</title>
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
  <link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:ital,wght@0,300;0,400;0,500;0,700;1,400&display=swap" rel="stylesheet">
  <style>
    :root {
      --bg: #000000;
      --card-bg: #080808;
      --card-hover: #111111;
      --border: #222222;
      --border-bright: #444444;
      --text: #f0f0f0;
      --text-dim: #777777;
      --accent: #ffffff;
    }

    * {
      box-sizing: border-box;
      margin: 0;
      padding: 0;
    }

    body {
      background: var(--bg);
      color: var(--text);
      font-family: 'JetBrains Mono', monospace;
      font-size: 13px;
      line-height: 1.5;
      min-height: 100vh;
      display: flex;
      flex-direction: column;
      overflow-x: hidden;
      letter-spacing: -0.02em;
    }

    /* HEADER */
    header {
      border-bottom: 1px solid var(--border);
      padding: 18px 24px;
      display: flex;
      flex-wrap: wrap;
      align-items: center;
      justify-content: space-between;
      gap: 16px;
      background: var(--bg);
      position: sticky;
      top: 0;
      z-index: 50;
    }

    .brand {
      display: flex;
      align-items: center;
      gap: 12px;
    }

    .brand-mark {
      width: 10px;
      height: 10px;
      background: var(--accent);
      display: inline-block;
      animation: blink 2s infinite;
    }

    @keyframes blink {
      0%, 100% { opacity: 1; }
      50% { opacity: 0.2; }
    }

    .brand-title {
      font-weight: 700;
      font-size: 14px;
      letter-spacing: 0.08em;
      text-transform: uppercase;
      color: var(--text);
    }

    .meta-counts {
      color: var(--text-dim);
      font-size: 11px;
    }

    .controls {
      display: flex;
      align-items: center;
      gap: 12px;
      flex-grow: 1;
      max-width: 420px;
    }

    .search-input {
      width: 100%;
      background: #0d0d0d;
      border: 1px solid var(--border);
      color: var(--text);
      font-family: inherit;
      font-size: 12px;
      padding: 7px 12px;
      outline: none;
      transition: border-color 0.2s;
    }

    .search-input:focus {
      border-color: var(--accent);
    }

    .search-input::placeholder {
      color: #444;
    }

    /* MAIN GALLERY */
    main {
      flex: 1;
      padding: 24px;
      max-width: 1800px;
      margin: 0 auto;
      width: 100%;
    }

    .video-grid {
      display: grid;
      grid-template-columns: repeat(auto-fill, minmax(300px, 1fr));
      gap: 20px;
    }

    /* VIDEO CARD */
    .video-card {
      background: var(--card-bg);
      border: 1px solid var(--border);
      display: flex;
      flex-direction: column;
      cursor: pointer;
      position: relative;
      user-select: none;
      transition: border-color 0.25s cubic-bezier(0.16, 1, 0.3, 1), transform 0.25s cubic-bezier(0.16, 1, 0.3, 1), background-color 0.25s;
      overflow: hidden;
    }

    .video-card:hover {
      border-color: var(--border-bright);
      background: var(--card-hover);
      transform: translateY(-2px);
    }

    .thumb-wrapper {
      position: relative;
      width: 100%;
      aspect-ratio: 16 / 9;
      background: #050505;
      overflow: hidden;
      display: flex;
      align-items: center;
      justify-content: center;
      border-bottom: 1px solid var(--border);
    }

    /* Smooth blur-up transition */
    .thumb-img {
      width: 100%;
      height: 100%;
      object-fit: cover;
      opacity: 0;
      filter: blur(12px);
      transform: scale(1.05);
      transition: opacity 0.5s ease, filter 0.5s ease, transform 0.5s ease;
    }

    .thumb-img.loaded {
      opacity: 1;
      filter: blur(0);
      transform: scale(1);
    }

    .thumb-placeholder {
      position: absolute;
      inset: 0;
      display: flex;
      flex-direction: column;
      align-items: center;
      justify-content: center;
      color: #333333;
      font-size: 11px;
      gap: 6px;
      pointer-events: none;
    }

    .play-overlay-icon {
      position: absolute;
      width: 44px;
      height: 44px;
      background: rgba(0, 0, 0, 0.75);
      border: 1px solid rgba(255, 255, 255, 0.3);
      display: flex;
      align-items: center;
      justify-content: center;
      color: #ffffff;
      opacity: 0;
      transform: scale(0.85);
      transition: opacity 0.2s, transform 0.2s, border-color 0.2s;
      pointer-events: none;
    }

    .video-card:hover .play-overlay-icon {
      opacity: 1;
      transform: scale(1);
      border-color: #ffffff;
    }

    /* Progress bar for resumed state */
    .progress-bar {
      position: absolute;
      bottom: 0;
      left: 0;
      height: 2px;
      background: var(--accent);
      width: 0%;
      z-index: 10;
    }

    /* CARD INFO */
    .card-info {
      padding: 12px 14px;
      display: flex;
      flex-direction: column;
      gap: 6px;
    }

    .video-title {
      font-size: 13px;
      font-weight: 500;
      color: var(--text);
      word-break: break-all;
      display: -webkit-box;
      -webkit-line-clamp: 2;
      -webkit-box-orient: vertical;
      overflow: hidden;
      line-height: 1.4;
    }

    .card-meta {
      display: flex;
      align-items: center;
      justify-content: space-between;
      color: var(--text-dim);
      font-size: 11px;
      margin-top: auto;
    }

    .video-path {
      max-width: 65%;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }

    /* EMPTY / LOADING STATES */
    .empty-state {
      text-align: center;
      padding: 80px 20px;
      color: var(--text-dim);
      border: 1px dashed var(--border);
      grid-column: 1 / -1;
    }

    /* MODAL VIDEO PLAYER */
    .player-modal {
      position: fixed;
      inset: 0;
      background: rgba(0, 0, 0, 0.95);
      backdrop-filter: blur(10px);
      z-index: 100;
      display: flex;
      flex-direction: column;
      opacity: 0;
      pointer-events: none;
      transition: opacity 0.25s ease;
    }

    .player-modal.active {
      opacity: 1;
      pointer-events: auto;
    }

    .player-topbar {
      display: flex;
      align-items: center;
      justify-content: space-between;
      padding: 14px 24px;
      border-bottom: 1px solid var(--border);
      background: #000;
    }

    .player-title {
      font-size: 13px;
      font-weight: 600;
      color: var(--text);
      max-width: 70vw;
      white-space: nowrap;
      overflow: hidden;
      text-overflow: ellipsis;
    }

    .player-controls-hint {
      color: var(--text-dim);
      font-size: 11px;
      display: flex;
      gap: 16px;
      align-items: center;
    }

    .close-btn {
      background: none;
      border: 1px solid var(--border);
      color: var(--text);
      font-family: inherit;
      font-size: 11px;
      padding: 4px 10px;
      cursor: pointer;
      transition: border-color 0.2s, background-color 0.2s;
    }

    .close-btn:hover {
      border-color: var(--accent);
      background: #111;
    }

    .video-viewport {
      flex: 1;
      display: flex;
      align-items: center;
      justify-content: center;
      padding: 16px;
      position: relative;
    }

    video#active-player {
      width: 100%;
      height: 100%;
      max-width: 1600px;
      max-height: 85vh;
      outline: none;
      background: #000;
      box-shadow: 0 0 40px rgba(0,0,0,0.8);
    }

    /* FOOTER */
    footer {
      border-top: 1px solid var(--border);
      padding: 14px 24px;
      color: var(--text-dim);
      font-size: 11px;
      display: flex;
      justify-content: space-between;
      align-items: center;
    }

    @media (max-width: 640px) {
      header {
        padding: 14px;
      }
      main {
        padding: 14px;
      }
      .controls {
        max-width: 100%;
      }
      .player-controls-hint span {
        display: none;
      }
    }
  </style>
</head>
<body>

  <header>
    <div class="brand">
      <span class="brand-mark"></span>
      <span class="brand-title">PLAYBOX</span>
      <span class="meta-counts" id="video-count">[ SCANNING... ]</span>
    </div>

    <div class="controls">
      <input type="text" id="search" class="search-input" placeholder="FILTER BY NAME OR PATH (/)..." spellcheck="false" autocomplete="off">
    </div>
  </header>

  <main>
    <div id="video-grid" class="video-grid">
      <div class="empty-state">SCANNING LOCAL FOLDER...</div>
    </div>
  </main>

  <footer>
    <div>PLAYBOX // 100% RUST STD</div>
    <div id="status-hint">READY</div>
  </footer>

  <!-- PLAYER MODAL -->
  <div id="player-modal" class="player-modal" role="dialog" aria-modal="true">
    <div class="player-topbar">
      <div class="player-title" id="player-title">NOW PLAYING</div>
      <div class="player-controls-hint">
        <span>[SPACE: PLAY/PAUSE]</span>
        <span>[ARROWS: SEEK 5s]</span>
        <span>[F: FULLSCREEN]</span>
        <button class="close-btn" id="close-player-btn">[ESC] CLOSE</button>
      </div>
    </div>
    <div class="video-viewport">
      <video id="active-player" controls preload="auto" playsinline></video>
    </div>
  </div>

  <script>
    (function () {
      let allVideos = [];
      const grid = document.getElementById('video-grid');
      const countEl = document.getElementById('video-count');
      const searchInput = document.getElementById('search');
      const playerModal = document.getElementById('player-modal');
      const playerVideo = document.getElementById('active-player');
      const playerTitle = document.getElementById('player-title');
      const closeBtn = document.getElementById('close-player-btn');
      let currentPlayingPath = null;

      // Fetch Video List
      async function loadVideos() {
        try {
          const res = await fetch('/api/videos');
          allVideos = await res.json();
          renderGrid(allVideos);
        } catch (e) {
          grid.innerHTML = '<div class="empty-state">ERROR CONNECTING TO STREAM SERVER.</div>';
        }
      }

      function renderGrid(videos) {
        countEl.textContent = `[ ${videos.length} VIDEOS ]`;
        if (videos.length === 0) {
          grid.innerHTML = '<div class="empty-state">NO VIDEO FILES FOUND IN THIS DIRECTORY.</div>';
          return;
        }

        grid.innerHTML = '';
        const fragment = document.createDocumentFragment();

        videos.forEach(v => {
          const card = document.createElement('div');
          card.className = 'video-card';
          card.dataset.path = v.rel_path;

          const savedTime = parseFloat(localStorage.getItem('playbox_pos_' + v.rel_path) || '0');
          const savedDuration = parseFloat(localStorage.getItem('playbox_dur_' + v.rel_path) || '0');
          const pct = savedDuration > 0 ? Math.min(100, (savedTime / savedDuration) * 100) : 0;

          card.innerHTML = `
            <div class="thumb-wrapper">
              <div class="thumb-placeholder">
                <svg width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5"><polygon points="5 3 19 12 5 21 5 3"></polygon></svg>
                <span>LOADING</span>
              </div>
              <img class="thumb-img" data-src="/api/thumb?path=${encodeURIComponent(v.rel_path)}" alt="${escapeHtml(v.name)}" loading="lazy">
              <div class="play-overlay-icon">
                <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor"><polygon points="6 4 20 12 6 20 6 4"></polygon></svg>
              </div>
              <div class="progress-bar" style="width: ${pct}%"></div>
            </div>
            <div class="card-info">
              <div class="video-title" title="${escapeHtml(v.name)}">${escapeHtml(v.name)}</div>
              <div class="card-meta">
                <span class="video-path" title="${escapeHtml(v.rel_path)}">${escapeHtml(v.rel_path)}</span>
                <span>${escapeHtml(v.size_fmt)}</span>
              </div>
            </div>
          `;

          card.addEventListener('click', () => playVideo(v));
          fragment.appendChild(card);
        });

        grid.appendChild(fragment);
        setupLazyLoading();
      }

      // IntersectionObserver for smooth fade-in and client canvas fallback
      function setupLazyLoading() {
        const images = document.querySelectorAll('.thumb-img[data-src]');
        if (!('IntersectionObserver' in window)) {
          images.forEach(img => loadThumb(img));
          return;
        }

        const observer = new IntersectionObserver((entries, obs) => {
          entries.forEach(entry => {
            if (entry.isIntersecting) {
              const img = entry.target;
              obs.unobserve(img);
              loadThumb(img);
            }
          });
        }, { rootMargin: '100px' });

        images.forEach(img => observer.observe(img));
      }

      function loadThumb(img) {
        const src = img.dataset.src;
        if (!src) return;

        const temp = new Image();
        temp.onload = () => {
          img.src = src;
          img.classList.add('loaded');
          const placeholder = img.previousElementSibling;
          if (placeholder && placeholder.classList.contains('thumb-placeholder')) {
            placeholder.style.display = 'none';
          }
        };

        temp.onerror = () => {
          // Fallback: extract frame client-side using an offscreen video element
          const card = img.closest('.video-card');
          if (card && card.dataset.path) {
            captureClientThumbnail(card.dataset.path, img);
          }
        };

        temp.src = src;
      }

      // Client-side thumbnail extraction using HTML5 canvas
      function captureClientThumbnail(relPath, targetImg) {
        const streamUrl = `/api/stream?path=${encodeURIComponent(relPath)}`;
        const video = document.createElement('video');
        video.crossOrigin = 'anonymous';
        video.muted = true;
        video.preload = 'metadata';
        video.src = streamUrl;

        video.addEventListener('loadeddata', () => {
          video.currentTime = Math.min(2, video.duration / 2 || 1);
        });

        video.addEventListener('seeked', () => {
          try {
            const canvas = document.createElement('canvas');
            canvas.width = 480;
            canvas.height = (video.videoHeight / (video.videoWidth || 1)) * 480 || 270;
            const ctx = canvas.getContext('2d');
            ctx.drawImage(video, 0, 0, canvas.width, canvas.height);
            targetImg.src = canvas.toDataURL('image/jpeg', 0.7);
            targetImg.classList.add('loaded');
            const placeholder = targetImg.previousElementSibling;
            if (placeholder && placeholder.classList.contains('thumb-placeholder')) {
              placeholder.style.display = 'none';
            }
          } catch (e) {}
          video.remove();
        });

        video.addEventListener('error', () => {
          video.remove();
        });
      }

      // Player Control
      function playVideo(videoItem) {
        currentPlayingPath = videoItem.rel_path;
        playerTitle.textContent = videoItem.name;
        playerModal.classList.add('active');
        document.body.style.overflow = 'hidden';

        const streamUrl = `/api/stream?path=${encodeURIComponent(videoItem.rel_path)}`;
        playerVideo.src = streamUrl;

        // Restore saved position
        const savedTime = parseFloat(localStorage.getItem('playbox_pos_' + videoItem.rel_path) || '0');
        if (savedTime > 2) {
          playerVideo.currentTime = savedTime;
        }

        playerVideo.play().catch(() => {});
      }

      function closePlayer() {
        if (!playerModal.classList.contains('active')) return;
        
        if (currentPlayingPath && playerVideo.duration) {
          localStorage.setItem('playbox_pos_' + currentPlayingPath, playerVideo.currentTime);
          localStorage.setItem('playbox_dur_' + currentPlayingPath, playerVideo.duration);

          // Update card progress bar in UI
          const card = document.querySelector(`.video-card[data-path="${CSS.escape(currentPlayingPath)}"]`);
          if (card) {
            const bar = card.querySelector('.progress-bar');
            if (bar) {
              const pct = (playerVideo.currentTime / playerVideo.duration) * 100;
              bar.style.width = pct + '%';
            }
          }
        }

        playerVideo.pause();
        playerVideo.removeAttribute('src');
        playerVideo.load();
        playerModal.classList.remove('active');
        document.body.style.overflow = '';
      }

      closeBtn.addEventListener('click', closePlayer);

      // Periodically record playback position
      playerVideo.addEventListener('timeupdate', () => {
        if (currentPlayingPath && playerVideo.currentTime > 0) {
          localStorage.setItem('playbox_pos_' + currentPlayingPath, playerVideo.currentTime);
          if (playerVideo.duration) {
            localStorage.setItem('playbox_dur_' + currentPlayingPath, playerVideo.duration);
          }
        }
      });

      // Keyboard Controls
      window.addEventListener('keydown', (e) => {
        if (e.key === 'Escape') {
          closePlayer();
          return;
        }

        if (playerModal.classList.contains('active')) {
          if (e.key === ' ' || e.key === 'k') {
            e.preventDefault();
            playerVideo.paused ? playerVideo.play() : playerVideo.pause();
          } else if (e.key === 'ArrowLeft' || e.key === 'j') {
            e.preventDefault();
            playerVideo.currentTime = Math.max(0, playerVideo.currentTime - 5);
          } else if (e.key === 'ArrowRight' || e.key === 'l') {
            e.preventDefault();
            playerVideo.currentTime = Math.min(playerVideo.duration || Infinity, playerVideo.currentTime + 5);
          } else if (e.key === 'f' || e.key === 'F') {
            e.preventDefault();
            if (!document.fullscreenElement) {
              playerVideo.requestFullscreen().catch(() => {});
            } else {
              document.exitFullscreen().catch(() => {});
            }
          } else if (e.key === 'm' || e.key === 'M') {
            e.preventDefault();
            playerVideo.muted = !playerVideo.muted;
          }
        } else if (e.key === '/' && document.activeElement !== searchInput) {
          e.preventDefault();
          searchInput.focus();
        }
      });

      // Search / Filter
      searchInput.addEventListener('input', () => {
        const query = searchInput.value.toLowerCase().trim();
        if (!query) {
          renderGrid(allVideos);
          return;
        }
        const filtered = allVideos.filter(v =>
          v.name.toLowerCase().includes(query) || v.rel_path.toLowerCase().includes(query)
        );
        renderGrid(filtered);
      });

      function escapeHtml(str) {
        return (str || '').replace(/[&<>"']/g, function (m) {
          return ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[m];
        });
      }

      loadVideos();
    })();
  </script>
</body>
</html>
"#;
